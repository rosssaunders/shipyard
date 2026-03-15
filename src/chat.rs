use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

use crate::{
    AppState, brain,
    tasks,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub actions: Option<Vec<ChatAction>>,
    pub timestamp: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChatAction {
    pub kind: String,
    pub summary: String,
    pub detail: Option<String>,
}

#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
    pub project_id: Option<String>,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub reply: String,
    pub actions: Vec<ChatAction>,
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub project_id: Option<String>,
    pub limit: Option<usize>,
}

pub async fn get_history(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HistoryQuery>,
) -> Json<Vec<ChatMessage>> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    Json(load_chat_history(
        &state,
        query.project_id.as_deref(),
        limit,
    ))
}

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Json<ChatResponse> {
    let message = req.message.trim();
    if message.is_empty() {
        return Json(ChatResponse {
            reply: "Say what you need.".to_string(),
            actions: Vec::new(),
        });
    }

    let selected_project = tasks::resolve_project(&state, req.project_id.as_deref());
    let project_id = selected_project.as_ref().map(|p| p.id.clone());

    // Store user message
    store_message(&state, project_id.as_deref(), "user", message, None);

    // Load history and convert to LlmMessage format for the agent loop
    let history = load_chat_history(&state, project_id.as_deref(), 20);
    let llm_history = history_to_llm_messages(&history);

    // Run the agent loop (pi-style tool-calling loop)
    // HTTP path: no streaming, no steering (use WebSocket for those)
    let (reply, events) = match brain::agent_loop(
        &state,
        selected_project.as_ref(),
        &llm_history,
        message,
        None,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            let fallback = format!("LLM error: {err}");
            store_message(&state, project_id.as_deref(), "assistant", &fallback, None);
            return Json(ChatResponse {
                reply: fallback,
                actions: Vec::new(),
            });
        }
    };

    // Convert brain events into ChatActions for the frontend
    let actions = events_to_actions(&events);

    let assistant_reply = if reply.trim().is_empty() {
        if actions.is_empty() {
            "Done.".to_string()
        } else {
            actions
                .iter()
                .map(|a| a.summary.clone())
                .collect::<Vec<_>>()
                .join("\n")
        }
    } else {
        reply
    };

    store_message(
        &state,
        project_id.as_deref(),
        "assistant",
        &assistant_reply,
        Some(&actions),
    );

    Json(ChatResponse {
        reply: assistant_reply,
        actions,
    })
}

/// Convert BrainEvents into ChatActions for the response
fn events_to_actions(events: &[brain::BrainEvent]) -> Vec<ChatAction> {
    let mut actions = Vec::new();

    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            brain::BrainEvent::ToolStart { tool, args } => {
                // Find the matching ToolEnd
                let result = events[i + 1..]
                    .iter()
                    .find_map(|e| match e {
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
            _ => {}
        }
        i += 1;
    }

    actions
}

/// Convert chat history into LlmMessages for the brain context window.
/// Only includes user/assistant pairs — tool calls are internal to each loop run.
fn history_to_llm_messages(history: &[ChatMessage]) -> Vec<brain::LlmMessage> {
    history
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| {
            let mut content = m.content.clone();
            // Append action summaries so the brain has context about what it did
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

// ---------------------------------------------------------------------------
// Persistence (unchanged)
// ---------------------------------------------------------------------------

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
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    if let Some(project_id) = project_id {
        stmt.query_map(rusqlite::params![project_id, limit as i64], |row| {
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
