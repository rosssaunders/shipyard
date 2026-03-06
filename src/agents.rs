use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Agent {
    pub id: String,
    pub project_id: String,
    pub issue_number: Option<i64>,
    pub status: String,
    pub model: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub prompt: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct SpawnRequest {
    pub project_id: String,
    pub issue_number: Option<i64>,
    pub prompt: String,
    pub model: Option<String>,
}

pub struct AgentManager {
    processes: Mutex<HashMap<String, AgentProcess>>,
}

struct AgentProcess {
    _child: Child,
    output: Arc<Mutex<String>>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }

    pub async fn spawn(
        &self,
        id: &str,
        workdir: &str,
        model: &str,
        prompt: &str,
    ) -> anyhow::Result<u32> {
        let mut child = Command::new("codex")
            .args(["--yolo", "-m", model, "exec", prompt])
            .current_dir(workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let pid = child.id().unwrap_or(0);
        let output = Arc::new(Mutex::new(String::new()));
        let output_clone = output.clone();

        // Stream stdout in background
        let mut stdout = child.stdout.take().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        output_clone.lock().unwrap().push_str(&text);
                    }
                    Err(_) => break,
                }
            }
        });

        self.processes.lock().unwrap().insert(
            id.to_string(),
            AgentProcess {
                _child: child,
                output,
            },
        );

        Ok(pid)
    }

    pub fn get_output(&self, id: &str) -> Option<String> {
        self.processes
            .lock()
            .unwrap()
            .get(id)
            .map(|p| p.output.lock().unwrap().clone())
    }

    pub fn kill(&self, id: &str) -> bool {
        if let Some(mut process) = self.processes.lock().unwrap().remove(id) {
            let _ = process._child.start_kill();
            true
        } else {
            false
        }
    }
}

// --- HTTP handlers ---

#[axum::debug_handler]
pub async fn spawn_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SpawnRequest>,
) -> Json<Agent> {
    let id = Uuid::new_v4().to_string();
    let model = req.model.unwrap_or_else(|| "gpt-5.4".to_string());
    let now = chrono::Utc::now().to_rfc3339();

    // Get project info (scope the lock to avoid holding across await)
    let (_owner, _repo, _default_branch, worktree_path, branch) = {
        let conn = state.db.conn();
        let (owner, repo, default_branch): (String, String, String) = conn
            .query_row(
                "SELECT owner, repo, default_branch FROM projects WHERE id = ?1",
                [&req.project_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

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

        // Create worktree
        let _ = std::fs::create_dir_all(&worktree_path);
        let _ = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                &branch,
                &worktree_path,
                &default_branch,
            ])
            .current_dir(&repo_path)
            .output();

        // Insert into DB
        conn.execute(
            "INSERT INTO agents (id, project_id, issue_number, status, model, worktree_path, branch, prompt, created_at)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![id, req.project_id, req.issue_number, model, worktree_path, branch, req.prompt, now],
        )
        .unwrap();

        (owner, repo, default_branch, worktree_path, branch)
    };

    // Spawn the coding agent
    let pid = state
        .agents
        .spawn(&id, &worktree_path, &model, &req.prompt)
        .await
        .unwrap_or(0);

    // Update PID
    state
        .db
        .conn()
        .execute(
            "UPDATE agents SET pid = ?1 WHERE id = ?2",
            rusqlite::params![pid, id],
        )
        .unwrap();

    Json(Agent {
        id,
        project_id: req.project_id,
        issue_number: req.issue_number,
        status: "running".to_string(),
        model,
        worktree_path: Some(worktree_path),
        branch: Some(branch),
        prompt: req.prompt,
        created_at: now,
        finished_at: None,
        exit_code: None,
    })
}

pub async fn list_agents(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> Json<Vec<Agent>> {
    let conn = state.db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT id, project_id, issue_number, status, model, worktree_path, branch, prompt, created_at, finished_at, exit_code
             FROM agents WHERE project_id = ?1 ORDER BY created_at DESC",
        )
        .unwrap();

    let agents = stmt
        .query_map([&project_id], |row| {
            Ok(Agent {
                id: row.get(0)?,
                project_id: row.get(1)?,
                issue_number: row.get(2)?,
                status: row.get(3)?,
                model: row.get(4)?,
                worktree_path: row.get(5)?,
                branch: row.get(6)?,
                prompt: row.get(7)?,
                created_at: row.get(8)?,
                finished_at: row.get(9)?,
                exit_code: row.get(10)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Json(agents)
}

pub async fn get_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Option<Agent>> {
    let conn = state.db.conn();
    let agent = conn
        .query_row(
            "SELECT id, project_id, issue_number, status, model, worktree_path, branch, prompt, created_at, finished_at, exit_code
             FROM agents WHERE id = ?1",
            [&id],
            |row| {
                Ok(Agent {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    issue_number: row.get(2)?,
                    status: row.get(3)?,
                    model: row.get(4)?,
                    worktree_path: row.get(5)?,
                    branch: row.get(6)?,
                    prompt: row.get(7)?,
                    created_at: row.get(8)?,
                    finished_at: row.get(9)?,
                    exit_code: row.get(10)?,
                })
            },
        )
        .ok();

    Json(agent)
}

pub async fn get_agent_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> String {
    state
        .agents
        .get_output(&id)
        .unwrap_or_else(|| "(no output)".to_string())
}

pub async fn kill_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<bool> {
    let killed = state.agents.kill(&id);
    if killed {
        let _ = state.db.conn().execute(
            "UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?1",
            [&id],
        );
    }
    Json(killed)
}
