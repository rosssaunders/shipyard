use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::process::Command;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IssueInfo {
    pub title: String,
    pub body: String,
    pub comments: Vec<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrInfo {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub head_ref_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestResult {
    pub command: String,
    pub success: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReconReport {
    pub issue: Option<IssueInfo>,
    pub related_prs: Vec<PrInfo>,
    pub existing_branch: Option<String>,
    pub recent_commits: Vec<String>,
    pub baseline_tests: Option<TestResult>,
    pub possibly_fixed: bool,
    pub file_tree: String,
    pub key_files: Vec<(String, String)>,
    pub repo_path: String,
}

pub async fn run_recon(
    owner: &str,
    repo: &str,
    issue_number: Option<i64>,
    repo_path: &str,
) -> ReconReport {
    let issue_fut = fetch_issue(owner, repo, issue_number);
    let related_prs_fut = fetch_related_prs(owner, repo, issue_number);
    let branch_fut = find_existing_branch(repo_path, issue_number);
    let commits_fut = fetch_recent_commits(repo_path);
    let tests_fut = run_baseline_tests(repo_path);
    let tree_fut = fetch_file_tree(repo_path);
    let key_files_fut = read_key_files(repo_path);

    let (
        issue,
        related_prs,
        existing_branch,
        recent_commits,
        baseline_tests,
        file_tree,
        key_files,
    ) = tokio::join!(
        issue_fut,
        related_prs_fut,
        branch_fut,
        commits_fut,
        tests_fut,
        tree_fut,
        key_files_fut
    );

    let possibly_fixed = detect_possible_fix(issue.as_ref(), &related_prs, &recent_commits);

    ReconReport {
        issue,
        related_prs,
        existing_branch,
        recent_commits,
        baseline_tests,
        possibly_fixed,
        file_tree,
        key_files,
        repo_path: repo_path.to_string(),
    }
}

async fn fetch_issue(owner: &str, repo: &str, issue_number: Option<i64>) -> Option<IssueInfo> {
    let number = issue_number?;
    let repo_name = format!("{owner}/{repo}");
    let output = run_command(
        "gh",
        &[
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            &repo_name,
            "--json",
            "title,body,comments,labels,assignees",
        ],
        None,
    )
    .await?;

    let value: serde_json::Value = match serde_json::from_str(&output) {
        Ok(value) => value,
        Err(err) => {
            warn!(issue_number = number, error = %err, "failed to parse issue json");
            return None;
        }
    };

    Some(IssueInfo {
        title: value["title"].as_str().unwrap_or_default().to_string(),
        body: value["body"].as_str().unwrap_or_default().to_string(),
        comments: value["comments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|comment| comment["body"].as_str().map(ToOwned::to_owned))
            .collect(),
        labels: value["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label["name"].as_str().map(ToOwned::to_owned))
            .collect(),
        assignees: value["assignees"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|assignee| assignee["login"].as_str().map(ToOwned::to_owned))
            .collect(),
    })
}

async fn fetch_related_prs(owner: &str, repo: &str, issue_number: Option<i64>) -> Vec<PrInfo> {
    let Some(number) = issue_number else {
        return Vec::new();
    };

    let repo_name = format!("{owner}/{repo}");
    let output = match run_command(
        "gh",
        &[
            "pr",
            "list",
            "--repo",
            &repo_name,
            "--search",
            &number.to_string(),
            "--json",
            "number,title,state,headRefName",
        ],
        None,
    )
    .await
    {
        Some(output) => output,
        None => return Vec::new(),
    };

    match serde_json::from_str::<Vec<serde_json::Value>>(&output) {
        Ok(values) => values
            .into_iter()
            .map(|value| PrInfo {
                number: value["number"].as_i64().unwrap_or_default(),
                title: value["title"].as_str().unwrap_or_default().to_string(),
                state: value["state"].as_str().unwrap_or_default().to_string(),
                head_ref_name: value["headRefName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect(),
        Err(err) => {
            warn!(issue_number = number, error = %err, "failed to parse related PRs");
            Vec::new()
        }
    }
}

async fn find_existing_branch(repo_path: &str, issue_number: Option<i64>) -> Option<String> {
    let number = issue_number?;
    let pattern = format!("issue-{number}");
    let output = run_command("git", &["branch", "-a"], Some(repo_path)).await?;

    output
        .lines()
        .map(str::trim)
        .map(|line| line.trim_start_matches('*').trim())
        .find(|line| line.contains(&pattern))
        .map(ToOwned::to_owned)
}

async fn fetch_recent_commits(repo_path: &str) -> Vec<String> {
    match run_command("git", &["log", "--oneline", "-20", "main"], Some(repo_path)).await {
        Some(output) => output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        None => Vec::new(),
    }
}

async fn run_baseline_tests(repo_path: &str) -> Option<TestResult> {
    let command = "cargo test --release".to_string();
    let output = run_shell_command("cargo test --release 2>&1", Some(repo_path)).await?;
    Some(TestResult {
        command,
        success: output.status,
        output: output.stdout,
    })
}

async fn fetch_file_tree(repo_path: &str) -> String {
    let script = r#"find . \
  \( -path './target' -o -path './target/*' -o -path './.git' -o -path './.git/*' -o -path './node_modules' -o -path './node_modules/*' \) -prune \
  -o -print | sort"#;

    match run_shell_command(script, Some(repo_path)).await {
        Some(output) if !output.stdout.trim().is_empty() => output.stdout,
        Some(_) | None => "(unable to read file tree)".to_string(),
    }
}

async fn read_key_files(repo_path: &str) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for name in ["README.md", "AGENTS.md", "ARCHITECTURE.md"] {
        let path = format!("{repo_path}/{name}");
        match fs::read_to_string(&path).await {
            Ok(contents) => {
                let snippet = contents
                    .lines()
                    .take(200)
                    .collect::<Vec<_>>()
                    .join("\n");
                files.push((name.to_string(), snippet));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(file = %path, error = %err, "failed to read key file"),
        }
    }
    files
}

fn detect_possible_fix(
    issue: Option<&IssueInfo>,
    related_prs: &[PrInfo],
    recent_commits: &[String],
) -> bool {
    let Some(issue) = issue else {
        return false;
    };

    let keywords = extract_keywords(&issue.title);
    if keywords.is_empty() {
        return false;
    }

    let commit_hit = recent_commits.iter().any(|commit| matches_keywords(commit, &keywords));
    let pr_hit = related_prs.iter().any(|pr| {
        matches_keywords(&pr.title, &keywords) || matches_keywords(&pr.head_ref_name, &keywords)
    });

    commit_hit || pr_hit
}

fn extract_keywords(title: &str) -> Vec<String> {
    let mut keywords: Vec<String> = title
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|token| {
            let lowered = token.trim().to_ascii_lowercase();
            if lowered.len() >= 4 {
                Some(lowered)
            } else {
                None
            }
        })
        .collect();
    keywords.sort();
    keywords.dedup();
    keywords
}

fn matches_keywords(haystack: &str, keywords: &[String]) -> bool {
    let lowered = haystack.to_ascii_lowercase();
    let matched = keywords
        .iter()
        .filter(|keyword| lowered.contains(keyword.as_str()))
        .count();
    matched >= 2 || matched == keywords.len().min(1)
}

async fn run_command(cmd: &str, args: &[&str], cwd: Option<&str>) -> Option<String> {
    let mut command = Command::new(cmd);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    match command.output().await {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => {
            warn!(
                command = cmd,
                status = ?output.status.code(),
                stderr = %String::from_utf8_lossy(&output.stderr),
                "recon command failed"
            );
            None
        }
        Err(err) => {
            warn!(command = cmd, error = %err, "failed to run recon command");
            None
        }
    }
}

async fn run_shell_command(script: &str, cwd: Option<&str>) -> Option<ShellOutput> {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    match command.output().await {
        Ok(output) => Some(ShellOutput {
            status: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_string(),
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim()
                .to_string(),
        }),
        Err(err) => {
            warn!(error = %err, "failed to run shell command during recon");
            None
        }
    }
}

struct ShellOutput {
    status: bool,
    stdout: String,
    #[allow(dead_code)]
    stderr: String,
}
