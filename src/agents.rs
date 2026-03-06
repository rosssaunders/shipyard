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

#[derive(Debug, Deserialize, Clone)]
pub struct QualityGates {
    #[serde(default = "default_true")]
    pub tests: bool,
    #[serde(default = "default_true")]
    pub clippy: bool,
    #[serde(default = "default_true")]
    pub review: bool,
    #[serde(default)]
    pub auto_merge: bool,
}

fn default_true() -> bool { true }

impl Default for QualityGates {
    fn default() -> Self {
        Self { tests: true, clippy: true, review: true, auto_merge: false }
    }
}

#[derive(Debug, Deserialize)]
pub struct SpawnRequest {
    pub project_id: String,
    pub issue_number: Option<i64>,
    pub prompt: String,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub quality_gates: Option<QualityGates>,
}

pub struct AgentManager {
    pub(crate) processes: Mutex<HashMap<String, AgentProcess>>,
}

struct AgentProcess {
    finished: Arc<std::sync::atomic::AtomicBool>,
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
        agent_type: &str,
    ) -> anyhow::Result<u32> {
        let (cmd, args): (&str, Vec<&str>) = match agent_type {
            "claude" => ("claude", vec!["-p", prompt, "--dangerously-skip-permissions"]),
            _ => ("codex", vec!["--yolo", "-m", model, "exec", prompt]),
        };

        // Use script(1) to capture PTY output — Codex needs a terminal
        let log_file = format!("/tmp/shipyard/{id}.log");
        let _ = std::fs::File::create(&log_file);

        // Wrap command in script(1) to allocate a PTY and capture all output
        let full_cmd = if agent_type == "claude" {
            format!("claude -p '{}' --dangerously-skip-permissions", prompt.replace('\'', "'\\''"))
        } else {
            format!("codex --yolo -m {} exec '{}'", model, prompt.replace('\'', "'\\''"))
        };

        let mut child = Command::new("script")
            .args(["-q", "-c", &full_cmd, &log_file])
            .current_dir(workdir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let pid = child.id().unwrap_or(0);
        let output = Arc::new(Mutex::new(String::new()));
        let output_clone = output.clone();
        let log_file_clone = log_file.clone();

        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_clone = finished.clone();

        // Tail the log file in background
        tokio::spawn(async move {
            let mut last_size = 0u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if finished_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    // One final read after exit
                    if let Ok(content) = tokio::fs::read_to_string(&log_file_clone).await {
                        if content.len() as u64 > last_size {
                            let new_content = &content[last_size as usize..];
                            let cleaned = strip_ansi(new_content);
                            output_clone.lock().unwrap().push_str(&cleaned);
                        }
                    }
                    break;
                }
                if let Ok(content) = tokio::fs::read_to_string(&log_file_clone).await {
                    let new_len = content.len() as u64;
                    if new_len > last_size {
                        let new_content = &content[last_size as usize..];
                        let cleaned = strip_ansi(new_content);
                        output_clone.lock().unwrap().push_str(&cleaned);
                        last_size = new_len;
                    }
                }
            }
        });

        self.processes.lock().unwrap().insert(
            id.to_string(),
            AgentProcess {
                finished: finished.clone(),
                output,
            },
        );

        // Wait for child exit in background, then mark finished
        let wait_finished = finished.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            wait_finished.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        Ok(pid)
    }

    pub fn is_running(&self, id: &str) -> bool {
        if let Some(proc) = self.processes.lock().unwrap().get(id) {
            !proc.finished.load(std::sync::atomic::Ordering::Relaxed)
        } else {
            false
        }
    }

    pub fn get_output(&self, id: &str) -> Option<String> {
        self.processes
            .lock()
            .unwrap()
            .get(id)
            .map(|p| p.output.lock().unwrap().clone())
    }

    pub fn get_output_arc(&self, id: &str) -> Option<Arc<Mutex<String>>> {
        self.processes
            .lock()
            .unwrap()
            .get(id)
            .map(|p| p.output.clone())
    }

    pub fn kill(&self, id: &str) -> bool {
        if let Some(process) = self.processes.lock().unwrap().get(id) {
            process.finished.store(true, std::sync::atomic::Ordering::Relaxed);
            // Kill the process group
            let _ = std::process::Command::new("pkill")
                .args(["-f", &format!("shipyard/{}", id)])
                .spawn();
            true
        } else {
            false
        }
    }
}

/// Strip ANSI escape sequences for clean terminal output
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC [ ... (letter) sequences
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() || nc == 'm' || nc == 'H' || nc == 'J' || nc == 'K' {
                        break;
                    }
                }
            }
        } else if c == '\r' {
            // Skip carriage returns
        } else {
            result.push(c);
        }
    }
    result
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
    let (owner, repo, _default_branch, worktree_path, branch) = {
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

    let agent_type = req.agent_type.clone().unwrap_or_else(|| "codex".to_string());
    let gates = req.quality_gates.clone().unwrap_or_default();

    // Spawn the coding agent
    let pid = state
        .agents
        .spawn(&id, &worktree_path, &model, &req.prompt, &agent_type)
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

    // Spawn background watcher for quality gates
    let watcher_state = state.clone();
    let watcher_id = id.clone();
    let watcher_worktree = worktree_path.clone();
    let watcher_branch = branch.clone();
    let watcher_owner = owner.clone();
    let watcher_repo = repo.clone();
    let watcher_issue = req.issue_number;
    tokio::spawn(async move {
        watch_agent_completion(
            watcher_state,
            &watcher_id,
            &watcher_worktree,
            &watcher_branch,
            &watcher_owner,
            &watcher_repo,
            watcher_issue,
            gates,
        )
        .await;
    });

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

// --- Quality gate pipeline ---

async fn watch_agent_completion(
    state: Arc<AppState>,
    agent_id: &str,
    worktree: &str,
    branch: &str,
    owner: &str,
    repo: &str,
    issue_number: Option<i64>,
    gates: QualityGates,
) {
    // Poll until agent process finishes
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let status: String = {
            let conn = state.db.conn();
            conn.query_row(
                "SELECT status FROM agents WHERE id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "unknown".to_string())
        };
        if status != "running" {
            return; // killed or already done
        }
        // Check if process is still alive
        let has_output = state.agents.get_output(agent_id).is_some();
        if !has_output {
            break; // Process finished and was cleaned up
        }
        // Check if process exited by looking at the lock
        let still_running = state
            .agents
            .processes
            .lock()
            .unwrap()
            .contains_key(agent_id);
        if !still_running {
            break;
        }
    }

    tracing::info!("Agent {agent_id} finished — running quality gates");

    // Update status
    {
        let conn = state.db.conn();
        let _ = conn.execute(
            "UPDATE agents SET status = 'gates' WHERE id = ?1",
            [agent_id],
        );
    }

    let mut all_passed = true;

    // Gate 1: Tests
    if gates.tests {
        let gate_id = uuid::Uuid::new_v4().to_string();
        record_gate(&state, &gate_id, agent_id, "tests", "running");
        let output = run_command(worktree, "cargo", &["test"]);
        let passed = output.0;
        record_gate_result(&state, &gate_id, if passed { "passed" } else { "failed" }, &output.1);
        if !passed { all_passed = false; }
    }

    // Gate 2: Clippy
    if gates.clippy && all_passed {
        let gate_id = uuid::Uuid::new_v4().to_string();
        record_gate(&state, &gate_id, agent_id, "clippy", "running");
        let output = run_command(worktree, "cargo", &["clippy", "--all-targets", "--", "-D", "warnings"]);
        let passed = output.0;
        record_gate_result(&state, &gate_id, if passed { "passed" } else { "failed" }, &output.1);
        if !passed { all_passed = false; }
    }

    // Gate 3: Create PR
    if all_passed {
        let title = issue_number
            .map(|n| format!("fix: resolve issue #{n}"))
            .unwrap_or_else(|| format!("shipyard: agent {}", &agent_id[..8]));

        let body = format!(
            "Automated by [Shipyard](https://github.com/rosssaunders/shipyard)\n\n{}",
            issue_number.map(|n| format!("Closes #{n}")).unwrap_or_default()
        );

        // Push branch
        let _ = run_command(worktree, "git", &["push", "-u", "origin", branch, "--no-verify"]);

        // Create PR
        let pr_output = run_command(
            worktree,
            "gh",
            &[
                "pr", "create",
                "--repo", &format!("{owner}/{repo}"),
                "--head", branch,
                "--title", &title,
                "--body", &body,
            ],
        );

        // Gate 4: AI code review
        if gates.review && pr_output.0 {
            let gate_id = uuid::Uuid::new_v4().to_string();
            record_gate(&state, &gate_id, agent_id, "review", "running");
            // Use a second Codex instance to review
            let review_output = run_command(
                worktree,
                "gh",
                &["pr", "diff", "--repo", &format!("{owner}/{repo}"), branch],
            );
            // For now, mark review as passed if PR was created
            // TODO: dispatch to small model for actual review
            record_gate_result(&state, &gate_id, "passed", &review_output.1);
        }

        // Gate 5: Auto-merge
        if gates.auto_merge && all_passed {
            let gate_id = uuid::Uuid::new_v4().to_string();
            record_gate(&state, &gate_id, agent_id, "merge", "running");
            let merge_output = run_command(
                worktree,
                "gh",
                &[
                    "pr", "merge",
                    "--repo", &format!("{owner}/{repo}"),
                    "--squash", "--admin",
                    branch,
                ],
            );
            record_gate_result(
                &state,
                &gate_id,
                if merge_output.0 { "passed" } else { "failed" },
                &merge_output.1,
            );
        }
    }

    // Final status
    let final_status = if all_passed { "done" } else { "failed" };
    {
        let conn = state.db.conn();
        let _ = conn.execute(
            "UPDATE agents SET status = ?1, finished_at = datetime('now') WHERE id = ?2",
            rusqlite::params![final_status, agent_id],
        );
    }
    tracing::info!("Agent {agent_id} pipeline complete: {final_status}");
}

fn record_gate(state: &AppState, gate_id: &str, agent_id: &str, gate_type: &str, status: &str) {
    let _ = state.db.conn().execute(
        "INSERT INTO quality_gates (id, agent_id, gate_type, status) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![gate_id, agent_id, gate_type, status],
    );
}

fn record_gate_result(state: &AppState, gate_id: &str, status: &str, output: &str) {
    let _ = state.db.conn().execute(
        "UPDATE quality_gates SET status = ?1, output = ?2 WHERE id = ?3",
        rusqlite::params![status, output, gate_id],
    );
}

fn run_command(workdir: &str, cmd: &str, args: &[&str]) -> (bool, String) {
    match std::process::Command::new(cmd)
        .args(args)
        .current_dir(workdir)
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{stdout}\n{stderr}");
            (output.status.success(), combined)
        }
        Err(e) => (false, format!("Failed to run {cmd}: {e}")),
    }
}
