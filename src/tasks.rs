use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;
use crate::agents::QualityGates;

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
    let model = "gpt-5.4".to_string();
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
    let bg_text = req.text.clone();
    let bg_branch = branch.clone();
    tokio::spawn(async move {
        run_task_pipeline(
            bg_state, bg_id, owner, repo, model, agent_type,
            bg_branch, worktree_path, bg_text, issue_number, None,
            QualityGates { tests: true, clippy: true, review: true, auto_merge: true },
        ).await;
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

/// Add an event to a task's timeline
pub fn add_event(state: &AppState, task_id: &str, kind: &str, icon: &str, message: &str, detail: Option<&str>) {
    let _ = state.db.conn().execute(
        "INSERT INTO task_events (task_id, kind, icon, message, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![task_id, kind, icon, message, detail],
    );
}

pub fn update_task_status(state: &AppState, task_id: &str, status: &str) {
    let finished = if matches!(status, "done" | "failed" | "killed") {
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
    let model = req.model.unwrap_or_else(|| "gpt-5.4".to_string());
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
    let bg_owner = owner.clone();
    let bg_repo = repo.clone();
    let bg_model = model.clone();
    let bg_agent_type = agent_type.clone();
    let bg_branch = branch.clone();
    let bg_worktree = worktree_path.clone();
    let bg_title = req.title.clone();
    let bg_issue = req.issue_number;
    let bg_extra = req.extra_instructions.clone();
    let bg_gates = gates;

    tokio::spawn(async move {
        run_task_pipeline(
            bg_state, bg_id, bg_owner, bg_repo, bg_model, bg_agent_type,
            bg_branch, bg_worktree, bg_title, bg_issue, bg_extra, bg_gates,
        ).await;
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

async fn run_task_pipeline(
    state: Arc<AppState>,
    id: String,
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
) {
    let task_id = &id;
    let worktree = &worktree_path;

    // Brain planning phase
    add_event(&state, task_id, "brain", "🧠", "Brain is analyzing the task...", None);

    // Load project skills for context
    let project_skills = {
        let conn = state.db.conn();
        conn.query_row(
            "SELECT skills FROM projects p JOIN tasks t ON t.project_id = p.id WHERE t.id = ?1",
            [task_id],
            |r| r.get::<_, String>(0),
        ).unwrap_or_default()
    };
    let skills_context = if project_skills.is_empty() {
        None
    } else {
        Some(format!("## Project Knowledge (Skills)\n{project_skills}"))
    };

    let plan = crate::brain::plan_task(
        &owner,
        &repo,
        issue_number,
        &title,
        extra_instructions.as_deref(),
        &model,
        skills_context.as_deref(),
    )
    .await;

    let prompt = match &plan {
        Ok(plan) => {
            add_event(&state, task_id, "brain", "🧠",
                &format!("Complexity: {}/5 — {}", plan.complexity, plan.assessment),
                None);

            if plan.subtasks.len() > 1 {
                add_event(&state, task_id, "brain", "🧠",
                    &format!("Breaking into {} subtasks", plan.subtasks.len()),
                    Some(&plan.subtasks.iter().map(|s| format!("• {}", s.title)).collect::<Vec<_>>().join("\n")));
            }

            plan.subtasks.iter().map(|s| s.prompt.as_str()).collect::<Vec<_>>().join("\n\n---\n\n")
        }
        Err(e) => {
            add_event(&state, task_id, "brain", "⚠️",
                &format!("Brain planning failed, using basic prompt: {e}"), None);
            let mut p = if let Some(issue_num) = issue_number {
                format!("Fix issue #{issue_num}: {title}. Run `gh issue view {issue_num}` for full context. Implement the fix, ensure all tests pass, commit and push.")
            } else {
                title.clone()
            };
            if let Some(extra) = &extra_instructions {
                p.push_str(&format!("\n\nAdditional instructions: {extra}"));
            }
            p
        }
    };

    add_event(&state, task_id, "dispatch", "🚀",
        &format!("Dispatching to {} ({})", agent_type, model), None);

    // Spawn agent
    let pid = state
        .agents
        .spawn(task_id, worktree, &model, &prompt, &agent_type)
        .await
        .unwrap_or(0);

    {
        let conn = state.db.conn();
        let _ = conn.execute("UPDATE tasks SET pid = ?1 WHERE id = ?2", rusqlite::params![pid as i64, task_id]);
    }

    add_event(&state, task_id, "agent", "🔨", "Agent started working...", None);

    // Poll until agent finishes (with timeout)
    let max_wait = std::time::Duration::from_secs(12 * 3600); // 12 hour max
    let start = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let still_running = state.agents.is_running(&task_id);
        if !still_running {
            break;
        }
        if start.elapsed() > max_wait {
            add_event(&state, &task_id, "system", "⏰", "Agent timed out after 12 hours", None);
            state.agents.kill(&task_id);
            update_task_status(&state, &task_id, "failed");
            return;
        }
    }

    add_event(&state, &task_id, "agent", "🔨", "Agent finished coding", None);
    update_task_status(&state, &task_id, "gates");

    // Check if there are any commits
    let has_commits = run_cmd(&worktree, "git", &["log", "--oneline", "HEAD", "^main", "--"]);
    if !has_commits.0 || has_commits.1.trim().is_empty() {
        add_event(&state, &task_id, "error", "⚠️", "No commits produced by agent", None);
        update_task_status(&state, &task_id, "failed");
        return;
    }

    let commit_count = has_commits.1.trim().lines().count();
    add_event(&state, &task_id, "info", "📝",
        &format!("{commit_count} commit(s) ready"), None);

    let mut all_passed = true;

    // Gate: Tests
    if gates.tests {
        add_event(&state, &task_id, "gate", "🧪", "Running tests...", None);
        let result = run_cmd(&worktree, "cargo", &["test"]);
        if result.0 {
            // Extract test count from output
            let summary = result.1.lines()
                .filter(|l| l.contains("test result:"))
                .collect::<Vec<_>>()
                .join(", ");
            add_event(&state, &task_id, "gate", "✅",
                &format!("Tests passed{}", if summary.is_empty() { String::new() } else { format!(" — {summary}") }),
                Some(&result.1));
        } else {
            add_event(&state, &task_id, "gate", "❌", "Tests failed", Some(&result.1));
            all_passed = false;
        }
    }

    // Gate: Clippy
    if gates.clippy && all_passed {
        add_event(&state, &task_id, "gate", "📎", "Running clippy...", None);
        let result = run_cmd(&worktree, "cargo", &["clippy", "--all-targets", "--", "-D", "warnings"]);
        if result.0 {
            add_event(&state, &task_id, "gate", "✅", "Clippy clean", None);
        } else {
            add_event(&state, &task_id, "gate", "❌", "Clippy warnings found", Some(&result.1));
            all_passed = false;
        }
    }

    if !all_passed {
        update_task_status(&state, &task_id, "failed");
        add_event(&state, &task_id, "system", "❌", "Quality gates failed", None);
        return;
    }

    // Push and create PR
    add_event(&state, &task_id, "system", "🚀", "Pushing branch...", None);
    let push = run_cmd(&worktree, "git", &["push", "-u", "origin", &branch, "--no-verify"]);
    if !push.0 {
        add_event(&state, &task_id, "error", "❌", "Failed to push branch", Some(&push.1));
        update_task_status(&state, &task_id, "failed");
        return;
    }

    let pr_title = issue_number
        .map(|n| format!("fix: resolve issue #{n}"))
        .unwrap_or_else(|| format!("shipyard: {}", &task_id[..8]));
    let pr_body = format!(
        "Automated by [Shipyard](https://github.com/rosssaunders/shipyard)\n\n{}",
        issue_number.map(|n| format!("Closes #{n}")).unwrap_or_default()
    );

    let pr = run_cmd(&worktree, "gh", &[
        "pr", "create",
        "--repo", &format!("{owner}/{repo}"),
        "--head", &branch,
        "--title", &pr_title,
        "--body", &pr_body,
    ]);

    if pr.0 {
        let pr_url = pr.1.trim();
        add_event(&state, &task_id, "pr", "🔗", &format!("PR created: {pr_url}"), None);
    } else {
        add_event(&state, &task_id, "error", "⚠️", "Failed to create PR", Some(&pr.1));
    }

    // Dispatch reviewer agent
    if gates.review {
        add_event(&state, &task_id, "brain", "🧠", "Dispatching reviewer agent...", None);

        let review_prompt = format!(
            "You are a code reviewer. Review the changes on this branch vs main.\n\n\
            Run: git diff main...HEAD\n\n\
            Check for:\n\
            1. Does it solve the stated task?\n\
            2. Any bugs or regressions?\n\
            3. WASM compatibility (no tokio, no std::fs in WASM paths)?\n\
            4. Missing tests?\n\
            5. Code quality?\n\n\
            Run the tests: cargo test\n\n\
            At the end, create a file called REVIEW.md with:\n\
            - approved: true/false\n\
            - summary: one line\n\
            - issues: list any problems found\n\n\
            If approved, write 'approved: true' on the first line.\n\
            If not approved, write 'approved: false' and list the issues."
        );

        let reviewer_model = {
            let conn = state.db.conn();
            conn.query_row("SELECT model FROM tasks WHERE id = ?1", [task_id.as_str()], |r| r.get::<_,String>(0))
                .unwrap_or_else(|_| "gpt-5.4".to_string())
        };

        add_event(&state, &task_id, "review", "🔍", "Reviewer agent started...", None);

        // Spawn reviewer in same worktree (read-only review)
        let review_result = run_cmd_timeout(&worktree, "codex", &[
            "--yolo", "-m", &reviewer_model, "exec", &review_prompt
        ], 600); // 10 min timeout

        // Read REVIEW.md if it exists
        let review_file = format!("{worktree}/REVIEW.md");
        let review_content = std::fs::read_to_string(&review_file).unwrap_or_default();
        let _ = std::fs::remove_file(&review_file); // clean up

        let approved = review_content.to_lowercase().contains("approved: true")
            || review_content.to_lowercase().contains("approved:true");

        if !review_content.is_empty() {
            if approved {
                add_event(&state, &task_id, "review", "✅",
                    "Reviewer approved", Some(&review_content));
            } else {
                add_event(&state, &task_id, "review", "❌",
                    "Reviewer found issues", Some(&review_content));
                all_passed = false;

                // TODO: retry loop — brain dispatches builder v2 with reviewer feedback
            }
        } else {
            // No REVIEW.md — check if tests passed in reviewer output
            add_event(&state, &task_id, "review", "⚠️",
                "Reviewer didn't produce REVIEW.md — treating as approved", None);
        }
    }

    if !all_passed {
        update_task_status(&state, &task_id, "failed");
        add_event(&state, &task_id, "brain", "🧠", "Brain: review failed, not merging", None);
        // Still learn from failures — they're the most valuable lessons
        learn_from_task(&state, &task_id, &title, &model).await;
        return;
    }

    // Brain merges — gates passed + reviewer approved
    add_event(&state, &task_id, "brain", "🧠", "Brain: all gates green, reviewer approved → merging", None);

    // Always merge if gates pass (brain decides, not the user)
    {
        add_event(&state, &task_id, "system", "🔀", "Auto-merging...", None);
        let merge = run_cmd(&worktree, "gh", &[
            "pr", "merge",
            "--repo", &format!("{owner}/{repo}"),
            "--squash", "--admin", &branch,
        ]);
        if merge.0 {
            add_event(&state, &task_id, "system", "✅", "Merged to main", None);
        } else {
            add_event(&state, &task_id, "error", "⚠️", "Auto-merge failed", Some(&merge.1));
        }
    }

    // Brain learns from this task
    learn_from_task(&state, &task_id, &title, &model).await;

    update_task_status(&state, &task_id, "done");
    add_event(&state, &task_id, "system", "🏁", "Task complete", None);
}

/// Simple diff summary — show what lines were added/removed
fn diff_summary(old: &str, new: &str) -> String {
    let old_lines: std::collections::HashSet<&str> = old.lines().collect();
    let new_lines: std::collections::HashSet<&str> = new.lines().collect();
    
    let added: Vec<&&str> = new_lines.difference(&old_lines).take(10).collect();
    let removed: Vec<&&str> = old_lines.difference(&new_lines).take(10).collect();
    
    let mut summary = String::new();
    if !added.is_empty() {
        summary.push_str("Added:\n");
        for line in &added {
            summary.push_str(&format!("+ {}\n", line));
        }
    }
    if !removed.is_empty() {
        summary.push_str("Removed:\n");
        for line in &removed {
            summary.push_str(&format!("- {}\n", line));
        }
    }
    if summary.is_empty() { "Minor reformatting".to_string() } else { summary }
}

/// Brain reviews what was learned from a task and updates project skills
async fn learn_from_task(state: &Arc<AppState>, task_id: &str, title: &str, model: &str) {
    add_event(state, task_id, "brain", "🧠", "Brain: reviewing what was learned...", None);

    let events_summary = {
        let conn = state.db.conn();
        let mut stmt = conn.prepare(
            "SELECT icon, message, detail FROM task_events WHERE task_id = ?1 ORDER BY id"
        ).unwrap();
        let events: Vec<String> = stmt.query_map([task_id], |row| {
            let icon: String = row.get(0)?;
            let msg: String = row.get(1)?;
            let detail: Option<String> = row.get(2)?;
            Ok(if let Some(d) = detail {
                format!("{icon} {msg}\n  Detail: {}", d.chars().take(500).collect::<String>())
            } else {
                format!("{icon} {msg}")
            })
        }).unwrap().filter_map(|r| r.ok()).collect();
        events.join("\n")
    };

    let current_skills = {
        let conn = state.db.conn();
        conn.query_row(
            "SELECT p.skills FROM projects p JOIN tasks t ON t.project_id = p.id WHERE t.id = ?1",
            [task_id],
            |r| r.get::<_, String>(0),
        ).unwrap_or_default()
    };

    let learn_prompt = format!(
        "A coding agent just completed a task (it may have succeeded or failed). \
        Review the event timeline and update the project skills document with anything learned.\n\n\
        ## Task: {title}\n\n\
        ## Events Timeline\n{events_summary}\n\n\
        ## Current Skills Document\n{current_skills}\n\n\
        Rules:\n\
        - ADD new gotchas, patterns, or learnings discovered during this task\n\
        - Failures are especially valuable — if something went wrong, document WHY and HOW to avoid it\n\
        - KEEP everything already in the skills doc that's still relevant\n\
        - REMOVE anything proven wrong by this task\n\
        - If nothing new was learned, return the skills document UNCHANGED\n\
        - Be specific: \"use thread_local! not tokio::task_local! because WASM is single-threaded\"\n\
        - Keep it under 2000 words\n\n\
        Return ONLY the updated skills document (raw markdown, no code fences)."
    );

    match crate::brain::call_llm_pub(model,
        "You maintain a project knowledge document for coding agents. Output raw markdown only, no wrapping.",
        &learn_prompt).await
    {
        Ok(updated_skills) => {
            let changed = updated_skills.trim() != current_skills.trim();
            if changed && !updated_skills.trim().is_empty() {
                let conn = state.db.conn();
                let _ = conn.execute(
                    "UPDATE projects SET skills = ?1 WHERE id = (SELECT project_id FROM tasks WHERE id = ?2)",
                    rusqlite::params![updated_skills.trim(), task_id],
                );
                add_event(state, task_id, "brain", "📚", "Skills updated with new learnings",
                    Some(&diff_summary(&current_skills, &updated_skills)));
            } else {
                add_event(state, task_id, "brain", "📚", "No new learnings to add", None);
            }
        }
        Err(e) => {
            add_event(state, task_id, "brain", "⚠️",
                &format!("Skills update failed: {e}"), None);
        }
    }
}

fn run_cmd_timeout(workdir: &str, cmd: &str, args: &[&str], timeout_secs: u64) -> (bool, String) {
    match std::process::Command::new(cmd)
        .args(args)
        .current_dir(workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => {
            match child.wait_with_output() {
                Ok(output) => {
                    let _ = timeout_secs; // TODO: actual timeout with thread
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    (output.status.success(), format!("{stdout}\n{stderr}"))
                }
                Err(e) => (false, format!("Process error: {e}")),
            }
        }
        Err(e) => (false, format!("Failed to run {cmd}: {e}")),
    }
}

fn run_cmd(workdir: &str, cmd: &str, args: &[&str]) -> (bool, String) {
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
