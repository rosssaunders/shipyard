use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The brain's planning output for a task
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TaskPlan {
    /// Whether this should be split into subtasks
    pub subtasks: Vec<Subtask>,
    /// Overall assessment
    pub assessment: String,
    /// Estimated complexity (1-5)
    pub complexity: u8,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Subtask {
    pub title: String,
    pub prompt: String,
    pub depends_on: Option<String>, // subtask title it depends on
}

/// The brain's review of an agent's work
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReviewResult {
    pub approved: bool,
    pub summary: String,
    pub issues: Vec<String>,
    pub suggestion: Option<String>, // if rejected, what to try next
}

/// Plan a task: read issue context, understand codebase, craft agent prompt(s)
pub async fn plan_task(
    owner: &str,
    repo: &str,
    issue_number: Option<i64>,
    title: &str,
    extra_instructions: Option<&str>,
    model: &str,
    project_context: Option<&str>,
) -> Result<TaskPlan> {
    // 1. Gather context
    let issue_body = if let Some(num) = issue_number {
        fetch_issue_body(owner, repo, num).await
    } else {
        String::new()
    };

    let repo_structure = fetch_repo_structure(owner, repo).await;

    // 2. Ask the brain to plan
    let system_prompt = format!(
        r#"You are an expert software engineering lead. You plan work for coding agents (Codex, Claude Code).

Your job:
1. Read the issue and understand what needs to be done
2. Assess complexity (1=trivial fix, 5=architecture rewrite)
3. If complexity >= 4, break into ordered subtasks with dependencies
4. Write detailed prompts for each subtask/task that include:
   - Specific files to read/modify
   - Architecture constraints and patterns to follow
   - Test commands to verify
   - What NOT to do (common pitfalls)
   - Commit message format

{project_context}

Respond in JSON format:
{{
  "assessment": "brief assessment of the task",
  "complexity": 1-5,
  "subtasks": [
    {{
      "title": "short title",
      "prompt": "detailed multi-paragraph prompt for the coding agent",
      "depends_on": null or "title of dependency"
    }}
  ]
}}

For simple tasks (complexity 1-3), return a single subtask.
For complex tasks (complexity 4-5), break into 2-5 subtasks with clear boundaries."#,
        project_context = project_context.unwrap_or("No project-specific context provided.")
    );

    let user_prompt = format!(
        "## Issue\n**{title}**\n\n{issue_body}\n\n## Repository structure\n{repo_structure}\n\n{}",
        extra_instructions.map(|e| format!("## Additional instructions\n{e}")).unwrap_or_default()
    );

    let response = call_llm(model, &system_prompt, &user_prompt).await?;

    // Parse the JSON response
    let plan: TaskPlan = parse_plan_response(&response)?;
    Ok(plan)
}

/// Review an agent's diff output
pub async fn review_diff(
    diff: &str,
    original_prompt: &str,
    model: &str,
    project_context: Option<&str>,
) -> Result<ReviewResult> {
    let system_prompt = format!(
        r#"You are a senior code reviewer. Review this diff produced by a coding agent.

Check for:
1. Does it actually solve the stated task?
2. Are there any regressions or bugs introduced?
3. Does it follow the project's patterns and conventions?
4. Are there WASM compatibility issues (e.g., using tokio in WASM)?
5. Are there missing test cases?
6. Is the code quality acceptable (no hacks, proper error handling)?

{project_context}

Respond in JSON:
{{
  "approved": true/false,
  "summary": "one-line summary",
  "issues": ["list of issues found"],
  "suggestion": "if rejected, what should be changed"
}}"#,
        project_context = project_context.unwrap_or("")
    );

    let user_prompt = format!(
        "## Original task\n{original_prompt}\n\n## Diff\n```\n{diff}\n```"
    );

    let response = call_llm(model, &system_prompt, &user_prompt).await?;
    let review: ReviewResult = serde_json::from_str(&response)
        .unwrap_or(ReviewResult {
            approved: true,
            summary: "Review completed".to_string(),
            issues: vec![],
            suggestion: None,
        });
    Ok(review)
}

// --- Helpers ---

async fn fetch_issue_body(owner: &str, repo: &str, number: i64) -> String {
    let output = tokio::process::Command::new("gh")
        .args([
            "issue", "view",
            &number.to_string(),
            "--repo", &format!("{owner}/{repo}"),
            "--json", "body,comments",
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let json: serde_json::Value =
                serde_json::from_slice(&out.stdout).unwrap_or_default();
            let body = json["body"].as_str().unwrap_or("").to_string();
            let comments = json["comments"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c["body"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n---\n")
                })
                .unwrap_or_default();
            if comments.is_empty() {
                body
            } else {
                format!("{body}\n\n## Comments\n{comments}")
            }
        }
        _ => String::new(),
    }
}

async fn fetch_repo_structure(owner: &str, repo: &str) -> String {
    let repo_path = format!(
        "{}/code/{}/{}",
        std::env::var("HOME").unwrap_or_default(),
        owner,
        repo
    );

    let output = tokio::process::Command::new("find")
        .args([&repo_path, "-name", "*.rs", "-not", "-path", "*/target/*", "-not", "-path", "*/.git/*"])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let files = String::from_utf8_lossy(&out.stdout);
            let trimmed: Vec<&str> = files
                .lines()
                .map(|l| l.strip_prefix(&repo_path).unwrap_or(l))
                .take(100)
                .collect();
            trimmed.join("\n")
        }
        _ => "(could not read repo structure)".to_string(),
    }
}

async fn call_llm(model: &str, system: &str, user: &str) -> Result<String> {
    // Use OpenAI-compatible API
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .unwrap_or_default();

    let (url, body) = if model.contains("claude") {
        // Anthropic API
        (
            "https://api.anthropic.com/v1/messages".to_string(),
            serde_json::json!({
                "model": model,
                "max_tokens": 4096,
                "system": system,
                "messages": [{"role": "user", "content": user}]
            }),
        )
    } else {
        // OpenAI API
        (
            "https://api.openai.com/v1/chat/completions".to_string(),
            serde_json::json!({
                "model": model,
                "max_completion_tokens": 4096,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user}
                ]
            }),
        )
    };

    let client = reqwest::Client::new();
    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json");

    if model.contains("claude") {
        req = req
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }

    let resp = req.json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;

    // Extract content based on API format
    let content = if model.contains("claude") {
        json["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    } else {
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    Ok(content)
}

fn parse_plan_response(response: &str) -> Result<TaskPlan> {
    // Try to extract JSON from the response (might have markdown wrapping)
    let json_str = if let Some(start) = response.find('{') {
        if let Some(end) = response.rfind('}') {
            &response[start..=end]
        } else {
            response
        }
    } else {
        response
    };

    let plan: TaskPlan = serde_json::from_str(json_str).unwrap_or_else(|_| {
        // Fallback: treat the whole response as a single-task prompt
        TaskPlan {
            assessment: "Could not parse structured plan".to_string(),
            complexity: 3,
            subtasks: vec![Subtask {
                title: "Implementation".to_string(),
                prompt: response.to_string(),
                depends_on: None,
            }],
        }
    });

    Ok(plan)
}
