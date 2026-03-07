//! Parses raw Codex/Claude output into structured stage events.
//! Runs in background, tailing the output buffer and emitting task_events.

use std::sync::Arc;
use crate::AppState;
use crate::tasks::add_event;

/// Stages the agent goes through
#[derive(Debug, Clone, PartialEq)]
enum Stage {
    Reading,
    Thinking,
    Writing,
    Running,
    Committing,
    Unknown,
}

struct ParserState {
    current_stage: Stage,
    files_read: Vec<String>,
    files_written: Vec<String>,
}

/// Spawn a background task that parses agent output into structured events
pub fn spawn_log_parser(
    state: Arc<AppState>,
    task_id: String,
    output: Arc<std::sync::Mutex<String>>,
) {
    tokio::spawn(async move {
        let mut parser = ParserState {
            current_stage: Stage::Unknown,
            files_read: Vec::new(),
            files_written: Vec::new(),
        };
        let mut last_len = 0usize;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let content = {
                let guard = output.lock().unwrap();
                if guard.len() == last_len {
                    // Check if agent is done (no new output for a while)
                    continue;
                }
                let new = guard[last_len..].to_string();
                last_len = guard.len();
                new
            };

            // Parse new output lines
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }

                // Detect stage transitions
                if is_reading_line(trimmed) {
                    if let Some(file) = extract_filename(trimmed)
                        && !parser.files_read.contains(&file)
                    {
                        parser.files_read.push(file.clone());
                        if parser.current_stage != Stage::Reading {
                            parser.current_stage = Stage::Reading;
                            add_event(&state, &task_id, "stage", "📖",
                                "Reading codebase...", None);
                        }
                        // Batch file reads — emit every 3 files
                        if parser.files_read.len().is_multiple_of(3) {
                            let recent: Vec<_> = parser.files_read.iter()
                                .rev().take(3).collect();
                            add_event(&state, &task_id, "stage", "📖",
                                &format!("Read {} files", parser.files_read.len()),
                                Some(&recent.iter().map(|f| format!("  {f}")).collect::<Vec<_>>().join("\n")));
                        }
                    }
                } else if is_thinking_line(trimmed) {
                    if parser.current_stage != Stage::Thinking {
                        parser.current_stage = Stage::Thinking;
                        // Extract the thinking content
                        let thought = extract_thought(trimmed);
                        add_event(&state, &task_id, "stage", "🤔",
                            &format!("Thinking: {}", truncate(&thought, 100)), None);
                    }
                } else if is_writing_line(trimmed) {
                    if let Some(file) = extract_written_filename(trimmed)
                        && !parser.files_written.contains(&file)
                    {
                        parser.files_written.push(file.clone());
                        parser.current_stage = Stage::Writing;
                        add_event(&state, &task_id, "stage", "✏️",
                            &format!("Writing {file}"), None);
                    }
                } else if is_running_line(trimmed) {
                    if parser.current_stage != Stage::Running {
                        parser.current_stage = Stage::Running;
                        let cmd = extract_command(trimmed);
                        add_event(&state, &task_id, "stage", "⚡",
                            &format!("Running: {}", truncate(&cmd, 80)), None);
                    }
                } else if is_commit_line(trimmed) {
                    parser.current_stage = Stage::Committing;
                    add_event(&state, &task_id, "stage", "📝",
                        &format!("Committing: {}", truncate(trimmed, 80)), None);
                } else if is_test_result_line(trimmed) {
                    add_event(&state, &task_id, "stage", "🧪",
                        &format!("Test result: {}", truncate(trimmed, 100)), None);
                } else if is_error_line(trimmed) {
                    add_event(&state, &task_id, "stage", "⚠️",
                        &format!("Error: {}", truncate(trimmed, 100)), None);
                }
            }

            // Check if output has stopped growing (agent might be done)
            // The main task watcher handles actual completion detection
        }
    });
}

// --- Pattern matchers ---

fn is_reading_line(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("reading ") || lower.contains("read ") ||
    lower.contains("inspecting ") || lower.contains("scanning ") ||
    lower.contains("opening ") || lower.contains("looking at ") ||
    (lower.contains("cat ") && lower.contains(".rs")) ||
    lower.starts_with("```") && lower.contains("/src/")
}

fn is_thinking_line(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("thinking") || lower.contains("analyzing") ||
    lower.contains("planning") || lower.contains("considering") ||
    lower.contains("i need to") || lower.contains("i'll ") ||
    lower.contains("let me") || lower.contains("the approach") ||
    lower.contains("strategy")
}

fn is_writing_line(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("writing ") || lower.contains("creating ") ||
    lower.contains("modifying ") || lower.contains("updating ") ||
    lower.contains("wrote ") || lower.contains("created ") ||
    lower.contains("added ") || lower.contains("modified ")
}

fn is_running_line(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.starts_with("$ ") || lower.starts_with("> ") ||
    lower.contains("cargo test") || lower.contains("cargo clippy") ||
    lower.contains("cargo build") || lower.contains("npm ") ||
    lower.contains("running ") || lower.contains("executing ")
}

fn is_commit_line(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("commit") && (lower.contains("feat") || lower.contains("fix") ||
    lower.contains("docs") || lower.contains("chore") || lower.contains("perf"))
}

fn is_test_result_line(s: &str) -> bool {
    s.contains("test result:") || s.contains("passed") && s.contains("failed")
}

fn is_error_line(s: &str) -> bool {
    let lower = s.to_lowercase();
    (lower.starts_with("error") || lower.contains("error[e")) &&
    !lower.contains("error handling") && !lower.contains("error message")
}

fn extract_filename(s: &str) -> Option<String> {
    // Look for file paths like src/foo/bar.rs
    for word in s.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-');
        if (clean.contains('/') && clean.contains('.')) || clean.ends_with(".rs") || clean.ends_with(".ts") || clean.ends_with(".toml") {
            return Some(clean.to_string());
        }
    }
    None
}

fn extract_written_filename(s: &str) -> Option<String> {
    extract_filename(s)
}

fn extract_thought(s: &str) -> String {
    s.to_string()
}

fn extract_command(s: &str) -> String {
    s.trim_start_matches("$ ").trim_start_matches("> ").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max])
    } else {
        s.to_string()
    }
}
