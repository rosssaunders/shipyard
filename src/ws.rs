use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use std::sync::Arc;

use crate::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, _state: Arc<AppState>) {
    // Send initial state
    let welcome = serde_json::json!({
        "type": "connected",
        "message": "⚓ Welcome to Shipyard"
    });
    let _ = socket
        .send(Message::Text(welcome.to_string()))
        .await;

    // Handle incoming messages
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(text) => {
                let text_str: &str = &text;
                if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(text_str) {
                    let response = handle_command(cmd).await;
                    let _ = socket
                        .send(Message::Text(response.to_string()))
                        .await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn handle_command(cmd: serde_json::Value) -> serde_json::Value {
    let cmd_type = cmd["type"].as_str().unwrap_or("unknown");

    match cmd_type {
        "ping" => serde_json::json!({ "type": "pong" }),
        "subscribe" => {
            // Subscribe to agent updates for a project
            serde_json::json!({
                "type": "subscribed",
                "project_id": cmd["project_id"]
            })
        }
        _ => serde_json::json!({
            "type": "error",
            "message": format!("Unknown command: {cmd_type}")
        }),
    }
}
