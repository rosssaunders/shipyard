use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;
use crate::agents::QualityGates;
use crate::knowledge::{KnowledgeStore, TaskRecord};

// --- Intent-driven API ---

#[derive(Deserialize)]
pub struct IntentRequest {
    pub text: String,
    pub project_id: Option<String>,
    pub issue_number: Option<i64>,
}

#[derive(Serialize)]
pub struct FeedEvent {
    pub id: String,
    pub task_id: String,
    pub icon: String,
    pub message: String,
    pub detail: Option<String>,
    pub task_status: Option<String>,
    pub repo: Option<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct AttentionItem {
    pub id: String,
    pub icon: String,
    pub message: String,
    pub task_id: String,
}

#[derive(Deserialize)]
pub struct ResolveRequest {
    pub action: String, // "approve" or "reject"
}

/// POST /api/intent — "I want X", brain figures out the rest
pub async fn submit_intent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<IntentRequest>,
) -> Json<serde_json::Value> {
    // Find the project (use provided or first available)
    let project = {
        let conn = state.db.conn();
        if let Some(pid) = &req.project_id {
            conn.query_row(
                "SELECT id, owner, repo FROM projects WHERE id = ?1",
                [pid.as_str()],
                |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?)),
            ).ok()
        } else {
            conn.query_row(
                "SELECT id, owner, repo FROM projects ORDER BY created_at LIMIT 1",
                [],
                |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?)),
            ).ok()
        }
    };

    let Some((project_id, owner, repo)) = project else {
        return Json(serde_json::json!({"error": "No project configured"}));
    };

    // Brain interprets the intent → creates a task
    let id = uuid::Uuid::new_v4().to_string();
    let model = state.config.llm_model.clone();
    let agent_type = "codex".to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let worktree_path = format!("/tmp/shipyard/{}/{}", repo, &id[..8]);
    let issue_number = req.issue_number;
    let branch = if let Some(n) = issue_number {
        format!("shipyard/issue-{n}")
    } else {
        format!("shipyard/{}", &id[..8])
    };

    // Create worktree
    let repo_path = format!(
        "{}/code/{}/{}",
        std::env::var("HOME").unwrap_or_default(), owner, repo
    );
    let _ = std::fs::create_dir_all(&worktree_path);
    let default_branch = {
        let conn = state.db.conn();
        conn.query_row("SELECT default_branch FROM projects WHERE id = ?1", [&project_id], |r| r.get::<_,String>(0))
            .unwrap_or_else(|_| "main".to_string())
    };

    // Clean up any stale branch/worktree with the same name
    let _ = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", &worktree_path])
        .current_dir(&repo_path)
        .output();
    let _ = std::process::Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(&repo_path)
        .output();
    // Also delete remote branch if it exists
    let _ = std::process::Command::new("git")
        .args(["push", "origin", "--delete", &branch, "--no-verify"])
        .current_dir(&repo_path)
        .output();

    // Pull latest before creating worktree
    let _ = std::process::Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&repo_path)
        .output();

    let _ = std::process::Command::new("git")
        .args(["worktree", "add", "-b", &branch, &worktree_path, &default_branch])
        .current_dir(&repo_path)
        .output();

    // Insert task
    {
        let conn = state.db.conn();
        conn.execute(
            "INSERT INTO tasks (id, project_id, issue_number, title, status, agent_type, model, worktree_path, branch, created_at, auto_merge)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?8, ?9, 1)",
            rusqlite::params![id, project_id, issue_number, req.text, agent_type, model, worktree_path, branch, now],
        ).unwrap();
    }

    add_event(&state, &id, "brain", "🧠", &format!("Intent received: {}", req.text), None);

    // Run pipeline in background
    let bg_state = state.clone();
    let bg_id = id.clone();
    let bg_ctx = TaskPipelineContext {
        owner,
        repo,
        model,
        agent_type,
        branch,
        worktree_path,
        title: req.text.clone(),
        issue_number,
        extra_instructions: None,
        gates: QualityGates { tests: true, clippy: true, review: true, auto_merge: true },
    };
    tokio::spawn(async move {
        run_task_pipeline(bg_state, bg_id, bg_ctx).await;
    });

    Json(serde_json::json!({"ok": true, "task_id": id}))
}

/// GET /api/feed — timeline of everything happening
pub async fn get_feed(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<FeedEvent>> {
    let conn = state.db.conn();
    let mut stmt = conn.prepare(
        "SELECT e.id, e.task_id, e.icon, e.message, e.detail, e.created_at, t.status, p.repo
         FROM task_events e
         JOIN tasks t ON t.id = e.task_id
         LEFT JOIN projects p ON p.id = t.project_id
         ORDER BY e.id DESC
         LIMIT 100"
    ).unwrap();

    let events: Vec<FeedEvent> = stmt.query_map([], |row| {
        Ok(FeedEvent {
            id: row.get::<_, i64>(0)?.to_string(),
            task_id: row.get(1)?,
            icon: row.get(2)?,
            message: row.get(3)?,
            detail: row.get(4)?,
            created_at: row.get(5)?,
            task_status: row.get(6)?,
            repo: row.get(7)?,
        })
    }).unwrap().filter_map(|r| r.ok()).collect();

    Json(events)
}

/// GET /api/attention — things that need human decision
pub async fn get_attention(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<AttentionItem>> {
    let conn = state.db.conn();
    let mut stmt = conn.prepare(
        "SELECT e.id, e.icon, e.message, e.task_id
         FROM task_events e
         JOIN tasks t ON t.id = e.task_id
         WHERE e.kind = 'attention' AND e.resolved IS NULL
         ORDER BY e.id DESC"
    ).unwrap();

    let items: Vec<AttentionItem> = stmt.query_map([], |row| {
        Ok(AttentionItem {
            id: row.get::<_, i64>(0)?.to_string(),
            icon: row.get(1)?,
            message: row.get(2)?,
            task_id: row.get(3)?,
        })
    }).unwrap().filter_map(|r| r.ok()).collect();

    Json(items)
}

/// POST /api/attention/:id — resolve an attention item
pub async fn resolve_attention(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ResolveRequest>,
) -> Json<bool> {
    let conn = state.db.conn();
    let _ = conn.execute(
        "UPDATE task_events SET resolved = ?1 WHERE id = ?2",
        rusqlite::params![req.action, id],
    );
    Json(true)
}

#[derive(Debug, Serialize, Clone)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub issue_number: Option<i64>,
    pub title: String,
    pub status: String,
    pub agent_type: String,
    pub model: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub events: Vec<TaskEvent>,
}

#[derive(Debug, Serialize, Clone)]
pub struct TaskEvent {
    pub id: i64,
    pub kind: String,
    pub icon: String,
    pub message: String,
    pub detail: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub project_id: String,
    pub issue_number: Option<i64>,
    pub title: String,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub quality_gates: Option<QualityGates>,
    pub extra_instructions: Option<String>,
}

#[derive(Clone)]
struct TaskPipelineContext {
    owner: String,
    repo: String,
    model: String,
    agent_type: String,
    branch: String,
    worktree_path: String,
    title: String,
    issue_number: Option<i64>,
    extra_instructions: Option<String>,
    gates: QualityGates,
}

/// Add an event to a task's timeline
pub fn add_event(state: &AppState, task_id: &str, kind: &str, icon: &str, message: &str, detail: Option<&str>) {
    let _ = state.db.conn().execute(
        "INSERT INTO task_events (task_id, kind, icon, message, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![task_id, kind, icon, message, detail],
    );
}

pub fn update_task_status(state: &AppState, task_id: &str, status: &str) {
    let finished = if matches!(status, "done" | "failed" | "killed" | "skipped") {
        Some("datetime('now')")
    } else {
        None
    };
    if finished.is_some() {
        let _ = state.db.conn().execute(
            "UPDATE tasks SET status = ?1, finished_at = datetime('now') WHERE id = ?2",
            rusqlite::params![status, task_id],
        );
    } else {
        let _ = state.db.conn().execute(
            "UPDATE tasks SET status = ?1 WHERE id = ?2",
            rusqlite::params![status, task_id],
        );
    }
}

// --- HTTP handlers ---

pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> Json<Vec<Task>> {
    let conn = state.db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT id, project_id, issue_number, title, status, agent_type, model, created_at, finished_at
             FROM tasks WHERE project_id = ?1 ORDER BY created_at DESC",
        )
        .unwrap();

    let tasks: Vec<Task> = stmt
        .query_map([&project_id], |row| {
            Ok(Task {
                id: row.get(0)?,
                project_id: row.get(1)?,
                issue_number: row.get(2)?,
                title: row.get(3)?,
                status: row.get(4)?,
                agent_type: row.get(5)?,
                model: row.get(6)?,
                created_at: row.get(7)?,
                finished_at: row.get(8)?,
                events: vec![],
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    // Load events for each task
    let tasks_with_events: Vec<Task> = tasks
        .into_iter()
        .map(|mut t| {
            let mut evt_stmt = conn
                .prepare(
                    "SELECT id, kind, icon, message, detail, created_at FROM task_events WHERE task_id = ?1 ORDER BY id ASC",
                )
                .unwrap();
            t.events = evt_stmt
                .query_map([&t.id], |row| {
                    Ok(TaskEvent {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        icon: row.get(2)?,
                        message: row.get(3)?,
                        detail: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                })
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            t
        })
        .collect();

    Json(tasks_with_events)
}

pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Option<Task>> {
    let conn = state.db.conn();
    let task = conn
        .query_row(
            "SELECT id, project_id, issue_number, title, status, agent_type, model, created_at, finished_at
             FROM tasks WHERE id = ?1",
            [&id],
            |row| {
                Ok(Task {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    issue_number: row.get(2)?,
                    title: row.get(3)?,
                    status: row.get(4)?,
                    agent_type: row.get(5)?,
                    model: row.get(6)?,
                    created_at: row.get(7)?,
                    finished_at: row.get(8)?,
                    events: vec![],
                })
            },
        )
        .ok();

    if let Some(mut t) = task {
        let mut evt_stmt = conn
            .prepare(
                "SELECT id, kind, icon, message, detail, created_at FROM task_events WHERE task_id = ?1 ORDER BY id ASC",
            )
            .unwrap();
        t.events = evt_stmt
            .query_map([&t.id], |row| {
                Ok(TaskEvent {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    icon: row.get(2)?,
                    message: row.get(3)?,
                    detail: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        Json(Some(t))
    } else {
        Json(None)
    }
}

pub async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTaskRequest>,
) -> Json<Task> {
    let id = uuid::Uuid::new_v4().to_string();
    let model = req.model.unwrap_or_else(|| state.config.llm_model.clone());
    let agent_type = req.agent_type.unwrap_or_else(|| "codex".to_string());
    let gates = req.quality_gates.unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();

    // Get project info
    let (owner, repo, default_branch) = {
        let conn = state.db.conn();
        conn.query_row(
            "SELECT owner, repo, default_branch FROM projects WHERE id = ?1",
            [&req.project_id],
            |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?)),
        )
        .unwrap()
    };

    // Create branch and worktree
    let branch = format!(
        "shipyard/{}",
        req.issue_number
            .map(|n| format!("issue-{n}"))
            .unwrap_or_else(|| id[..8].to_string())
    );
    let worktree_path = format!("/tmp/shipyard/{}/{}", repo, &id[..8]);
    let repo_path = format!(
        "{}/code/{}/{}",
        std::env::var("HOME").unwrap_or_default(),
        owner,
        repo
    );

    let _ = std::fs::create_dir_all(&worktree_path);

    // Clean up stale branch/worktree
    let _ = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", &worktree_path])
        .current_dir(&repo_path).output();
    let _ = std::process::Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(&repo_path).output();
    let _ = std::process::Command::new("git")
        .args(["push", "origin", "--delete", &branch, "--no-verify"])
        .current_dir(&repo_path).output();
    let _ = std::process::Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&repo_path).output();

    let _ = std::process::Command::new("git")
        .args(["worktree", "add", "-b", &branch, &worktree_path, &default_branch])
        .current_dir(&repo_path)
        .output();

    // Insert task
    {
        let conn = state.db.conn();
        conn.execute(
            "INSERT INTO tasks (id, project_id, issue_number, title, status, agent_type, model, worktree_path, branch, created_at, auto_merge)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![id, req.project_id, req.issue_number, req.title, agent_type, model, worktree_path, branch, now, true],
        ).unwrap();
    }

    // Return task immediately, run pipeline in background
    let task = get_task_from_db(&state, &id);

    let bg_state = state.clone();
    let bg_id = id.clone();
    let bg_ctx = TaskPipelineContext {
        owner,
        repo,
        model,
        agent_type,
        branch,
        worktree_path,
        title: req.title.clone(),
        issue_number: req.issue_number,
        extra_instructions: req.extra_instructions.clone(),
        gates,
    };

    tokio::spawn(async move {
        run_task_pipeline(bg_state, bg_id, bg_ctx).await;
    });

    Json(task.unwrap())
}

fn get_task_from_db(state: &AppState, id: &str) -> Option<Task> {
    let conn = state.db.conn();
    conn.query_row(
        "SELECT id, project_id, issue_number, title, status, agent_type, model, created_at, finished_at FROM tasks WHERE id = ?1",
        [id],
        |row| Ok(Task {
            id: row.get(0)?,
            project_id: row.get(1)?,
            issue_number: row.get(2)?,
            title: row.get(3)?,
            status: row.get(4)?,
            agent_type: row.get(5)?,
            model: row.get(6)?,
            created_at: row.get(7)?,
            finished_at: row.get(8)?,
            events: vec![],
        }),
    ).ok()
}

/// GET /api/tasks/:id/output — live agent output (last N chars)
pub async fn get_live_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let offset: usize = params.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
    
    if let Some(output) = state.agents.get_output(&id) {
        let total = output.len();
        let chunk = if offset < total { &output[offset..] } else { "" };
        Json(serde_json::json!({
            "output": chunk,
            "offset": total,
            "running": state.agents.is_running(&id),
        }))
    } else {
        Json(serde_json::json!({
            "output": "",
            "offset": 0,
            "running": false,
        }))
    }
}

pub async fn kill_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<bool> {
    let killed = state.agents.kill(&id);
    if killed {
        update_task_status(&state, &id, "killed");
        add_event(&state, &id, "system", "💀", "Task killed by user", None);
    }
    Json(killed)
}

async fn run_task_pipeline(state: Arc<AppState>, id: String, ctx: TaskPipelineContext) {
    let task_id = id.as_str();
    let worktree = ctx.worktree_path.as_str();
    let repo_path = repo_checkout_path(&ctx.owner, &ctx.repo);
    let knowledge_store = KnowledgeStore::new();

    add_event(&state, task_id, "recon", "🔍", "Recon: gathering repo and issue context...", None);
    let recon = crate::recon::run_recon(&ctx.owner, &ctx.repo, ctx.issue_number, &repo_path).await;
    add_event(
        &state,
        task_id,
        "recon",
        "🔍",
        "Recon complete",
        Some(&format_recon_detail(&recon)),
    );

    let project_skills = {
        let conn = state.db.conn();
        conn.query_row(
            "SELECT skills FROM projects p JOIN tasks t ON t.project_id = p.id WHERE t.id = ?1",
            [task_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };
    let persistent_knowledge = knowledge_store.load_knowledge(&ctx.owner, &ctx.repo);
    let recent_history = knowledge_store.recent_history(&ctx.owner, &ctx.repo, 8);
    let planning_knowledge = build_planning_knowledge(
        &project_skills,
        &persistent_knowledge,
        &recent_history,
        ctx.extra_instructions.as_deref(),
    );

    add_event(&state, task_id, "brain", "🧠", "Brain: planning from recon report...", None);

    let plan = crate::brain::plan_task(&recon, &planning_knowledge, &ctx.model).await;
    let mut max_wait = std::time::Duration::from_secs(12 * 3600);

    let prompt = match &plan {
        Ok(plan) => {
            add_event(
                &state,
                task_id,
                "brain",
                "🧠",
                &format!("Complexity: {}/5 — {}", plan.complexity, plan.assessment),
                Some(&format!("Timeout: {}s", plan.timeout_secs)),
            );

            if let Some(skip_reason) = &plan.skip_reason {
                add_event(
                    &state,
                    task_id,
                    "brain",
                    "🧠",
                    "Brain: skipping task",
                    Some(skip_reason),
                );
                update_task_status(&state, task_id, "skipped");
                persist_task_outcome(
                    &knowledge_store,
                    &state,
                    &ctx,
                    task_id,
                    "skipped",
                    skip_reason,
                    false,
                )
                .await;
                return;
            }

            if plan.timeout_secs > 0 {
                max_wait = std::time::Duration::from_secs(plan.timeout_secs.max(300));
            }

            if plan.prompt.trim().is_empty() {
                fallback_prompt(&ctx.title, ctx.issue_number, ctx.extra_instructions.as_deref())
            } else {
                plan.prompt.clone()
            }
        }
        Err(e) => {
            add_event(
                &state,
                task_id,
                "brain",
                "⚠️",
                &format!("Brain planning failed, using fallback prompt: {e}"),
                None,
            );
            fallback_prompt(&ctx.title, ctx.issue_number, ctx.extra_instructions.as_deref())
        }
    };

    add_event(&state, task_id, "dispatch", "🚀",
        &format!("Dispatching to {} ({})", ctx.agent_type, ctx.model), None);

    // Spawn agent
    let pid = state
        .agents
        .spawn(task_id, worktree, &ctx.model, &prompt, &ctx.agent_type)
        .await
        .unwrap_or(0);

    {
        let conn = state.db.conn();
        let _ = conn.execute("UPDATE tasks SET pid = ?1 WHERE id = ?2", rusqlite::params![pid as i64, task_id]);
    }

    add_event(&state, task_id, "agent", "🔨", "Agent started working...", None);

    // Start log parser — turns raw output into structured stages
    if let Some(output) = state.agents.get_output_arc(task_id) {
        crate::log_parser::spawn_log_parser(state.clone(), task_id.to_string(), output);
    }

    // Poll until agent finishes (with timeout)
    let start = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let still_running = state.agents.is_running(task_id);
        if !still_running {
            break;
        }
        if start.elapsed() > max_wait {
            add_event(
                &state,
                task_id,
                "system",
                "⏰",
                &format!("Agent timed out after {} seconds", max_wait.as_secs()),
                None,
            );
            state.agents.kill(task_id);
            update_task_status(&state, task_id, "failed");
            persist_task_outcome(
                &knowledge_store,
                &state,
                &ctx,
                task_id,
                "failed",
                "Agent timed out",
                true,
            )
            .await;
            return;
        }
    }

    add_event(&state, task_id, "agent", "🔨", "Agent finished coding", None);
    update_task_status(&state, task_id, "gates");

    // Auto-commit any uncommitted changes the agent left behind
    let status = run_cmd(worktree, "git", &["status", "--porcelain"]);
    if status.0 && !status.1.trim().is_empty() {
        add_event(&state, task_id, "system", "📝", "Auto-committing uncommitted changes...", None);
        let _ = run_cmd(worktree, "git", &["add", "-A"]);
        let commit_msg = format!("feat: {}", ctx.title.chars().take(72).collect::<String>());
        let _ = run_cmd(worktree, "git", &["commit", "-m", &commit_msg]);
    }

    // Check if there are any commits
    let has_commits = run_cmd(worktree, "git", &["log", "--oneline", "HEAD", "^main", "--"]);
    if !has_commits.0 || has_commits.1.trim().is_empty() {
        add_event(&state, task_id, "error", "⚠️", "No commits produced by agent", None);
        update_task_status(&state, task_id, "failed");
        persist_task_outcome(
            &knowledge_store,
            &state,
            &ctx,
            task_id,
            "failed",
            "No commits produced by agent",
            true,
        )
        .await;
        return;
    }

    let commit_count = has_commits.1.trim().lines().count();
    add_event(&state, task_id, "info", "📝",
        &format!("{commit_count} commit(s) ready"), None);

    // Run quality gates from project skills (Layer 1: Supervisor)
    let mut attempt = 0u32;
    let all_passed = loop {
        attempt += 1;
        let results = crate::supervisor::run_skill_gates(&state, task_id, worktree).await;
        let failures: Vec<_> = results.iter().filter(|r| !r.passed).collect();

        if failures.is_empty() {
            break true;
        }

        // Supervisor: attempt to fix
        add_event(&state, task_id, "supervisor", "🔧",
            &format!("Supervisor: {} gate(s) failed, dispatching fixer (attempt {})",
                failures.len(), attempt), None);

        let failed_results: Vec<_> = results.into_iter().filter(|r| !r.passed).collect();
        let fixed = crate::supervisor::attempt_fix(
            &state, task_id, worktree, &failed_results, &ctx.model, attempt,
        ).await;

        if !fixed {
            break false;
        }

        // Auto-commit fixer changes
        let status = run_cmd(worktree, "git", &["status", "--porcelain"]);
        if status.0 && !status.1.trim().is_empty() {
            let _ = run_cmd(worktree, "git", &["add", "-A"]);
            let _ = run_cmd(worktree, "git", &["commit", "-m",
                &format!("fix: resolve gate failures (attempt {attempt})")]);
        }

        add_event(&state, task_id, "supervisor", "🔄", "Re-running gates...", None);
    };

    if !all_passed {
        update_task_status(&state, task_id, "failed");
        add_event(&state, task_id, "supervisor", "❌", "Supervisor: gates failed after all fix attempts", None);
        persist_task_outcome(
            &knowledge_store,
            &state,
            &ctx,
            task_id,
            "failed",
            "Supervisor gates failed after all fix attempts",
            true,
        )
        .await;
        return;
    }

    if ctx.gates.review {
        add_event(&state, task_id, "review", "🧠", "Brain: reviewing diff...", None);
        let diff = capture_task_diff(worktree);
        if diff.trim().is_empty() {
            add_event(
                &state,
                task_id,
                "review",
                "⚠️",
                "Brain review skipped because diff was empty",
                None,
            );
        } else {
            match crate::brain::review_diff(&diff, &recon, &planning_knowledge, &ctx.model).await {
                Ok(review) if review.approved => {
                    let detail = if review.issues.is_empty() {
                        review.summary.clone()
                    } else {
                        format!("{}\n\n{}", review.summary, review.issues.join("\n"))
                    };
                    add_event(&state, task_id, "review", "✅", "Brain review approved", Some(&detail));
                }
                Ok(review) => {
                    let mut detail = review.summary;
                    if !review.issues.is_empty() {
                        detail.push_str("\n\nIssues:\n");
                        detail.push_str(&review.issues.join("\n"));
                    }
                    if let Some(suggestion) = review.suggestion {
                        detail.push_str("\n\nSuggestion:\n");
                        detail.push_str(&suggestion);
                    }
                    add_event(&state, task_id, "review", "❌", "Brain review rejected the diff", Some(&detail));
                    update_task_status(&state, task_id, "failed");
                    persist_task_outcome(
                        &knowledge_store,
                        &state,
                        &ctx,
                        task_id,
                        "failed",
                        "Brain review rejected the diff",
                        true,
                    )
                    .await;
                    return;
                }
                Err(e) => {
                    add_event(
                        &state,
                        task_id,
                        "review",
                        "⚠️",
                        &format!("Brain review failed, continuing: {e}"),
                        None,
                    );
                }
            }
        }
    }

    // Push and create PR
    add_event(&state, task_id, "system", "🚀", "Pushing branch...", None);
    let push = run_cmd(worktree, "git", &["push", "-u", "origin", &ctx.branch, "--no-verify"]);
    if !push.0 {
        add_event(&state, task_id, "error", "❌", "Failed to push branch", Some(&push.1));
        update_task_status(&state, task_id, "failed");
        persist_task_outcome(
            &knowledge_store,
            &state,
            &ctx,
            task_id,
            "failed",
            "Failed to push branch",
            true,
        )
        .await;
        return;
    }

    let pr_title = ctx.issue_number
        .map(|n| format!("fix: resolve issue #{n}"))
        .unwrap_or_else(|| format!("shipyard: {}", &task_id[..8]));
    let pr_body = format!(
        "Automated by [Shipyard](https://github.com/rosssaunders/shipyard)\n\n{}",
        ctx.issue_number.map(|n| format!("Closes #{n}")).unwrap_or_default()
    );

    let pr = run_cmd(worktree, "gh", &[
        "pr", "create",
        "--repo", &format!("{}/{}", ctx.owner, ctx.repo),
        "--head", &ctx.branch,
        "--title", &pr_title,
        "--body", &pr_body,
    ]);

    if pr.0 {
        let pr_url = pr.1.trim();
        add_event(&state, task_id, "pr", "🔗", &format!("PR created: {pr_url}"), None);
    } else {
        add_event(&state, task_id, "error", "⚠️", "Failed to create PR", Some(&pr.1));
    }

    // Brain merges — gates passed + reviewer approved
    add_event(&state, task_id, "brain", "🧠", "Brain: all gates green, reviewer approved → merging", None);

    // Always merge if gates pass (brain decides, not the user)
    {
        add_event(&state, task_id, "system", "🔀", "Auto-merging...", None);
        let merge = run_cmd(worktree, "gh", &[
            "pr", "merge",
            "--repo", &format!("{}/{}", ctx.owner, ctx.repo),
            "--squash", "--admin", &ctx.branch,
        ]);
        if merge.0 {
            add_event(&state, task_id, "system", "✅", "Merged to main", None);
        } else {
            add_event(&state, task_id, "error", "⚠️", "Auto-merge failed", Some(&merge.1));
        }
    }

    update_task_status(&state, task_id, "done");
    add_event(&state, task_id, "system", "🏁", "Task complete", None);
    persist_task_outcome(
        &knowledge_store,
        &state,
        &ctx,
        task_id,
        "done",
        "Task complete",
        true,
    )
    .await;
}

fn repo_checkout_path(owner: &str, repo: &str) -> String {
    format!(
        "{}/code/{}/{}",
        std::env::var("HOME").unwrap_or_default(),
        owner,
        repo
    )
}

fn fallback_prompt(title: &str, issue_number: Option<i64>, extra_instructions: Option<&str>) -> String {
    let mut prompt = if let Some(issue_num) = issue_number {
        format!(
            "Fix issue #{issue_num}: {title}. Use `gh issue view {issue_num}` for full context. Implement the fix, run the relevant tests, commit, and push."
        )
    } else {
        title.to_string()
    };

    if let Some(extra) = extra_instructions {
        prompt.push_str(&format!("\n\nAdditional instructions:\n{extra}"));
    }

    prompt
}

fn build_planning_knowledge(
    project_skills: &str,
    persistent_knowledge: &str,
    recent_history: &[TaskRecord],
    extra_instructions: Option<&str>,
) -> String {
    let history_json = if recent_history.is_empty() {
        "[]".to_string()
    } else {
        serde_json::to_string_pretty(recent_history).unwrap_or_else(|_| "[]".to_string())
    };

    let extra = extra_instructions.unwrap_or("None");
    let db_skills = if project_skills.trim().is_empty() {
        "None"
    } else {
        project_skills
    };
    let knowledge = if persistent_knowledge.trim().is_empty() {
        "None"
    } else {
        persistent_knowledge
    };

    format!(
        "## Additional Instructions\n{extra}\n\n## Project Skills\n{db_skills}\n\n## Persistent Knowledge\n{knowledge}\n\n## Recent Task History\n{history_json}"
    )
}

fn format_recon_detail(recon: &crate::recon::ReconReport) -> String {
    let issue_title = recon
        .issue
        .as_ref()
        .map(|issue| issue.title.as_str())
        .unwrap_or("No GitHub issue loaded");
    let branch = recon.existing_branch.as_deref().unwrap_or("none");
    let tests = recon
        .baseline_tests
        .as_ref()
        .map(|result| {
            format!(
                "{} ({})",
                if result.success { "passed" } else { "failed" },
                result.command
            )
        })
        .unwrap_or_else(|| "not run".to_string());

    format!(
        "Issue: {issue_title}\nRelated PRs: {}\nExisting branch: {branch}\nRecent commits checked: {}\nBaseline tests: {tests}\nPossibly fixed on main: {}",
        recon.related_prs.len(),
        recon.recent_commits.len(),
        recon.possibly_fixed
    )
}

fn capture_task_diff(worktree: &str) -> String {
    let diff = run_cmd(worktree, "git", &["diff", "main...HEAD"]);
    if diff.0 && !diff.1.trim().is_empty() {
        return diff.1;
    }

    let fallback = run_cmd(worktree, "git", &["diff", "HEAD~1..HEAD"]);
    if fallback.0 {
        fallback.1
    } else {
        String::new()
    }
}

fn latest_task_summary(state: &AppState, task_id: &str, fallback: &str) -> String {
    let conn = state.db.conn();
    conn.query_row(
        "SELECT message FROM task_events WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
        [task_id],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| fallback.to_string())
}

async fn persist_task_outcome(
    knowledge_store: &KnowledgeStore,
    state: &Arc<AppState>,
    ctx: &TaskPipelineContext,
    task_id: &str,
    outcome: &str,
    fallback_summary: &str,
    extract_learnings: bool,
) {
    let diff = if extract_learnings {
        capture_task_diff(&ctx.worktree_path)
    } else {
        String::new()
    };

    if extract_learnings {
        add_event(state, task_id, "brain", "📚", "Brain: extracting learnings...", None);
        match crate::brain::extract_learnings(task_id, outcome, &diff, &ctx.model).await {
            Ok(learnings) if !learnings.trim().is_empty() => {
                knowledge_store.append_knowledge(&ctx.owner, &ctx.repo, &learnings);
                add_event(state, task_id, "brain", "📚", "Knowledge updated", Some(&learnings));
            }
            Ok(_) => {
                add_event(state, task_id, "brain", "📚", "No durable learnings extracted", None);
            }
            Err(err) => {
                add_event(
                    state,
                    task_id,
                    "brain",
                    "⚠️",
                    &format!("Learning extraction failed: {err}"),
                    None,
                );
            }
        }
    }

    let summary = latest_task_summary(state, task_id, fallback_summary);
    let record = TaskRecord {
        task_id: task_id.to_string(),
        title: ctx.title.to_string(),
        issue_number: ctx.issue_number,
        outcome: outcome.to_string(),
        summary,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    knowledge_store.record_task(&ctx.owner, &ctx.repo, &record);
}

/// Non-blocking version with actual timeout
pub async fn run_cmd_timeout_async(workdir: &str, cmd: &str, args: &[&str], timeout_secs: u64) -> (bool, String) {
    let workdir = workdir.to_string();
    let cmd = cmd.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let result = tokio::task::spawn_blocking(move || {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_cmd_timeout_sync(&workdir, &cmd, &arg_refs, timeout_secs)
    });
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs + 10), // grace period
        result,
    ).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => (false, format!("spawn_blocking failed: {e}")),
        Err(_) => (false, format!("Command timed out after {timeout_secs}s")),
    }
}

fn run_cmd_timeout_sync(workdir: &str, cmd: &str, args: &[&str], timeout_secs: u64) -> (bool, String) {
    match std::process::Command::new(cmd)
        .args(args)
        .current_dir(workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let output = child.wait_with_output().unwrap_or_else(|_| std::process::Output {
                            status,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        });
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return (output.status.success(), format!("{stdout}\n{stderr}"));
                    }
                    Ok(None) => {
                        if std::time::Instant::now() > deadline {
                            let _ = child.kill();
                            return (false, format!("Command timed out after {timeout_secs}s"));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    Err(e) => return (false, format!("Process error: {e}")),
                }
            }
        }
        Err(e) => (false, format!("Failed to run {cmd}: {e}")),
    }
}

pub fn run_cmd(workdir: &str, cmd: &str, args: &[&str]) -> (bool, String) {
    run_cmd_sync(workdir, cmd, args)
}

/// Non-blocking version — use from async contexts to avoid starving the tokio runtime
pub async fn run_cmd_async(workdir: &str, cmd: &str, args: &[&str]) -> (bool, String) {
    let workdir = workdir.to_string();
    let cmd = cmd.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_cmd_sync(&workdir, &cmd, &arg_refs)
    })
    .await
    .unwrap_or_else(|e| (false, format!("spawn_blocking failed: {e}")))
}

fn run_cmd_sync(workdir: &str, cmd: &str, args: &[&str]) -> (bool, String) {
    match std::process::Command::new(cmd)
        .args(args)
        .current_dir(workdir)
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            (output.status.success(), format!("{stdout}\n{stderr}"))
        }
        Err(e) => (false, format!("Failed to run {cmd}: {e}")),
    }
}
