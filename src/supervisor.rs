//! Layer 1: Shipyard Brain (Supervisor)
//!
//! The CTO layer. Oversees all tasks, steps in on failures,
//! patches problems, learns from outcomes.
//!
//! All external commands run via spawn_blocking to avoid starving
//! the tokio runtime (which would hang the HTTP server).

use std::sync::Arc;
use crate::AppState;
use crate::tasks::{add_event, run_cmd_async, run_cmd_timeout_async};

/// Run quality gates from project skills (not hardcoded)
pub async fn run_skill_gates(
    state: &Arc<AppState>,
    task_id: &str,
    worktree: &str,
) -> Vec<GateResult> {
    let skills = load_project_skills(state, task_id);
    let gates = parse_gates_from_skills(&skills);

    let mut results = Vec::new();

    for gate in &gates {
        add_event(state, task_id, "gate", "🧪", &format!("Running: {}", gate.name), None);

        let parts: Vec<&str> = gate.command.split_whitespace().collect();
        if parts.is_empty() { continue; }

        let (cmd, args) = (parts[0], &parts[1..]);
        let result = run_cmd_async(worktree, cmd, args).await;

        if result.0 {
            add_event(state, task_id, "gate", "✅", &format!("{} passed", gate.name), None);
            results.push(GateResult { name: gate.name.clone(), passed: true, output: result.1 });
        } else {
            add_event(state, task_id, "gate", "❌", &format!("{} failed", gate.name), Some(&result.1));
            results.push(GateResult { name: gate.name.clone(), passed: false, output: result.1 });
        }
    }

    results
}

/// Layer 1 intervention: attempt to fix failed gates
pub async fn attempt_fix(
    state: &Arc<AppState>,
    task_id: &str,
    worktree: &str,
    failures: &[GateResult],
    model: &str,
    attempt: u32,
) -> bool {
    if attempt > 3 {
        add_event(state, task_id, "supervisor", "💀",
            "Supervisor: giving up after 3 fix attempts", None);
        return false;
    }

    add_event(state, task_id, "supervisor", "🔧",
        &format!("Supervisor: attempting fix (attempt {})", attempt), None);

    // Build a targeted fix prompt from the failures
    let error_context: String = failures.iter()
        .filter(|f| !f.passed)
        .map(|f| format!("## {} FAILED\n```\n{}\n```", f.name, 
            f.output.chars().take(2000).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n\n");

    let fix_prompt = format!(
        "The following quality gates failed. Fix the errors WITHOUT changing the feature logic.\n\
        Focus ONLY on making the gates pass.\n\n\
        {error_context}\n\n\
        Common fixes:\n\
        - WASM build failures: add #[cfg(not(target_arch = \"wasm32\"))] gates\n\
        - Clippy warnings: fix the specific lint\n\
        - Test failures: fix the test or the code it tests\n\n\
        After fixing, run the failing commands to verify.\n\
        Commit your fix with message: fix: resolve gate failures"
    );

    // Spawn fixer agent (10 min timeout, non-blocking)
    let fix_result = run_cmd_timeout_async(
        worktree, "codex",
        &["--yolo", "-m", model, "exec", &fix_prompt],
        600,
    ).await;

    // Auto-commit if fixer left uncommitted changes
    let status = run_cmd_async(worktree, "git", &["status", "--porcelain"]).await;
    if status.0 && !status.1.trim().is_empty() {
        let _ = run_cmd_async(worktree, "git", &["add", "-A"]).await;
        let _ = run_cmd_async(worktree, "git", &["commit", "-m", 
            &format!("fix: resolve gate failures (attempt {})", attempt)]).await;
    }

    if fix_result.0 {
        add_event(state, task_id, "supervisor", "🔧", "Fixer agent completed", None);
        true
    } else {
        add_event(state, task_id, "supervisor", "⚠️", "Fixer agent failed", None);
        false
    }
}

// --- Types ---

pub struct Gate {
    pub name: String,
    pub command: String,
}

pub struct GateResult {
    pub name: String,
    pub passed: bool,
    pub output: String,
}

// --- Helpers ---

fn load_project_skills(state: &Arc<AppState>, task_id: &str) -> String {
    let conn = state.db.conn();
    conn.query_row(
        "SELECT p.skills FROM projects p JOIN tasks t ON t.project_id = p.id WHERE t.id = ?1",
        [task_id],
        |r| r.get::<_, String>(0),
    ).unwrap_or_default()
}

/// Parse quality gates from skills markdown
fn parse_gates_from_skills(skills: &str) -> Vec<Gate> {
    let mut gates = Vec::new();
    let mut in_gates_section = false;

    for line in skills.lines() {
        let trimmed = line.trim();
        if trimmed.to_lowercase().contains("quality gate") || trimmed.to_lowercase().contains("## gates") {
            in_gates_section = true;
            continue;
        }
        if in_gates_section {
            if trimmed.starts_with("## ") || trimmed.starts_with("# ") {
                break;
            }
            if let Some(cmd) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
                let cmd = cmd.trim();
                if !cmd.is_empty() {
                    let name = if cmd.len() > 40 {
                        format!("{}...", &cmd[..37])
                    } else {
                        cmd.to_string()
                    };
                    gates.push(Gate { name, command: cmd.to_string() });
                }
            }
        }
    }

    if gates.is_empty() {
        gates.push(Gate { name: "Tests".into(), command: "cargo test".into() });
        gates.push(Gate { name: "Clippy".into(), command: "cargo clippy --all-targets -- -D warnings".into() });
    }

    gates
}
