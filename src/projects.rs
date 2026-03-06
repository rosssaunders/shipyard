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
        .prepare("SELECT id, owner, repo, default_branch, created_at FROM projects ORDER BY created_at")
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
            "issue", "list",
            "--repo", &format!("{owner}/{repo}"),
            "--state", "open",
            "--limit", "50",
            "--json", "number,title,labels,assignees,state,createdAt,updatedAt,milestone",
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
