use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Project {
    pub id: String,
    pub owner: String,
    pub repo: String,
    pub default_branch: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct AddProjectRequest {
    pub owner: String,
    pub repo: String,
    pub default_branch: Option<String>,
}

pub async fn list_projects(State(state): State<Arc<AppState>>) -> Json<Vec<Project>> {
    let conn = state.db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT id, owner, repo, default_branch, created_at FROM projects ORDER BY created_at",
        )
        .unwrap();

    let projects = stmt
        .query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                owner: row.get(1)?,
                repo: row.get(2)?,
                default_branch: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Json(projects)
}

pub async fn add_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddProjectRequest>,
) -> Json<Project> {
    let id = Uuid::new_v4().to_string();
    let branch = req.default_branch.unwrap_or_else(|| "main".to_string());
    let now = chrono::Utc::now().to_rfc3339();

    state
        .db
        .conn()
        .execute(
            "INSERT INTO projects (id, owner, repo, default_branch, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, req.owner, req.repo, branch, now],
        )
        .unwrap();

    Json(Project {
        id,
        owner: req.owner,
        repo: req.repo,
        default_branch: branch,
        created_at: now,
    })
}

pub async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Option<Project>> {
    let conn = state.db.conn();
    let project = conn
        .query_row(
            "SELECT id, owner, repo, default_branch, created_at FROM projects WHERE id = ?1",
            [&id],
            |row| {
                Ok(Project {
                    id: row.get(0)?,
                    owner: row.get(1)?,
                    repo: row.get(2)?,
                    default_branch: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )
        .ok();

    Json(project)
}

// --- Skills ---

#[derive(Debug, Deserialize)]
pub struct UpdateSkillsRequest {
    pub skills: String,
}

pub async fn get_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.conn();
    let skills: String = conn
        .query_row("SELECT skills FROM projects WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .unwrap_or_default();
    Json(serde_json::json!({"skills": skills}))
}

pub async fn update_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateSkillsRequest>,
) -> Json<bool> {
    let conn = state.db.conn();
    let _ = conn.execute(
        "UPDATE projects SET skills = ?1 WHERE id = ?2",
        rusqlite::params![req.skills, id],
    );
    Json(true)
}

/// Auto-generate skills by reading the repo's AGENTS.md + key files
pub async fn generate_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let (owner, repo) = {
        let conn = state.db.conn();
        match conn.query_row(
            "SELECT owner, repo FROM projects WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            Ok(r) => r,
            Err(_) => return Json(serde_json::json!({"error": "project not found"})),
        }
    };

    let repo_path = format!(
        "{}/code/{}/{}",
        std::env::var("HOME").unwrap_or_default(),
        owner,
        repo
    );

    // Read AGENTS.md if it exists
    let agents_md = std::fs::read_to_string(format!("{repo_path}/AGENTS.md")).unwrap_or_default();
    // Read Cargo.toml for project info
    let cargo_toml = std::fs::read_to_string(format!("{repo_path}/Cargo.toml")).unwrap_or_default();
    // Read .github/workflows for CI info
    let ci_files: Vec<String> = std::fs::read_dir(format!("{repo_path}/.github/workflows"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();

    // Ask the brain to synthesize skills
    let prompt = format!(
        "Generate a project skills/knowledge document for coding agents working on {owner}/{repo}.\n\n\
        Based on these files, create a concise markdown document covering:\n\
        1. Build commands and targets (including WASM if applicable)\n\
        2. Test commands\n\
        3. Key architecture (important files, modules, patterns)\n\
        4. Common gotchas and pitfalls\n\
        5. Commit conventions\n\
        6. CI requirements\n\n\
        ## AGENTS.md\n{agents_md}\n\n\
        ## Cargo.toml (first 50 lines)\n{cargo_first}\n\n\
        ## CI workflows\n{ci_info}\n\n\
        Keep it under 2000 words. Be specific and actionable.",
        cargo_first = cargo_toml.lines().take(50).collect::<Vec<_>>().join("\n"),
        ci_info = ci_files
            .iter()
            .take(2)
            .map(|f| f.lines().take(30).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>()
            .join("\n---\n"),
    );

    match crate::brain::call_llm_pub(
        &state.config.llm_model,
        "You generate concise project knowledge documents for coding agents. Output raw markdown only, no wrapping.",
        &prompt,
    )
    .await {
        Ok(skills) => {
            // Save to DB
            let conn = state.db.conn();
            let _ = conn.execute(
                "UPDATE projects SET skills = ?1 WHERE id = ?2",
                rusqlite::params![skills, id],
            );
            Json(serde_json::json!({"ok": true, "skills": skills}))
        }
        Err(e) => Json(serde_json::json!({"error": format!("{e}")})),
    }
}

pub async fn list_issues(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Vec<serde_json::Value>> {
    // Get project owner/repo from DB
    let (owner, repo) = {
        let conn = state.db.conn();
        match conn.query_row(
            "SELECT owner, repo FROM projects WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            Ok(r) => r,
            Err(_) => return Json(vec![]),
        }
    };

    // Use gh CLI to fetch issues (authenticated already)
    let output = std::process::Command::new("gh")
        .args([
            "issue",
            "list",
            "--repo",
            &format!("{owner}/{repo}"),
            "--state",
            "open",
            "--limit",
            "50",
            "--json",
            "number,title,labels,assignees,state,createdAt,updatedAt,milestone",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let json: Vec<serde_json::Value> =
                serde_json::from_slice(&out.stdout).unwrap_or_default();
            Json(json)
        }
        _ => Json(vec![]),
    }
}
