//! Layer 1: Shipyard Brain (Supervisor)
//!
//! The CTO layer. Oversees all tasks, steps in on failures,
//! patches problems, learns from outcomes.

use std::sync::Arc;
use crate::AppState;
use crate::tasks::{add_event, update_task_status, run_cmd};

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
        let result = run_cmd(worktree, cmd, args);

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

    // Spawn fixer agent
    let fix_result = crate::tasks::run_cmd_timeout_pub(
        worktree, "codex",
        &["--yolo", "-m", model, "exec", &fix_prompt],
        600, // 10 min for fixes
    );

    // Auto-commit if fixer left uncommitted changes
    let status = run_cmd(worktree, "git", &["status", "--porcelain"]);
    if status.0 && !status.1.trim().is_empty() {
        let _ = run_cmd(worktree, "git", &["add", "-A"]);
        let _ = run_cmd(worktree, "git", &["commit", "-m", 
            &format!("fix: resolve gate failures (attempt {})", attempt)]);
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
/// Looks for a section like:
/// ## Quality Gates
/// - cargo test --release
/// - cargo clippy --all-targets -- -D warnings
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
                break; // Next section
            }
            if let Some(cmd) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
                let cmd = cmd.trim();
                if !cmd.is_empty() {
                    // Use the command itself as the name, shortened
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

    // Fallback: if no gates in skills, use defaults
    if gates.is_empty() {
        gates.push(Gate { name: "Tests".into(), command: "cargo test".into() });
        gates.push(Gate { name: "Clippy".into(), command: "cargo clippy --all-targets -- -D warnings".into() });
    }

    gates
}
