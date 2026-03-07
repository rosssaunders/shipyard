use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

use crate::{
    AppState, brain,
    knowledge::KnowledgeStore,
    recon,
    tasks::{self, LaunchTaskRequest, ProjectContext},
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

#[derive(Clone)]
struct ProjectSummary {
    id: String,
    owner: String,
    repo: String,
    skills: String,
}

#[derive(Clone)]
struct TaskStatusSnapshot {
    repo: String,
    issue_number: Option<i64>,
    title: String,
    status: String,
    created_at: String,
}

enum PendingAction {
    Dispatch {
        description: String,
    },
    Recon {
        owner: String,
        repo: String,
        issue_number: Option<i64>,
    },
    Status,
    Knowledge {
        owner: String,
        repo: String,
    },
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
    let project_id = selected_project.as_ref().map(|project| project.id.clone());
    let history = load_chat_history(&state, project_id.as_deref(), 20);

    store_message(&state, project_id.as_deref(), "user", message, None);

    let system_prompt = build_system_prompt(&state, selected_project.as_ref());
    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(brain::LlmMessage {
        role: "system".to_string(),
        content: system_prompt,
    });
    messages.extend(history.iter().map(history_entry_for_llm));
    messages.push(brain::LlmMessage {
        role: "user".to_string(),
        content: message.to_string(),
    });

    let llm_reply = match brain::call_llm_messages_pub(&state.config.llm_model, &messages).await {
        Ok(reply) => reply,
        Err(err) => {
            let fallback = format!("I hit an LLM error: {err}");
            store_message(&state, project_id.as_deref(), "assistant", &fallback, None);
            return Json(ChatResponse {
                reply: fallback,
                actions: Vec::new(),
            });
        }
    };

    let (reply, pending_actions) = extract_actions(&llm_reply, selected_project.as_ref());
    let actions = execute_actions(state.clone(), selected_project.as_ref(), pending_actions).await;

    let assistant_reply = if reply.trim().is_empty() {
        fallback_reply(&actions)
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

fn build_system_prompt(state: &AppState, selected_project: Option<&ProjectContext>) -> String {
    let projects = load_projects(state);
    let project_list = if projects.is_empty() {
        "- No projects configured yet.".to_string()
    } else {
        projects
            .iter()
            .map(|project| {
                let marker =
                    if selected_project.map(|item| item.id.as_str()) == Some(project.id.as_str()) {
                        " (current)"
                    } else {
                        ""
                    };
                format!("- {}/{}{}", project.owner, project.repo, marker)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let recent_feed = load_recent_feed(
        state,
        selected_project.map(|project| project.id.as_str()),
        12,
    );
    let recent_feed_text = if recent_feed.is_empty() {
        "- No recent task activity.".to_string()
    } else {
        recent_feed.join("\n")
    };

    let current_status = summarize_task_status(
        state,
        selected_project.map(|project| project.id.as_str()),
        8,
    );
    let current_status_text = if current_status.is_empty() {
        "- No tasks yet.".to_string()
    } else {
        current_status
            .iter()
            .map(|task| {
                let issue = task
                    .issue_number
                    .map(|number| format!(" #{}", number))
                    .unwrap_or_default();
                format!(
                    "- [{}] {}{}: {} ({})",
                    task.status, task.repo, issue, task.title, task.created_at
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let knowledge = if let Some(project) = selected_project {
        let store = KnowledgeStore::new();
        let learned = store.load_knowledge(&project.owner, &project.repo);
        let saved_skills = projects
            .iter()
            .find(|item| item.id == project.id)
            .map(|item| item.skills.clone())
            .unwrap_or_default();

        let mut sections = Vec::new();
        if !saved_skills.trim().is_empty() {
            sections.push(format!("## Saved Project Skills\n{}", saved_skills.trim()));
        }
        if !learned.trim().is_empty() {
            sections.push(format!("## knowledge.md\n{}", learned.trim()));
        }
        if sections.is_empty() {
            "No project knowledge saved yet.".to_string()
        } else {
            sections.join("\n\n")
        }
    } else {
        "No project selected.".to_string()
    };

    format!(
        "You are Shipyard, an AI engineering manager. You manage coding agents (Codex, Claude Code) to fix GitHub issues and build features.\n\n\
You have access to the following capabilities:\n\
- DISPATCH: Start a coding task. Reply with [DISPATCH: <description>] to trigger.\n\
- RECON: Run reconnaissance on an issue/repo. Reply with [RECON: owner/repo #issue] to trigger.\n\
- STATUS: Check task status. Reply with [STATUS] to show current tasks.\n\
- KNOWLEDGE: Query what youve learned about a project. Reply with [KNOWLEDGE: owner/repo] to trigger.\n\n\
Current projects:\n\
{project_list}\n\n\
Current task status:\n\
{current_status_text}\n\n\
Recent task history:\n\
{recent_feed_text}\n\n\
Project knowledge:\n\
{knowledge}\n\n\
Be concise, direct, and helpful. You are an engineering lead, not a chatbot.\n\
When the user asks you to do something, do it. Dont ask for confirmation unless genuinely ambiguous.\n\
When showing status, use short summaries not walls of text.\n\
If the user asks you to take action, include the right action tag exactly once per action.\n\
If no project is selected, ask the user to add one or choose one."
    )
}

fn load_projects(state: &AppState) -> Vec<ProjectSummary> {
    let conn = state.db.conn();
    let mut stmt = match conn
        .prepare("SELECT id, owner, repo, skills FROM projects ORDER BY created_at ASC")
    {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    stmt.query_map([], |row| {
        Ok(ProjectSummary {
            id: row.get(0)?,
            owner: row.get(1)?,
            repo: row.get(2)?,
            skills: row.get(3)?,
        })
    })
    .into_iter()
    .flatten()
    .filter_map(|row| row.ok())
    .collect()
}

fn load_recent_feed(state: &AppState, project_id: Option<&str>, limit: usize) -> Vec<String> {
    let conn = state.db.conn();
    let sql = if project_id.is_some() {
        "SELECT e.icon, e.message, e.created_at, p.repo
         FROM task_events e
         JOIN tasks t ON t.id = e.task_id
         LEFT JOIN projects p ON p.id = t.project_id
         WHERE t.project_id = ?1
         ORDER BY e.id DESC
         LIMIT ?2"
    } else {
        "SELECT e.icon, e.message, e.created_at, p.repo
         FROM task_events e
         JOIN tasks t ON t.id = e.task_id
         LEFT JOIN projects p ON p.id = t.project_id
         ORDER BY e.id DESC
         LIMIT ?1"
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    if let Some(project_id) = project_id {
        stmt.query_map(rusqlite::params![project_id, limit as i64], |row| {
            Ok(format!(
                "- {} {} [{}] ({})",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, String>(2)?,
            ))
        })
        .into_iter()
        .flatten()
        .filter_map(|row| row.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(format!(
                "- {} {} [{}] ({})",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, String>(2)?,
            ))
        })
        .into_iter()
        .flatten()
        .filter_map(|row| row.ok())
        .collect()
    }
}

fn summarize_task_status(
    state: &AppState,
    project_id: Option<&str>,
    limit: usize,
) -> Vec<TaskStatusSnapshot> {
    let conn = state.db.conn();
    let sql = if project_id.is_some() {
        "SELECT p.repo, t.issue_number, t.title, t.status, t.created_at
         FROM tasks t
         LEFT JOIN projects p ON p.id = t.project_id
         WHERE t.project_id = ?1
         ORDER BY t.created_at DESC
         LIMIT ?2"
    } else {
        "SELECT p.repo, t.issue_number, t.title, t.status, t.created_at
         FROM tasks t
         LEFT JOIN projects p ON p.id = t.project_id
         ORDER BY t.created_at DESC
         LIMIT ?1"
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    if let Some(project_id) = project_id {
        stmt.query_map(rusqlite::params![project_id, limit as i64], |row| {
            Ok(TaskStatusSnapshot {
                repo: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                issue_number: row.get(1)?,
                title: row.get(2)?,
                status: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .into_iter()
        .flatten()
        .filter_map(|row| row.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(TaskStatusSnapshot {
                repo: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                issue_number: row.get(1)?,
                title: row.get(2)?,
                status: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .into_iter()
        .flatten()
        .filter_map(|row| row.ok())
        .collect()
    }
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
                    .and_then(|value| serde_json::from_str(&value).ok()),
                timestamp: row.get(3)?,
            })
        })
        .into_iter()
        .flatten()
        .filter_map(|row| row.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(ChatMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                actions: row
                    .get::<_, Option<String>>(2)?
                    .and_then(|value| serde_json::from_str(&value).ok()),
                timestamp: row.get(3)?,
            })
        })
        .into_iter()
        .flatten()
        .filter_map(|row| row.ok())
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
    let actions_json = actions.and_then(|value| serde_json::to_string(value).ok());
    let timestamp = chrono::Utc::now().to_rfc3339();
    let _ = state.db.conn().execute(
        "INSERT INTO chat_messages (project_id, role, content, actions, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![project_id, role, content, actions_json, timestamp],
    );
}

fn history_entry_for_llm(message: &ChatMessage) -> brain::LlmMessage {
    let mut content = message.content.clone();
    if let Some(actions) = &message.actions
        && !actions.is_empty()
    {
        let action_lines = actions
            .iter()
            .map(|action| {
                if let Some(detail) = &action.detail {
                    format!("- {}: {} ({})", action.kind, action.summary, detail)
                } else {
                    format!("- {}: {}", action.kind, action.summary)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        content.push_str("\n\nActions taken:\n");
        content.push_str(&action_lines);
    }

    brain::LlmMessage {
        role: message.role.clone(),
        content,
    }
}

fn extract_actions(
    text: &str,
    selected_project: Option<&ProjectContext>,
) -> (String, Vec<PendingAction>) {
    let mut cleaned = String::new();
    let mut actions = Vec::new();
    let mut cursor = 0usize;

    while let Some(relative_open) = text[cursor..].find('[') {
        let open = cursor + relative_open;
        cleaned.push_str(&text[cursor..open]);

        let Some(relative_close) = text[open..].find(']') else {
            cleaned.push_str(&text[open..]);
            cursor = text.len();
            break;
        };

        let close = open + relative_close;
        let token = &text[open + 1..close];
        if let Some(action) = parse_action_token(token, selected_project) {
            actions.push(action);
        } else {
            cleaned.push_str(&text[open..=close]);
        }

        cursor = close + 1;
    }

    cleaned.push_str(&text[cursor..]);
    let cleaned = cleaned
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    (cleaned, actions)
}

fn parse_action_token(
    token: &str,
    selected_project: Option<&ProjectContext>,
) -> Option<PendingAction> {
    let trimmed = token.trim();
    if trimmed.eq_ignore_ascii_case("STATUS") {
        return Some(PendingAction::Status);
    }

    let (kind, payload) = trimmed.split_once(':')?;
    let payload = payload.trim();

    if kind.eq_ignore_ascii_case("DISPATCH") {
        if payload.is_empty() {
            return None;
        }
        return Some(PendingAction::Dispatch {
            description: payload.to_string(),
        });
    }

    if kind.eq_ignore_ascii_case("RECON") {
        let (owner, repo) = parse_repo_target(payload, selected_project)?;
        return Some(PendingAction::Recon {
            owner,
            repo,
            issue_number: tasks::detect_issue_number(payload),
        });
    }

    if kind.eq_ignore_ascii_case("KNOWLEDGE") {
        let (owner, repo) = parse_repo_target(payload, selected_project)?;
        return Some(PendingAction::Knowledge { owner, repo });
    }

    None
}

fn parse_repo_target(
    payload: &str,
    selected_project: Option<&ProjectContext>,
) -> Option<(String, String)> {
    if let Some(repo_ref) = payload.split_whitespace().find(|token| token.contains('/')) {
        let (owner, repo) = repo_ref.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }

    selected_project.map(|project| (project.owner.clone(), project.repo.clone()))
}

async fn execute_actions(
    state: Arc<AppState>,
    selected_project: Option<&ProjectContext>,
    pending: Vec<PendingAction>,
) -> Vec<ChatAction> {
    let mut actions = Vec::new();
    for item in pending {
        match item {
            PendingAction::Dispatch { description } => {
                actions.push(execute_dispatch(state.clone(), selected_project, &description).await);
            }
            PendingAction::Recon {
                owner,
                repo,
                issue_number,
            } => {
                actions.push(execute_recon(state.clone(), &owner, &repo, issue_number).await);
            }
            PendingAction::Status => {
                actions.push(execute_status(&state, selected_project));
            }
            PendingAction::Knowledge { owner, repo } => {
                actions.push(execute_knowledge(&state, &owner, &repo));
            }
        }
    }
    actions
}

async fn execute_dispatch(
    state: Arc<AppState>,
    selected_project: Option<&ProjectContext>,
    description: &str,
) -> ChatAction {
    let Some(project) = selected_project else {
        return ChatAction {
            kind: "dispatch".to_string(),
            summary: "Dispatch blocked".to_string(),
            detail: Some("No project selected.".to_string()),
        };
    };

    let issue_number = tasks::detect_issue_number(description);
    match tasks::launch_task(
        state.clone(),
        LaunchTaskRequest {
            project_id: project.id.clone(),
            issue_number,
            title: description.to_string(),
            model: None,
            agent_type: Some("codex".to_string()),
            quality_gates: Some(crate::agents::QualityGates {
                tests: true,
                clippy: true,
                review: true,
                auto_merge: true,
            }),
            extra_instructions: None,
            auto_merge: true,
        },
    )
    .await
    {
        Ok(task) => {
            tasks::add_event(
                &state,
                &task.id,
                "brain",
                "🧠",
                &format!("Chat dispatch: {}", description),
                None,
            );
            ChatAction {
                kind: "dispatch".to_string(),
                summary: format!("Task dispatched: {}", task.title),
                detail: Some(format!(
                    "Task {} running on {}/{}",
                    task.id, project.owner, project.repo
                )),
            }
        }
        Err(err) => ChatAction {
            kind: "dispatch".to_string(),
            summary: "Dispatch failed".to_string(),
            detail: Some(err.to_string()),
        },
    }
}

async fn execute_recon(
    _state: Arc<AppState>,
    owner: &str,
    repo: &str,
    issue_number: Option<i64>,
) -> ChatAction {
    let repo_path = tasks::repo_checkout_path(owner, repo);
    let recon = recon::run_recon(owner, repo, issue_number, &repo_path).await;
    let issue_label = issue_number
        .map(|number| format!(" #{}", number))
        .unwrap_or_default();

    ChatAction {
        kind: "recon".to_string(),
        summary: format!("Recon: {owner}/{repo}{issue_label}"),
        detail: Some(format_recon_detail(&recon)),
    }
}

fn execute_status(state: &AppState, selected_project: Option<&ProjectContext>) -> ChatAction {
    let snapshots = summarize_task_status(
        state,
        selected_project.map(|project| project.id.as_str()),
        8,
    );
    let running = snapshots
        .iter()
        .filter(|task| task.status == "running")
        .count();
    let done = snapshots
        .iter()
        .filter(|task| task.status == "done")
        .count();
    let failed = snapshots
        .iter()
        .filter(|task| matches!(task.status.as_str(), "failed" | "killed"))
        .count();

    let detail = if snapshots.is_empty() {
        "No tasks yet.".to_string()
    } else {
        snapshots
            .iter()
            .map(|task| {
                let issue = task
                    .issue_number
                    .map(|number| format!(" #{}", number))
                    .unwrap_or_default();
                format!("[{}] {}{}: {}", task.status, task.repo, issue, task.title)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    ChatAction {
        kind: "status".to_string(),
        summary: format!("Status: {running} running, {done} done, {failed} failed"),
        detail: Some(detail),
    }
}

fn execute_knowledge(state: &AppState, owner: &str, repo: &str) -> ChatAction {
    let store = KnowledgeStore::new();
    let knowledge = store.load_knowledge(owner, repo);
    let project = find_project_by_repo(state, owner, repo);
    let skills = project
        .and_then(|project| {
            load_projects(state)
                .into_iter()
                .find(|item| item.id == project.id)
                .map(|item| item.skills)
        })
        .unwrap_or_default();

    let detail = if knowledge.trim().is_empty() && skills.trim().is_empty() {
        "No saved project knowledge yet.".to_string()
    } else {
        let mut parts = Vec::new();
        if !skills.trim().is_empty() {
            parts.push(skills.trim().to_string());
        }
        if !knowledge.trim().is_empty() {
            parts.push(knowledge.trim().to_string());
        }
        truncate_detail(&parts.join("\n\n"), 1200)
    };

    ChatAction {
        kind: "knowledge".to_string(),
        summary: format!("Knowledge: {owner}/{repo}"),
        detail: Some(detail),
    }
}

fn find_project_by_repo(state: &AppState, owner: &str, repo: &str) -> Option<ProjectContext> {
    let conn = state.db.conn();
    conn.query_row(
        "SELECT id, owner, repo, default_branch FROM projects WHERE owner = ?1 AND repo = ?2",
        rusqlite::params![owner, repo],
        |row| {
            Ok(ProjectContext {
                id: row.get(0)?,
                owner: row.get(1)?,
                repo: row.get(2)?,
                default_branch: row.get(3)?,
            })
        },
    )
    .ok()
}

fn format_recon_detail(report: &recon::ReconReport) -> String {
    let issue = report
        .issue
        .as_ref()
        .map(|issue| format!("Issue: {}", issue.title))
        .unwrap_or_else(|| "Issue: unavailable".to_string());
    let branch = report
        .existing_branch
        .as_ref()
        .map(|branch| format!("Existing branch: {branch}"))
        .unwrap_or_else(|| "Existing branch: none".to_string());
    let tests = report
        .baseline_tests
        .as_ref()
        .map(|tests| {
            format!(
                "Baseline tests: {}",
                if tests.success { "passing" } else { "failing" }
            )
        })
        .unwrap_or_else(|| "Baseline tests: unavailable".to_string());
    let prs = if report.related_prs.is_empty() {
        "Related PRs: none".to_string()
    } else {
        format!(
            "Related PRs: {}",
            report
                .related_prs
                .iter()
                .map(|pr| format!("#{} {}", pr.number, pr.title))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let key_files = if report.key_files.is_empty() {
        "Key files: none".to_string()
    } else {
        format!(
            "Key files: {}",
            report
                .key_files
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    truncate_detail(
        &format!("{issue}\n{branch}\n{tests}\n{prs}\n{key_files}"),
        1200,
    )
}

fn truncate_detail(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_string();
    }

    let truncated = text.chars().take(max_len).collect::<String>();
    format!("{}...", truncated.trim_end())
}

fn fallback_reply(actions: &[ChatAction]) -> String {
    if actions.is_empty() {
        "I need a bit more context.".to_string()
    } else {
        actions
            .iter()
            .map(|action| action.summary.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
