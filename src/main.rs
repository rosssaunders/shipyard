mod db;
mod agents;
mod brain;
mod projects;
mod supervisor;
mod tasks;
mod ws;

use axum::{
    Router,
    routing::{get, post},
};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;
use std::sync::Arc;

pub struct AppState {
    pub db: db::Database,
    pub agents: agents::AgentManager,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let db = db::Database::open("shipyard.db")?;
    db.migrate()?;

    let state = Arc::new(AppState {
        db,
        agents: agents::AgentManager::new(),
    });

    let api = Router::new()
        // New intent-driven API
        .route("/api/intent", post(tasks::submit_intent))
        .route("/api/feed", get(tasks::get_feed))
        .route("/api/attention", get(tasks::get_attention))
        .route("/api/attention/:id", post(tasks::resolve_attention))
        // Projects
        .route("/api/projects", get(projects::list_projects))
        .route("/api/projects", post(projects::add_project))
        .route("/api/projects/:id", get(projects::get_project))
        .route("/api/projects/:id/issues", get(projects::list_issues))
        .route("/api/projects/:id/skills", get(projects::get_skills))
        .route("/api/projects/:id/skills", post(projects::update_skills))
        .route("/api/projects/:id/skills/generate", post(projects::generate_skills))
        .route("/api/projects/:id/agents", get(agents::list_agents))
        // Live output
        .route("/api/tasks/:id/output", get(tasks::get_live_output))
        // Tasks
        .route("/api/projects/:id/tasks", get(tasks::list_tasks))
        .route("/api/tasks", post(tasks::create_task))
        .route("/api/tasks/:id", get(tasks::get_task))
        .route("/api/tasks/:id/kill", post(tasks::kill_task))
        // Legacy agents
        .route("/api/agents/spawn", post(agents::spawn_agent))
        .route("/api/agents/:id", get(agents::get_agent))
        .route("/api/agents/:id/kill", post(agents::kill_agent))
        .route("/api/agents/:id/logs", get(agents::get_agent_logs))
        // WebSocket
        .route("/ws", get(ws::ws_handler))
        .with_state(state.clone());

    let app = api
        .fallback_service(ServeDir::new("www"));

    let addr = "0.0.0.0:3000";
    tracing::info!("⚓ Shipyard listening on {addr}");
    tracing::info!("  Open on phone: http://localhost:3000");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
