//! Publishing: the only place that can put something on the internet.
//!
//! The sequence is fixed and non-negotiable:
//!   1. lint (unless --force)
//!   2. lock the queue
//!   3. write `attempting` to disk  *before* the request
//!   4. POST
//!   5. write the outcome; never retry an unknown
//!
//! Step 3 is what makes a crash, a supervisor restart or a kill -9 safe: the state
//! file is the mutex across process lifetimes, because there is no idempotency
//! key to lean on.

use crate::api::{self, SendError};
use crate::auth;
use crate::check;
use crate::config::Config;
use crate::state::{Post, Queue};
use anyhow::Result;
use serde::Serialize;

/// A refusal is a decision, not a breakage: exit 3, and the message says why.
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Refusal {}

#[derive(Debug, Serialize)]
pub struct Report {
    pub id: String,
    pub state_before: String,
    pub state_after: String,
    pub urn: String,
    pub url: String,
    pub chars: usize,
    pub message: String,
    pub warnings: Vec<String>,
}

pub fn execute(cfg: &Config, queue: &Queue, id: &str, force: bool, dry_run: bool) -> Result<Report> {
    let mut post = queue.get(id)?;
    let state_before = post.state.clone();
    let issues = check::run(&post, &queue.dir);
    let warnings: Vec<String> = issues
        .iter()
        .filter(|i| i.is_warn())
        .map(|i| format!("{}: {}", i.rule, i.message))
        .collect();

    if issues.iter().any(|i| i.is_fail()) && !force {
        return Err(anyhow::Error::new(Refusal(format!(
            "{} fails `mk-feed check` — {} (use --force to override)",
            post.id,
            issues
                .iter()
                .filter(|i| i.is_fail())
                .map(|i| i.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        ))));
    }

    if matches!(post.state.as_str(), "attempting" | "unknown") && !force {
        return Err(anyhow::Error::new(Refusal(format!(
            "{} is {} — a human must resolve it first (--force to override)",
            post.id, post.state
        ))));
    }

    if !post.media.is_empty() {
        return Err(anyhow::Error::new(Refusal(
            "media publishing is not implemented yet (phase 2)".into(),
        )));
    }

    let commentary = compose_commentary(&post);
    let chars = commentary.chars().count();

    if dry_run {
        return Ok(Report {
            id: post.id.clone(),
            state_before: state_before.clone(),
            state_after: state_before,
            urn: String::new(),
            url: "https://www.linkedin.com/feed/".into(),
            chars,
            message: "dry run: nothing sent".into(),
            warnings,
        });
    }

    let _guard = queue.lock()?;
    // Re-read under the lock: the daemon or another shell may have moved it.
    post = queue.get(id)?;
    if !matches!(post.state.as_str(), "draft" | "ready" | "scheduled" | "failed") {
        return Err(anyhow::Error::new(Refusal(format!(
            "{} is now {} (changed while waiting for the lock)",
            post.id, post.state
        ))));
    }

    let prior_state = post.state.clone();
    post.state = "attempting".into();
    post.note = format!("attempt at {}", chrono::Local::now().to_rfc3339());
    queue.save(&post)?;

    let token = auth::access_token(cfg)?;
    let meta = auth::load_meta();
    let author = format!("urn:li:person:{}", meta.sub);
    let client = api::http()?;

    let mut outcome = api::create_post(
        &client,
        &token,
        &author,
        &commentary,
        &post.visibility,
        &cfg.api_version,
    );

    // A 401 means nothing was published, so refreshing and trying once is safe.
    if let Err((SendError::Rejected(msg), stale)) = &outcome {
        if *stale && msg.starts_with("401") {
            let tokens = auth::refresh(cfg)?;
            auth::store_tokens(cfg, &tokens)?;
            outcome = api::create_post(
                &client,
                &tokens.access_token,
                &author,
                &commentary,
                &post.visibility,
                &cfg.api_version,
            );
        }
    }

    let now = chrono::Local::now().to_rfc3339();
    let report = match outcome {
        Ok(urn) => {
            post.state = "posted".into();
            post.urn = urn.clone();
            post.posted_at = now.clone();
            post.note.clear();
            queue.save(&post)?;
            append_index(cfg, &post)?;
            let url = api::public_url(&urn);
            log_line(cfg, &post, "posted", &url, "")?;
            Report {
                id: post.id.clone(),
                state_before: prior_state,
                state_after: "posted".into(),
                urn,
                url,
                chars,
                message: "published".into(),
                warnings,
            }
        }
        Err((SendError::Rejected(msg), _)) => {
            post.state = "failed".into();
            post.note = msg.chars().take(400).collect();
            queue.save(&post)?;
            log_line(cfg, &post, "failed", "", &msg)?;
            Report {
                id: post.id.clone(),
                state_before: prior_state,
                state_after: "failed".into(),
                urn: String::new(),
                url: String::new(),
                chars,
                message: msg,
                warnings,
            }
        }
        Err((SendError::Unknown(msg), _)) => {
            post.state = "unknown".into();
            post.note = msg.chars().take(400).collect();
            queue.save(&post)?;
            log_line(cfg, &post, "unknown", "", &msg)?;
            Report {
                id: post.id.clone(),
                state_before: prior_state,
                state_after: "unknown".into(),
                urn: String::new(),
                url: String::new(),
                chars,
                message: msg,
                warnings,
            }
        }
    };

    Ok(report)
}

/// Hook + body. The API keeps `\n\n`, unlike pasting into the web composer
/// (which collapses blank lines) — the reason this tool exists.
pub fn compose_commentary(post: &Post) -> String {
    let body = post.body.trim();
    if body.is_empty() {
        post.hook.trim().to_string()
    } else {
        format!("{}\n\n{}", post.hook.trim(), body)
    }
}

fn log_line(cfg: &Config, post: &Post, event: &str, url: &str, error: &str) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    let line = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "event": event,
        "id": post.id,
        "urn": post.urn,
        "url": url,
        "chars": post.body_chars(),
        "error": error,
    });
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.state_file("publish.jsonl"))?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Insert a row after the last table row, so a trailing prose section stays put.
fn append_index(cfg: &Config, post: &Post) -> Result<()> {
    let Some(index) = &cfg.index else {
        return Ok(());
    };
    if !index.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(index)?;
    let url = api::public_url(&post.urn);
    let row = format!(
        "| {} | {} | [LinkedIn]({}) | `{}` |",
        chrono::Local::now().format("%Y-%m-%d"),
        post.hook.replace('|', "\\|"),
        url,
        post.artifact.replace('|', "\\|")
    );
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let last_table = lines.iter().rposition(|l| l.trim_start().starts_with('|'));
    match last_table {
        Some(at) => lines.insert(at + 1, row),
        None => lines.push(row),
    }
    std::fs::write(index, format!("{}\n", lines.join("\n")))?;
    Ok(())
}
