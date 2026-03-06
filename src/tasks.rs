use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;
use crate::agents::QualityGates;

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
            rusqlite::params![id, req.project_id, req.issue_number, req.title, agent_type, model, worktree_path, branch, now, gates.auto_merge as i32],
        ).unwrap();
    }

    // Brain planning phase
    add_event(&state, &id, "brain", "🧠", "Brain is analyzing the task...", None);

    let plan = crate::brain::plan_task(
        &owner,
        &repo,
        req.issue_number,
        &req.title,
        req.extra_instructions.as_deref(),
        &model,
        None, // TODO: load project-specific context
    )
    .await;

    let prompt = match &plan {
        Ok(plan) => {
            add_event(&state, &id, "brain", "🧠",
                &format!("Complexity: {}/5 — {}", plan.complexity, plan.assessment),
                None);

            if plan.subtasks.len() > 1 {
                add_event(&state, &id, "brain", "🧠",
                    &format!("Breaking into {} subtasks", plan.subtasks.len()),
                    Some(&plan.subtasks.iter().map(|s| format!("• {}", s.title)).collect::<Vec<_>>().join("\n")));
            }

            // For now, concatenate all subtask prompts (TODO: sequential dispatch)
            plan.subtasks.iter().map(|s| s.prompt.as_str()).collect::<Vec<_>>().join("\n\n---\n\n")
        }
        Err(e) => {
            add_event(&state, &id, "brain", "⚠️",
                &format!("Brain planning failed, using basic prompt: {e}"), None);
            // Fallback to basic prompt
            let mut p = if let Some(issue_num) = req.issue_number {
                format!("Fix issue #{issue_num}: {}. Run `gh issue view {issue_num}` for full context. Implement the fix, ensure all tests pass, commit and push.", req.title)
            } else {
                req.title.clone()
            };
            if let Some(extra) = &req.extra_instructions {
                p.push_str(&format!("\n\nAdditional instructions: {extra}"));
            }
            p
        }
    };

    add_event(&state, &id, "dispatch", "🚀",
        &format!("Dispatching to {} ({})", agent_type, model), None);

    // Spawn agent
    let pid = state
        .agents
        .spawn(&id, &worktree_path, &model, &prompt, &agent_type)
        .await
        .unwrap_or(0);

    {
        let conn = state.db.conn();
        let _ = conn.execute("UPDATE tasks SET pid = ?1 WHERE id = ?2", rusqlite::params![pid, id]);
    }

    add_event(&state, &id, "agent", "🔨", "Agent started working...", None);

    // Background watcher
    let watcher_state = state.clone();
    let watcher_id = id.clone();
    let watcher_worktree = worktree_path.clone();
    let watcher_branch = branch.clone();
    tokio::spawn(async move {
        watch_task(
            watcher_state,
            watcher_id,
            watcher_worktree,
            watcher_branch,
            owner,
            repo,
            req.issue_number,
            gates,
        )
        .await;
    });

    Json(Task {
        id,
        project_id: req.project_id,
        issue_number: req.issue_number,
        title: req.title,
        status: "running".to_string(),
        agent_type,
        model,
        created_at: now,
        finished_at: None,
        events: vec![],
    })
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

// --- Background task watcher ---

async fn watch_task(
    state: Arc<AppState>,
    task_id: String,
    worktree: String,
    branch: String,
    owner: String,
    repo: String,
    issue_number: Option<i64>,
    gates: QualityGates,
) {
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

    // Brain review
    if gates.review {
        add_event(&state, &task_id, "brain", "🧠", "Brain is reviewing the diff...", None);
        let diff_output = run_cmd(&worktree, "git", &["diff", "HEAD~1"]);
        if diff_output.0 && !diff_output.1.trim().is_empty() {
            let model = {
                let conn = state.db.conn();
                conn.query_row("SELECT model FROM tasks WHERE id = ?1", [task_id.as_str()], |r| r.get::<_,String>(0))
                    .unwrap_or_else(|_| "gpt-5.4".to_string())
            };
            match crate::brain::review_diff(&diff_output.1, &task_id, &model, None).await {
                Ok(review) => {
                    if review.approved {
                        add_event(&state, &task_id, "brain", "✅",
                            &format!("Review approved: {}", review.summary), None);
                    } else {
                        add_event(&state, &task_id, "brain", "❌",
                            &format!("Review rejected: {}", review.summary),
                            Some(&review.issues.join("\n")));
                        all_passed = false;
                    }
                }
                Err(e) => {
                    add_event(&state, &task_id, "brain", "⚠️",
                        &format!("Brain review error: {e}"), None);
                }
            }
        }
    }

    // Auto-merge
    if gates.auto_merge && all_passed {
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

    update_task_status(&state, &task_id, "done");
    add_event(&state, &task_id, "system", "🏁", "Task complete", None);
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
