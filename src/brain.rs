use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

use crate::AppState;
use crate::agents::QualityGates;
use crate::config::Config;
use crate::knowledge::KnowledgeStore;
use crate::recon::{self, ReconReport};
use crate::tasks::{self, LaunchTaskRequest, ProjectContext};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LlmMessage {
    pub role: String,
    pub content: serde_json::Value, // string or array of content blocks
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String, // JSON string
}

/// Events emitted during an agent loop turn, streamed to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum BrainEvent {
    /// Brain is thinking / calling LLM
    #[serde(rename = "thinking")]
    Thinking,
    /// Brain is executing a tool
    #[serde(rename = "tool_start")]
    ToolStart { tool: String, args: serde_json::Value },
    /// Tool finished
    #[serde(rename = "tool_end")]
    ToolEnd { tool: String, result: String },
    /// Final text reply from the brain
    #[serde(rename = "reply")]
    Reply { text: String },
    /// Loop finished
    #[serde(rename = "done")]
    Done,
    /// Error during loop
    #[serde(rename = "error")]
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Tool definitions (JSON schemas for OpenAI function calling)
// ---------------------------------------------------------------------------

fn tool_definitions() -> serde_json::Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "dispatch_task",
                "description": "Dispatch a coding agent to work on a task. Use when the user asks you to fix, build, or implement something.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Short description of the task"
                        },
                        "issue_number": {
                            "type": "integer",
                            "description": "GitHub issue number, if applicable"
                        },
                        "agent_type": {
                            "type": "string",
                            "enum": ["codex", "claude"],
                            "description": "Which coding agent to use. Defaults to codex."
                        }
                    },
                    "required": ["title"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_recon",
                "description": "Run reconnaissance on a GitHub issue or repo. Gathers issue details, related PRs, file tree, test baselines, and key files. Use before dispatching complex tasks.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "issue_number": {
                            "type": "integer",
                            "description": "GitHub issue number to investigate"
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "check_status",
                "description": "Check the status of recent tasks. Shows running, done, and failed tasks.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Max tasks to return. Defaults to 8."
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_knowledge",
                "description": "Query saved project knowledge and learnings from past tasks.",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_projects",
                "description": "List all configured projects.",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "kill_task",
                "description": "Kill a running task by its ID.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task ID to kill"
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Agent loop — the core pi-style loop
// ---------------------------------------------------------------------------

const MAX_TURNS: usize = 10;

/// Run the brain agent loop. Returns the final text reply and all events.
///
/// - `events_tx`: optional channel to stream BrainEvents live (for WebSocket).
/// - `steering_rx`: optional channel to receive steering messages mid-loop.
///   After each tool execution, the loop drains this channel. If a steering
///   message arrives, remaining tool calls are skipped and the message is
///   injected into context before the next LLM call (pi-style steering).
pub async fn agent_loop(
    state: &Arc<AppState>,
    project: Option<&ProjectContext>,
    history: &[LlmMessage],
    user_message: &str,
    events_tx: Option<mpsc::UnboundedSender<BrainEvent>>,
    mut steering_rx: Option<&mut mpsc::UnboundedReceiver<String>>,
) -> Result<(String, Vec<BrainEvent>)> {
    let emit = |event: BrainEvent, collected: &mut Vec<BrainEvent>, tx: &Option<mpsc::UnboundedSender<BrainEvent>>| {
        collected.push(event.clone());
        if let Some(tx) = tx {
            let _ = tx.send(event);
        }
    };

    let mut all_events = Vec::new();
    let config = Config::from_env();
    let model = &config.llm_model;

    // Build message list: system + history + new user message
    let system = build_system_prompt(state, project);
    let mut messages: Vec<LlmMessage> = Vec::with_capacity(history.len() + 2);
    messages.push(LlmMessage {
        role: "system".to_string(),
        content: json!(system),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });
    messages.extend_from_slice(history);
    messages.push(LlmMessage {
        role: "user".to_string(),
        content: json!(user_message),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });

    // Tool-calling loop
    for turn in 0..MAX_TURNS {
        emit(BrainEvent::Thinking, &mut all_events, &events_tx);

        let response = call_llm_with_tools(model, &messages).await?;

        // Check for tool calls
        let tool_calls = response
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("tool_calls"))
            .and_then(|tc| tc.as_array())
            .cloned()
            .unwrap_or_default();

        let assistant_content = response
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .cloned()
            .unwrap_or(json!(null));

        // Append assistant message (with tool_calls if any)
        let parsed_tool_calls: Vec<ToolCall> = tool_calls
            .iter()
            .filter_map(|tc| serde_json::from_value(tc.clone()).ok())
            .collect();

        messages.push(LlmMessage {
            role: "assistant".to_string(),
            content: assistant_content.clone(),
            tool_calls: if parsed_tool_calls.is_empty() {
                None
            } else {
                Some(parsed_tool_calls.clone())
            },
            tool_call_id: None,
            name: None,
        });

        if parsed_tool_calls.is_empty() {
            // No tool calls — we have the final reply
            let reply_text = assistant_content
                .as_str()
                .unwrap_or("")
                .to_string();
            emit(BrainEvent::Reply { text: reply_text.clone() }, &mut all_events, &events_tx);
            emit(BrainEvent::Done, &mut all_events, &events_tx);
            return Ok((reply_text, all_events));
        }

        // Execute tool calls, checking for steering after each one
        let mut steered = false;
        for (idx, tc) in parsed_tool_calls.iter().enumerate() {
            let args: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));

            emit(
                BrainEvent::ToolStart {
                    tool: tc.function.name.clone(),
                    args: args.clone(),
                },
                &mut all_events,
                &events_tx,
            );

            let result = execute_tool(state, project, &tc.function.name, &args).await;

            let result_str = match &result {
                Ok(s) => s.clone(),
                Err(e) => format!("Error: {e}"),
            };

            emit(
                BrainEvent::ToolEnd {
                    tool: tc.function.name.clone(),
                    result: truncate(&result_str, 2000),
                },
                &mut all_events,
                &events_tx,
            );

            messages.push(LlmMessage {
                role: "tool".to_string(),
                content: json!(result_str),
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
                name: Some(tc.function.name.clone()),
            });

            // --- Steering check (pi-style) ---
            // After each tool execution, drain the steering channel.
            // If a message arrived, skip remaining tools and inject it.
            if let Some(rx) = steering_rx.as_deref_mut() {
                if let Ok(steer_msg) = rx.try_recv() {
                    // Skip remaining tool calls with placeholder results
                    for skipped in &parsed_tool_calls[idx + 1..] {
                        messages.push(LlmMessage {
                            role: "tool".to_string(),
                            content: json!("Skipped — user sent a new message."),
                            tool_calls: None,
                            tool_call_id: Some(skipped.id.clone()),
                            name: Some(skipped.function.name.clone()),
                        });
                    }

                    // Inject steering message
                    emit(
                        BrainEvent::ToolStart {
                            tool: "steering".to_string(),
                            args: json!({ "message": steer_msg }),
                        },
                        &mut all_events,
                        &events_tx,
                    );
                    messages.push(LlmMessage {
                        role: "user".to_string(),
                        content: json!(steer_msg),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                    steered = true;
                    break;
                }
            }
        }

        // If we were steered, continue the loop (LLM will see the new user message)
        if steered {
            continue;
        }

        if turn == MAX_TURNS - 1 {
            warn!("Brain agent loop hit max turns ({MAX_TURNS})");
        }
    }

    let fallback = "I ran out of thinking turns. Here's what I know so far.".to_string();
    emit(BrainEvent::Reply { text: fallback.clone() }, &mut all_events, &events_tx);
    emit(BrainEvent::Done, &mut all_events, &events_tx);
    Ok((fallback, all_events))
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

async fn execute_tool(
    state: &Arc<AppState>,
    project: Option<&ProjectContext>,
    name: &str,
    args: &serde_json::Value,
) -> Result<String> {
    match name {
        "dispatch_task" => tool_dispatch_task(state, project, args).await,
        "run_recon" => tool_run_recon(state, project, args).await,
        "check_status" => tool_check_status(state, project, args),
        "query_knowledge" => tool_query_knowledge(project),
        "list_projects" => tool_list_projects(state),
        "kill_task" => tool_kill_task(state, args).await,
        _ => Err(anyhow!("Unknown tool: {name}")),
    }
}

async fn tool_dispatch_task(
    state: &Arc<AppState>,
    project: Option<&ProjectContext>,
    args: &serde_json::Value,
) -> Result<String> {
    let project = project.ok_or_else(|| anyhow!("No project selected. Ask the user to select one first."))?;
    let title = args["title"]
        .as_str()
        .ok_or_else(|| anyhow!("title is required"))?;
    let issue_number = args["issue_number"].as_i64();
    let agent_type = args["agent_type"].as_str().unwrap_or("codex").to_string();

    let task = tasks::launch_task(
        state.clone(),
        LaunchTaskRequest {
            project_id: project.id.clone(),
            issue_number,
            title: title.to_string(),
            model: None,
            agent_type: Some(agent_type.clone()),
            quality_gates: Some(QualityGates {
                tests: true,
                clippy: true,
                review: true,
                auto_merge: true,
            }),
            extra_instructions: None,
            auto_merge: true,
        },
    )
    .await?;

    tasks::add_event(
        state,
        &task.id,
        "brain",
        "brain",
        &format!("Chat dispatch: {title}"),
        None,
    );

    Ok(format!(
        "Task dispatched successfully.\n- ID: {}\n- Title: {}\n- Agent: {}\n- Status: running",
        task.id, task.title, agent_type
    ))
}

async fn tool_run_recon(
    _state: &Arc<AppState>,
    project: Option<&ProjectContext>,
    args: &serde_json::Value,
) -> Result<String> {
    let project = project.ok_or_else(|| anyhow!("No project selected."))?;
    let issue_number = args["issue_number"].as_i64();
    let repo_path = tasks::repo_checkout_path(&project.owner, &project.repo);

    let report = recon::run_recon(&project.owner, &project.repo, issue_number, &repo_path).await;
    let report_json = serde_json::to_string_pretty(&report)
        .unwrap_or_else(|_| "(failed to serialize recon)".to_string());

    Ok(truncate(&report_json, 6000))
}

fn tool_check_status(
    state: &Arc<AppState>,
    project: Option<&ProjectContext>,
    args: &serde_json::Value,
) -> Result<String> {
    let limit = args["limit"].as_u64().unwrap_or(8) as usize;
    let project_id = project.map(|p| p.id.as_str());

    let conn = state.db.conn();
    let sql = if project_id.is_some() {
        "SELECT t.id, p.repo, t.issue_number, t.title, t.status, t.created_at
         FROM tasks t
         LEFT JOIN projects p ON p.id = t.project_id
         WHERE t.project_id = ?1
         ORDER BY t.created_at DESC
         LIMIT ?2"
    } else {
        "SELECT t.id, p.repo, t.issue_number, t.title, t.status, t.created_at
         FROM tasks t
         LEFT JOIN projects p ON p.id = t.project_id
         ORDER BY t.created_at DESC
         LIMIT ?1"
    };

    let mut stmt = conn.prepare(sql).context("failed to query tasks")?;

    let rows: Vec<String> = if let Some(pid) = project_id {
        stmt.query_map(rusqlite::params![pid, limit as i64], |row| {
            Ok(format_task_row(row))
        })
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(format_task_row(row))
        })
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect()
    };

    if rows.is_empty() {
        Ok("No tasks found.".to_string())
    } else {
        Ok(rows.join("\n"))
    }
}

fn format_task_row(row: &rusqlite::Row) -> String {
    let id: String = row.get(0).unwrap_or_default();
    let repo: Option<String> = row.get(1).unwrap_or_default();
    let issue: Option<i64> = row.get(2).unwrap_or_default();
    let title: String = row.get(3).unwrap_or_default();
    let status: String = row.get(4).unwrap_or_default();
    let created: String = row.get(5).unwrap_or_default();

    let issue_str = issue.map(|n| format!(" #{n}")).unwrap_or_default();
    let repo_str = repo.unwrap_or_default();
    format!("- [{status}] {repo_str}{issue_str}: {title} (id: {}, created: {created})", &id[..8.min(id.len())])
}

fn tool_query_knowledge(project: Option<&ProjectContext>) -> Result<String> {
    let project = project.ok_or_else(|| anyhow!("No project selected."))?;
    let store = KnowledgeStore::new();
    let knowledge = store.load_knowledge(&project.owner, &project.repo);

    if knowledge.trim().is_empty() {
        Ok("No saved knowledge for this project yet.".to_string())
    } else {
        Ok(truncate(&knowledge, 4000))
    }
}

fn tool_list_projects(state: &Arc<AppState>) -> Result<String> {
    let conn = state.db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, owner, repo, default_branch FROM projects ORDER BY created_at ASC",
    )?;

    let rows: Vec<String> = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let owner: String = row.get(1)?;
            let repo: String = row.get(2)?;
            let branch: String = row.get(3)?;
            Ok(format!("- {owner}/{repo} (branch: {branch}, id: {})", &id[..8.min(id.len())]))
        })
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        Ok("No projects configured.".to_string())
    } else {
        Ok(rows.join("\n"))
    }
}

async fn tool_kill_task(state: &Arc<AppState>, args: &serde_json::Value) -> Result<String> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow!("task_id is required"))?;

    // Look up the task to get its pid
    let conn = state.db.conn();
    let (status, pid): (String, Option<i64>) = conn
        .query_row(
            "SELECT status, pid FROM tasks WHERE id = ?1 OR id LIKE ?2",
            rusqlite::params![task_id, format!("{task_id}%")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("Task not found")?;

    if status != "running" {
        return Ok(format!("Task is not running (status: {status})"));
    }

    if let Some(pid) = pid {
        let pid = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
    }

    conn.execute(
        "UPDATE tasks SET status = 'killed' WHERE id = ?1 OR id LIKE ?2",
        rusqlite::params![task_id, format!("{task_id}%")],
    )?;

    Ok(format!("Task {task_id} killed."))
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn build_system_prompt(state: &Arc<AppState>, project: Option<&ProjectContext>) -> String {
    let projects = list_project_names(state);
    let project_list = if projects.is_empty() {
        "No projects configured yet.".to_string()
    } else {
        projects
            .iter()
            .map(|(owner, repo, id)| {
                let marker = if project.map(|p| p.id.as_str()) == Some(id.as_str()) {
                    " (selected)"
                } else {
                    ""
                };
                format!("- {owner}/{repo}{marker}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "You are Shipyard, an AI engineering manager. You orchestrate coding agents to fix GitHub issues and build features.\n\n\
        You have tools to take action. Use them — don't describe what you would do, just do it.\n\n\
        Projects:\n{project_list}\n\n\
        Guidelines:\n\
        - Be concise and direct. You're an engineering lead, not a chatbot.\n\
        - When asked to do something, use your tools immediately.\n\
        - For complex tasks, run recon first, then dispatch.\n\
        - If no project is selected, ask the user to pick one.\n\
        - Show brief summaries, not walls of text."
    )
}

fn list_project_names(state: &Arc<AppState>) -> Vec<(String, String, String)> {
    let conn = state.db.conn();
    let mut stmt = match conn.prepare("SELECT id, owner, repo FROM projects ORDER BY created_at ASC") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect()
}

// ---------------------------------------------------------------------------
// LLM call with tools
// ---------------------------------------------------------------------------

async fn call_llm_with_tools(
    model: &str,
    messages: &[LlmMessage],
) -> Result<serde_json::Value> {
    let config = Config::from_env();
    let endpoint = config.llm_endpoint.clone();
    let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
    let model = if model.trim().is_empty() {
        &config.llm_model
    } else {
        model
    };

    let body = json!({
        "model": model,
        "messages": messages,
        "tools": tool_definitions(),
    });

    let client = reqwest::Client::new();
    let mut request = client.post(&url).header("Content-Type", "application/json");

    if !config.api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {}", config.api_key));
    }

    let response = request
        .json(&body)
        .send()
        .await
        .with_context(|| format!("failed to call LLM endpoint {url}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(anyhow!("LLM request failed ({status}): {text}"));
    }

    serde_json::from_str(&text).context("failed to parse LLM response")
}

// ---------------------------------------------------------------------------
// Legacy single-shot helpers (kept for plan_task / review_diff / extract_learnings)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TaskPlan {
    pub assessment: String,
    pub complexity: u8,
    pub prompt: String,
    pub skip_reason: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReviewResult {
    pub approved: bool,
    pub summary: String,
    pub issues: Vec<String>,
    pub suggestion: Option<String>,
}

pub async fn plan_task(recon: &ReconReport, knowledge: &str, model: &str) -> Result<TaskPlan> {
    let recon_json =
        serde_json::to_string_pretty(recon).context("failed to serialize recon report")?;

    let system_prompt = format!(
        r#"You are Shipyard Brain V2, an AI engineering manager that plans work for coding agents.

You will receive:
- A full recon report gathered before planning
- Project knowledge and recent task history

Your job:
1. Decide whether the task should be skipped because it is already fixed, already in flight, or lacks enough context.
2. Write one concrete execution prompt for a coding agent.
3. The prompt must reference specific file paths from recon, exact test commands, and likely gotchas.
4. Use the recon report directly. Do not ask the coding agent to repeat recon unless information is missing.
5. If recon suggests the issue may already be fixed, set `skip_reason`.
6. Choose a realistic timeout in seconds.

Planning input:
## Recon Report
{recon_json}

## Project Knowledge And Recent Task History
{knowledge}

Return JSON only with this schema:
{{
  "assessment": "short task assessment",
  "complexity": 1,
  "prompt": "specific execution prompt for one coding agent",
  "skip_reason": null,
  "timeout_secs": 3600
}}

Rules for `prompt`:
- Name the files or directories to inspect first.
- Include the exact verification commands.
- Mention branch/PR context if relevant.
- Mention concrete gotchas from knowledge or recon.
- Keep it actionable and repo-specific.
"#
    );

    let response = call_llm_json(model, &system_prompt, "Plan the task.").await?;
    let mut plan: TaskPlan = parse_json_response(&response)?;
    plan.complexity = plan.complexity.clamp(1, 5);
    if plan.timeout_secs == 0 {
        plan.timeout_secs = 3600;
    }
    Ok(plan)
}

pub async fn review_diff(
    diff: &str,
    recon: &ReconReport,
    knowledge: &str,
    model: &str,
) -> Result<ReviewResult> {
    let recon_json =
        serde_json::to_string_pretty(recon).context("failed to serialize recon report")?;

    let system_prompt = format!(
        r#"You are Shipyard Brain V2 acting as a reviewer.

Review the diff against:
- The recon report
- Project knowledge and recent task history

Check:
1. Does the diff address the issue described in recon?
2. Does it introduce regressions or miss obvious edge cases?
3. Are tests or validation steps missing?
4. Does it violate project-specific gotchas or patterns?

## Recon Report
{recon_json}

## Project Knowledge And Recent Task History
{knowledge}

Return JSON only:
{{
  "approved": true,
  "summary": "short summary",
  "issues": ["issue"],
  "suggestion": "optional next step"
}}
"#
    );

    let user_prompt = format!("Review this diff:\n```diff\n{diff}\n```");
    let response = call_llm_json(model, &system_prompt, &user_prompt).await?;
    parse_json_response(&response)
}

pub async fn extract_learnings(
    task_id: &str,
    outcome: &str,
    diff: &str,
    model: &str,
) -> Result<String> {
    let system_prompt = r#"You extract durable project learnings for future coding agents.

Return concise markdown only. Focus on:
- validated repo-specific patterns
- failure modes and how to avoid them
- useful test or verification commands

If there is nothing durable to save, return an empty string."#;

    let user_prompt =
        format!("Task ID: {task_id}\nOutcome: {outcome}\n\nDiff:\n```diff\n{diff}\n```");

    let response = call_llm_simple(model, system_prompt, &user_prompt, None).await?;
    Ok(response.trim().to_string())
}

pub async fn call_llm_pub(model: &str, system: &str, user: &str) -> Result<String> {
    call_llm_simple(model, system, user, None).await
}

async fn call_llm_json(model: &str, system: &str, user: &str) -> Result<String> {
    call_llm_simple(model, system, user, Some(json!({ "type": "json_object" }))).await
}

async fn call_llm_simple(
    model: &str,
    system: &str,
    user: &str,
    response_format: Option<serde_json::Value>,
) -> Result<String> {
    let messages = vec![
        LlmMessage {
            role: "system".to_string(),
            content: json!(system),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
        LlmMessage {
            role: "user".to_string(),
            content: json!(user),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
    ];
    call_llm_messages_simple(model, &messages, response_format).await
}

async fn call_llm_messages_simple(
    model: &str,
    messages: &[LlmMessage],
    response_format: Option<serde_json::Value>,
) -> Result<String> {
    let config = Config::from_env();
    let endpoint = config.llm_endpoint.clone();
    let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
    let model = if model.trim().is_empty() {
        &config.llm_model
    } else {
        model
    };
    let api_key = &config.api_key;

    let mut body = json!({
        "model": model,
        "messages": messages
    });

    if let Some(format) = response_format {
        body["response_format"] = format;
    }

    let client = reqwest::Client::new();
    let mut request = client.post(&url).header("Content-Type", "application/json");

    if !api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }

    let response = request
        .json(&body)
        .send()
        .await
        .with_context(|| format!("failed to call LLM endpoint {url}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(anyhow!("LLM request failed with status {status}: {text}"));
    }

    let json: serde_json::Value =
        serde_json::from_str(&text).context("failed to parse LLM response body")?;

    extract_message_content(&json).ok_or_else(|| {
        warn!(response = %text, "LLM response missing message content");
        anyhow!("LLM response did not contain message content")
    })
}

fn parse_json_response<T>(response: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let trimmed = response.trim();
    let without_prefix = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let cleaned = without_prefix
        .strip_suffix("```")
        .unwrap_or(without_prefix)
        .trim();

    let candidate = if let Some(start) = cleaned.find('{') {
        if let Some(end) = cleaned.rfind('}') {
            &cleaned[start..=end]
        } else {
            cleaned
        }
    } else {
        cleaned
    };

    serde_json::from_str(candidate).context("failed to parse structured LLM JSON response")
}

fn extract_message_content(value: &serde_json::Value) -> Option<String> {
    let content = &value["choices"][0]["message"]["content"];
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }

    content.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...(truncated)", &s[..max])
    }
}
