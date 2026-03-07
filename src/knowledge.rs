use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::warn;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskRecord {
    pub task_id: String,
    pub title: String,
    pub issue_number: Option<i64>,
    pub outcome: String,
    pub summary: String,
    pub created_at: String,
}

pub struct KnowledgeStore {
    base_dir: PathBuf,
}

impl KnowledgeStore {
    pub fn new() -> Self {
        let base_dir = Config::from_env().data_dir;
        Self { base_dir }
    }

    pub fn load_knowledge(&self, owner: &str, repo: &str) -> String {
        let path = self.project_dir(owner, repo).join("knowledge.md");
        match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                warn!(file = %path.display(), error = %err, "failed to load knowledge");
                String::new()
            }
        }
    }

    pub fn append_knowledge(&self, owner: &str, repo: &str, entry: &str) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }

        let project_dir = self.ensure_project_dir(owner, repo);
        let path = project_dir.join("knowledge.md");
        let mut current = fs::read_to_string(&path).unwrap_or_default();

        if !current.trim().is_empty() && !current.ends_with("\n\n") {
            current.push_str("\n\n");
        }

        current.push_str(&format!(
            "## {}\n{}\n",
            Utc::now().to_rfc3339(),
            trimmed
        ));

        if let Err(err) = fs::write(&path, current) {
            warn!(file = %path.display(), error = %err, "failed to append knowledge");
        }
    }

    pub fn record_task(&self, owner: &str, repo: &str, task: &TaskRecord) {
        let project_dir = self.ensure_project_dir(owner, repo);
        let path = project_dir.join("history.json");
        let mut history = self.read_history_file(&path);
        history.push(task.clone());

        if let Ok(json) = serde_json::to_string_pretty(&history) {
            if let Err(err) = fs::write(&path, json) {
                warn!(file = %path.display(), error = %err, "failed to write task history");
            }
        } else {
            warn!(file = %path.display(), "failed to serialize task history");
        }
    }

    pub fn recent_history(&self, owner: &str, repo: &str, limit: usize) -> Vec<TaskRecord> {
        let path = self.project_dir(owner, repo).join("history.json");
        let history = self.read_history_file(&path);
        if limit == 0 {
            return Vec::new();
        }

        history
            .into_iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    fn project_dir(&self, owner: &str, repo: &str) -> PathBuf {
        self.base_dir
            .join("projects")
            .join(format!("{}_{}", sanitize_segment(owner), sanitize_segment(repo)))
    }

    fn ensure_project_dir(&self, owner: &str, repo: &str) -> PathBuf {
        let path = self.project_dir(owner, repo);
        if let Err(err) = fs::create_dir_all(&path) {
            warn!(dir = %path.display(), error = %err, "failed to create knowledge directory");
        }
        path
    }

    fn read_history_file(&self, path: &PathBuf) -> Vec<TaskRecord> {
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|err| {
                warn!(file = %path.display(), error = %err, "failed to parse task history");
                Vec::new()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                warn!(file = %path.display(), error = %err, "failed to read task history");
                Vec::new()
            }
        }
    }
}

fn sanitize_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' })
        .collect()
}
