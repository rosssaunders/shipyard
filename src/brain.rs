use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::recon::ReconReport;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TaskPlan {
    pub assessment: String,
    pub complexity: u8,
    pub prompt: String,
    pub skip_reason: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReviewResult {
    pub approved: bool,
    pub summary: String,
    pub issues: Vec<String>,
    pub suggestion: Option<String>,
}

pub async fn plan_task(
    recon: &ReconReport,
    knowledge: &str,
    model: &str,
) -> Result<TaskPlan> {
    let recon_json = serde_json::to_string_pretty(recon)
        .context("failed to serialize recon report")?;

    let system_prompt = format!(
        r#"You are Shipyard Brain V2, an AI engineering manager that plans work for coding agents.

You will receive:
- A full recon report gathered before planning
- Project knowledge and recent task history

Your job:
1. Decide whether the task should be skipped because it is already fixed, already in flight, or lacks enough context.
2. Write one concrete execution prompt for a coding agent.
3. The prompt must reference specific file paths from recon, exact test commands, and likely gotchas.
4. Use the recon report directly. Do not ask the coding agent to repeat recon unless information is missing.
5. If recon suggests the issue may already be fixed, set `skip_reason`.
6. Choose a realistic timeout in seconds.

Planning input:
## Recon Report
{recon_json}

## Project Knowledge And Recent Task History
{knowledge}

Return JSON only with this schema:
{{
  "assessment": "short task assessment",
  "complexity": 1,
  "prompt": "specific execution prompt for one coding agent",
  "skip_reason": null,
  "timeout_secs": 3600
}}

Rules for `prompt`:
- Name the files or directories to inspect first.
- Include the exact verification commands.
- Mention branch/PR context if relevant.
- Mention concrete gotchas from knowledge or recon.
- Keep it actionable and repo-specific.
"#
    );

    let response = call_llm_json(model, &system_prompt, "Plan the task.").await?;
    let mut plan: TaskPlan = parse_json_response(&response)?;
    plan.complexity = plan.complexity.clamp(1, 5);
    if plan.timeout_secs == 0 {
        plan.timeout_secs = 3600;
    }
    Ok(plan)
}

pub async fn review_diff(
    diff: &str,
    recon: &ReconReport,
    knowledge: &str,
    model: &str,
) -> Result<ReviewResult> {
    let recon_json = serde_json::to_string_pretty(recon)
        .context("failed to serialize recon report")?;

    let system_prompt = format!(
        r#"You are Shipyard Brain V2 acting as a reviewer.

Review the diff against:
- The recon report
- Project knowledge and recent task history

Check:
1. Does the diff address the issue described in recon?
2. Does it introduce regressions or miss obvious edge cases?
3. Are tests or validation steps missing?
4. Does it violate project-specific gotchas or patterns?

## Recon Report
{recon_json}

## Project Knowledge And Recent Task History
{knowledge}

Return JSON only:
{{
  "approved": true,
  "summary": "short summary",
  "issues": ["issue"],
  "suggestion": "optional next step"
}}
"#
    );

    let user_prompt = format!("Review this diff:\n```diff\n{diff}\n```");
    let response = call_llm_json(model, &system_prompt, &user_prompt).await?;
    parse_json_response(&response)
}

pub async fn extract_learnings(
    task_id: &str,
    outcome: &str,
    diff: &str,
    model: &str,
) -> Result<String> {
    let system_prompt = r#"You extract durable project learnings for future coding agents.

Return concise markdown only. Focus on:
- validated repo-specific patterns
- failure modes and how to avoid them
- useful test or verification commands

If there is nothing durable to save, return an empty string."#;

    let user_prompt = format!(
        "Task ID: {task_id}\nOutcome: {outcome}\n\nDiff:\n```diff\n{diff}\n```"
    );

    let response = call_llm(model, system_prompt, &user_prompt, None).await?;
    Ok(response.trim().to_string())
}

pub async fn call_llm_pub(model: &str, system: &str, user: &str) -> Result<String> {
    call_llm(model, system, user, None).await
}

async fn call_llm_json(model: &str, system: &str, user: &str) -> Result<String> {
    call_llm(
        model,
        system,
        user,
        Some(json!({ "type": "json_object" })),
    )
    .await
}

async fn call_llm(
    model: &str,
    system: &str,
    user: &str,
    response_format: Option<serde_json::Value>,
) -> Result<String> {
    let endpoint = llm_endpoint();
    let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
    let model = resolved_model(model);
    let api_key = std::env::var("SHIPYARD_API_KEY").unwrap_or_default();

    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });

    if let Some(format) = response_format {
        body["response_format"] = format;
    }

    let client = reqwest::Client::new();
    let mut request = client
        .post(&url)
        .header("Content-Type", "application/json");

    if !api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }

    let response = request
        .json(&body)
        .send()
        .await
        .with_context(|| format!("failed to call LLM endpoint {url}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(anyhow!("LLM request failed with status {status}: {text}"));
    }

    let json: serde_json::Value =
        serde_json::from_str(&text).context("failed to parse LLM response body")?;

    extract_message_content(&json).ok_or_else(|| {
        warn!(response = %text, "LLM response missing message content");
        anyhow!("LLM response did not contain message content")
    })
}

fn parse_json_response<T>(response: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let cleaned = response
        .trim()
        .strip_prefix("```json")
        .or_else(|| response.trim().strip_prefix("```"))
        .unwrap_or(response)
        .trim()
        .strip_suffix("```")
        .unwrap_or(response)
        .trim();

    let candidate = if let Some(start) = cleaned.find('{') {
        if let Some(end) = cleaned.rfind('}') {
            &cleaned[start..=end]
        } else {
            cleaned
        }
    } else {
        cleaned
    };

    serde_json::from_str(candidate).context("failed to parse structured LLM JSON response")
}

fn extract_message_content(value: &serde_json::Value) -> Option<String> {
    let content = &value["choices"][0]["message"]["content"];
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }

    content.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn llm_endpoint() -> String {
    std::env::var("SHIPYARD_LLM_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:3000/v1".to_string())
}

fn resolved_model(model: &str) -> String {
    if model.trim().is_empty() {
        std::env::var("SHIPYARD_LLM_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4.5".to_string())
    } else {
        model.to_string()
    }
}
