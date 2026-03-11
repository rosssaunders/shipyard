use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::AppState;
use crate::brain;
use crate::chat::{ChatAction, ChatMessage};
use crate::tasks;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

/// Per-connection state for steering
struct ConnState {
    /// Sender for steering messages into the running brain loop
    steering_tx: Option<mpsc::UnboundedSender<String>>,
}

#[derive(Deserialize)]
struct WsCommand {
    #[serde(rename = "type")]
    cmd_type: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send welcome
    let welcome = json!({ "type": "connected", "message": "Shipyard" });
    let _ = ws_tx.send(Message::Text(welcome.to_string())).await;

    let mut conn = ConnState { steering_tx: None };

    // Channel for the brain loop to send events through
    // (created per-chat request, forwarded to ws_tx)
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<OutboundMsg>();

    // Spawn a task that forwards internal messages to the WebSocket sink
    let fwd_handle = tokio::spawn(async move {
        while let Some(msg) = internal_rx.recv().await {
            let text = match msg {
                OutboundMsg::Json(v) => v.to_string(),
            };
            if ws_tx.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    // Read loop
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => {
                let text_str: &str = &text;
                let Ok(cmd) = serde_json::from_str::<WsCommand>(text_str) else {
                    let _ = internal_tx.send(OutboundMsg::Json(
                        json!({ "type": "error", "message": "Invalid JSON" }),
                    ));
                    continue;
                };

                match cmd.cmd_type.as_str() {
                    "ping" => {
                        let _ = internal_tx.send(OutboundMsg::Json(json!({ "type": "pong" })));
                    }

                    "chat" => {
                        let message = cmd.message.unwrap_or_default();
                        if message.trim().is_empty() {
                            let _ = internal_tx.send(OutboundMsg::Json(
                                json!({ "type": "error", "message": "Empty message" }),
                            ));
                            continue;
                        }

                        // Create steering channel for this run
                        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
                        conn.steering_tx = Some(steer_tx);

                        // Spawn the brain loop
                        let loop_state = state.clone();
                        let loop_tx = internal_tx.clone();
                        let project_id = cmd.project_id.clone();
                        let msg_text = message.clone();

                        tokio::spawn(async move {
                            run_chat_loop(
                                loop_state,
                                project_id,
                                msg_text,
                                loop_tx,
                                steer_rx,
                            )
                            .await;
                        });
                    }

                    "steer" => {
                        // Inject a steering message into the running brain loop
                        let steer_msg = cmd.message.unwrap_or_default();
                        if steer_msg.trim().is_empty() {
                            continue;
                        }
                        if let Some(tx) = &conn.steering_tx {
                            let _ = tx.send(steer_msg);
                        } else {
                            // No active loop — treat as a new chat
                            let _ = internal_tx.send(OutboundMsg::Json(
                                json!({ "type": "error", "message": "No active brain loop to steer. Send a 'chat' message first." }),
                            ));
                        }
                    }

                    _ => {
                        let _ = internal_tx.send(OutboundMsg::Json(
                            json!({ "type": "error", "message": format!("Unknown command: {}", cmd.cmd_type) }),
                        ));
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Clean up
    drop(internal_tx);
    let _ = fwd_handle.await;
}

enum OutboundMsg {
    Json(serde_json::Value),
}

/// Run the brain agent loop for a WebSocket chat request.
/// Streams BrainEvents back and persists the final result.
async fn run_chat_loop(
    state: Arc<AppState>,
    project_id: Option<String>,
    user_message: String,
    ws_tx: mpsc::UnboundedSender<OutboundMsg>,
    mut steering_rx: mpsc::UnboundedReceiver<String>,
) {
    let selected_project = tasks::resolve_project(&state, project_id.as_deref());
    let pid = selected_project.as_ref().map(|p| p.id.clone());

    // Store user message
    store_message(&state, pid.as_deref(), "user", &user_message, None);

    // Load history
    let history = load_chat_history(&state, pid.as_deref(), 20);
    let llm_history = history_to_llm_messages(&history);

    // Create event channel for streaming
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<brain::BrainEvent>();

    // Spawn the loop itself so we can forward events concurrently
    let loop_state = state.clone();
    let loop_project = selected_project.clone();
    let loop_msg = user_message.clone();
    let loop_handle = tokio::spawn(async move {
        brain::agent_loop(
            &loop_state,
            loop_project.as_ref(),
            &llm_history,
            &loop_msg,
            Some(events_tx),
            Some(&mut steering_rx),
        )
        .await
    });

    // Forward brain events to WebSocket as they arrive
    let mut all_events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        all_events.push(event.clone());

        let payload = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let _ = ws_tx.send(OutboundMsg::Json(payload));
    }

    // Wait for the loop to finish and get the reply
    let (reply, _) = match loop_handle.await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
            let error_msg = format!("Brain error: {err}");
            let _ = ws_tx.send(OutboundMsg::Json(
                json!({ "type": "error", "message": error_msg }),
            ));
            (error_msg, Vec::new())
        }
        Err(err) => {
            let error_msg = format!("Brain task panicked: {err}");
            let _ = ws_tx.send(OutboundMsg::Json(
                json!({ "type": "error", "message": error_msg }),
            ));
            (error_msg, Vec::new())
        }
    };

    // Convert events to actions and persist
    let actions = events_to_actions(&all_events);
    let assistant_reply = if reply.trim().is_empty() {
        if actions.is_empty() {
            "Done.".to_string()
        } else {
            actions.iter().map(|a| a.summary.clone()).collect::<Vec<_>>().join("\n")
        }
    } else {
        reply
    };

    store_message(&state, pid.as_deref(), "assistant", &assistant_reply, Some(&actions));

    // Send final complete message
    let _ = ws_tx.send(OutboundMsg::Json(json!({
        "type": "chat_complete",
        "reply": assistant_reply,
        "actions": actions,
    })));
}

// ---------------------------------------------------------------------------
// Helpers shared with chat.rs (duplicated to avoid circular deps)
// ---------------------------------------------------------------------------

fn events_to_actions(events: &[brain::BrainEvent]) -> Vec<ChatAction> {
    let mut actions = Vec::new();
    for (i, event) in events.iter().enumerate() {
        if let brain::BrainEvent::ToolStart { tool, args } = event {
            if tool == "steering" {
                continue;
            }
            let result = events[i + 1..].iter().find_map(|e| match e {
                brain::BrainEvent::ToolEnd { tool: t, result } if t == tool => {
                    Some(result.clone())
                }
                _ => None,
            });

            let summary = match tool.as_str() {
                "dispatch_task" => {
                    let title = args["title"].as_str().unwrap_or("task");
                    format!("Dispatched: {title}")
                }
                "run_recon" => {
                    let issue = args["issue_number"].as_i64();
                    match issue {
                        Some(n) => format!("Recon: #{n}"),
                        None => "Recon: repo".to_string(),
                    }
                }
                "check_status" => "Checked status".to_string(),
                "query_knowledge" => "Queried knowledge".to_string(),
                "list_projects" => "Listed projects".to_string(),
                "kill_task" => {
                    let id = args["task_id"].as_str().unwrap_or("?");
                    format!("Killed task: {id}")
                }
                other => format!("Tool: {other}"),
            };

            actions.push(ChatAction {
                kind: tool.clone(),
                summary,
                detail: result.map(|r| truncate_detail(&r, 1200)),
            });
        }
    }
    actions
}

fn history_to_llm_messages(history: &[ChatMessage]) -> Vec<brain::LlmMessage> {
    history
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| {
            let mut content = m.content.clone();
            if let Some(actions) = &m.actions {
                if !actions.is_empty() {
                    let lines: Vec<String> = actions
                        .iter()
                        .map(|a| {
                            if let Some(d) = &a.detail {
                                format!("- {}: {} ({})", a.kind, a.summary, truncate_detail(d, 200))
                            } else {
                                format!("- {}: {}", a.kind, a.summary)
                            }
                        })
                        .collect();
                    content.push_str("\n\n[Actions taken]\n");
                    content.push_str(&lines.join("\n"));
                }
            }
            brain::LlmMessage {
                role: m.role.clone(),
                content: serde_json::json!(content),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }
        })
        .collect()
}

fn load_chat_history(state: &AppState, project_id: Option<&str>, limit: usize) -> Vec<ChatMessage> {
    let conn = state.db.conn();
    let sql = if project_id.is_some() {
        "SELECT role, content, actions, created_at
         FROM (
           SELECT id, role, content, actions, created_at
           FROM chat_messages
           WHERE project_id = ?1
           ORDER BY id DESC
           LIMIT ?2
         )
         ORDER BY id ASC"
    } else {
        "SELECT role, content, actions, created_at
         FROM (
           SELECT id, role, content, actions, created_at
           FROM chat_messages
           ORDER BY id DESC
           LIMIT ?1
         )
         ORDER BY id ASC"
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    if let Some(pid) = project_id {
        stmt.query_map(rusqlite::params![pid, limit as i64], |row| {
            Ok(ChatMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                actions: row
                    .get::<_, Option<String>>(2)?
                    .and_then(|v| serde_json::from_str(&v).ok()),
                timestamp: row.get(3)?,
            })
        })
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(ChatMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                actions: row
                    .get::<_, Option<String>>(2)?
                    .and_then(|v| serde_json::from_str(&v).ok()),
                timestamp: row.get(3)?,
            })
        })
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect()
    }
}

fn store_message(
    state: &AppState,
    project_id: Option<&str>,
    role: &str,
    content: &str,
    actions: Option<&[ChatAction]>,
) {
    let actions_json = actions.and_then(|v| serde_json::to_string(v).ok());
    let timestamp = chrono::Utc::now().to_rfc3339();
    let _ = state.db.conn().execute(
        "INSERT INTO chat_messages (project_id, role, content, actions, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![project_id, role, content, actions_json, timestamp],
    );
}

fn truncate_detail(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_len).collect();
        format!("{}...", truncated.trim_end())
    }
}
