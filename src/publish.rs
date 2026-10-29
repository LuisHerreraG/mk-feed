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
    /// The commentary exactly as it will go on the wire (escaped), for dry runs.
    pub payload: String,
    /// Image urns attached to the post, in order.
    pub media: Vec<String>,
    pub id: String,
    pub state_before: String,
    pub state_after: String,
    pub urn: String,
    pub url: String,
    pub chars: usize,
    pub message: String,
    pub warnings: Vec<String>,
}

/// `https://www.linkedin.com/feed/update/<urn>/` → `<urn>`
fn urn_from_url(url: &str) -> String {
    url.split("/update/")
        .nth(1)
        .map(|tail| tail.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

/// How many posts actually went out today, counted from the log — the same
/// record that answers "what did I publish", used here as a circuit breaker.
pub fn posted_today(cfg: &Config) -> u32 {
    let today = chrono::Local::now().date_naive();
    let raw = std::fs::read_to_string(cfg.state_file("publish.jsonl")).unwrap_or_default();
    let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let event = entry.get("event").and_then(|e| e.as_str()).unwrap_or("");
        // Older entries carried no `urn` on the way out; the url is enough to
        // match them, so a repaired history needs no migration.
        let urn = {
            let from_field = entry.get("urn").and_then(|u| u.as_str()).unwrap_or("");
            if from_field.is_empty() {
                urn_from_url(entry.get("url").and_then(|u| u.as_str()).unwrap_or(""))
            } else {
                from_field.to_string()
            }
        };
        if urn.is_empty() {
            continue;
        }
        // A post that came down does not spend the budget: the guard is about
        // what is standing in public, not about attempts.
        if event == "unpublished" {
            live.remove(&urn);
            continue;
        }
        if event == "posted" {
            let same_day = entry
                .get("at")
                .and_then(|at| at.as_str())
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&chrono::Local).date_naive() == today)
                .unwrap_or(false);
            if same_day {
                live.insert(urn);
            }
        }
    }
    live.len() as u32
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

    // The circuit breaker. A loop, a stuck daemon or a confused agent cannot
    // turn into a stream of posts: past the cap, every path refuses.
    if cfg.max_posts_per_day > 0 {
        let today = posted_today(cfg);
        if today >= cfg.max_posts_per_day {
            return Err(anyhow::Error::new(Refusal(format!(
                "daily cap reached: {today}/{} posts today (max_posts_per_day) — nothing was sent",
                cfg.max_posts_per_day
            ))));
        }
    }

    let commentary = compose_commentary(&post);
    let chars = commentary.chars().count();

    if dry_run {
        return Ok(Report {
            payload: crate::api::escape_little_format(&commentary),
            media: Vec::new(),
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

    // Obtain everything that can fail *before* arming: a missing token or a
    // broken TLS stack must leave the post exactly where it was, not stranded
    // in `attempting` where it would look like a publish of unknown outcome.
    // Arm last: the only window left is between the write and the POST, and
    // boot reconcile owns that one.
    let token = auth::access_token(cfg)?;
    let meta = auth::load_meta();
    let author = format!("urn:li:person:{}", meta.sub);
    let client = api::http()?;
    let mut media_urns: Vec<String> = Vec::new();

    // Images are uploaded before arming. An upload that fails publishes nothing,
    // so the post must be left exactly where it was — same rule as the token.
    if !post.media.is_empty() {
        let mut urns = Vec::new();
        for item in &post.media {
            let path = if std::path::Path::new(item).is_absolute() {
                std::path::PathBuf::from(item)
            } else {
                queue.dir.join(item)
            };
            let bytes = std::fs::read(&path).map_err(|e| {
                anyhow::Error::new(Refusal(format!(
                    "cannot read media {}: {e}",
                    path.display()
                )))
            })?;
            urns.push(api::upload_image(
                &client,
                &token,
                &author,
                &bytes,
                &cfg.api_version,
            )?);
        }
        // Personal tokens cannot poll readiness; give processing a moment.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        media_urns = urns;
    }

    let prior_state = post.state.clone();
    post.state = "attempting".into();
    post.note = format!("attempt at {}", chrono::Local::now().to_rfc3339());
    queue.save(&post)?;
    // Every arm leaves a trace: when a post ends up stranded, the log says who
    // armed it and when, instead of leaving the file as the only witness.
    log_line(cfg, &post, "attempting", "", "")?;

    let mut outcome = api::create_post(
        &client,
        &token,
        &author,
        &commentary,
        &post.visibility,
        &cfg.api_version,
        &media_urns,
    );

    // Media that is still processing is a 4xx: nothing was published, so one
    // retry after a pause is safe and is the only readiness signal available.
    if let Err((SendError::Rejected(msg), _)) = &outcome {
        if api::looks_like_media_not_ready(msg) {
            std::thread::sleep(std::time::Duration::from_secs(3));
            outcome = api::create_post(
                &client,
                &token,
                &author,
                &commentary,
                &post.visibility,
                &cfg.api_version,
                &media_urns,
            );
        }
    }

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
                &media_urns,
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
                payload: String::new(),
                id: post.id.clone(),
                state_before: prior_state,
                state_after: "posted".into(),
                media: media_urns.clone(),
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
                payload: String::new(),
                id: post.id.clone(),
                state_before: prior_state,
                state_after: "failed".into(),
                media: media_urns.clone(),
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
                payload: String::new(),
                id: post.id.clone(),
                state_before: prior_state,
                state_after: "unknown".into(),
                media: media_urns.clone(),
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

/// Approval is the editorial gate: the lint runs here, and nothing enters the
/// calendar without passing it. Deliberately separate from `send` — one is "this
/// is worth publishing", the other is "publish it now".
pub fn approve(queue: &Queue, id: &str, force: bool) -> Result<()> {
    let mut post = queue.get(id)?;
    if post.state != "draft" {
        return Err(anyhow::Error::new(Refusal(format!(
            "{} is {} — only a draft needs approval",
            post.id, post.state
        ))));
    }
    let issues = check::run(&post, &queue.dir);
    let fails: Vec<String> = issues
        .iter()
        .filter(|i| i.is_fail())
        .map(|i| i.message.clone())
        .collect();
    if !fails.is_empty() && !force {
        return Err(anyhow::Error::new(Refusal(format!(
            "{} fails the lint — {} (--force to override)",
            post.id,
            fails.join("; ")
        ))));
    }
    post.state = "ready".into();
    queue.save(&post)?;
    Ok(())
}

/// Undo a publish: delete the post and put the draft back in the queue.
/// Written for the honest use case — publishing a test post to prove the pipe
/// works, then taking it down.
pub fn unpublish(cfg: &Config, queue: &Queue, id: &str) -> Result<String> {
    let post = queue.get(id)?;
    if post.urn.is_empty() {
        return Err(anyhow::Error::new(Refusal(format!(
            "{} has no urn — it was never published through the API",
            post.id
        ))));
    }
    let token = auth::access_token(cfg)?;
    let client = api::http()?;
    let mut outcome = api::delete_post(&client, &token, &post.urn, &cfg.api_version);
    if let Err((SendError::Rejected(msg), stale)) = &outcome {
        if *stale && msg.starts_with("401") {
            let tokens = auth::refresh(cfg)?;
            auth::store_tokens(cfg, &tokens)?;
            outcome = api::delete_post(&client, &tokens.access_token, &post.urn, &cfg.api_version);
        }
    }
    match outcome {
        Ok(()) => {
            let urn = post.urn.clone();
            let _guard = queue.lock()?;
            let mut post = queue.get(id)?;
            post.state = "ready".into();
            post.urn.clear();
            post.posted_at.clear();
            post.note = format!("unpublished {urn} at {}", chrono::Local::now().to_rfc3339());
            queue.save(&post)?;
            drop(_guard);
            remove_index_row(cfg, &urn)?;
            let mut audited = post.clone();
            audited.urn = urn.clone();
            log_line(cfg, &audited, "unpublished", &api::public_url(&urn), "")?;
            Ok(urn)
        }
        Err((SendError::Rejected(msg), _)) => Err(anyhow::Error::new(Refusal(format!(
            "LinkedIn refused the delete: {msg}"
        )))),
        Err((SendError::Unknown(msg), _)) => {
            anyhow::bail!("unknown outcome deleting the post: {msg}")
        }
    }
}

/// The paste path's closing move: the same log and index a real publish writes,
/// so the record does not care how the bytes reached LinkedIn.
pub fn record_manual(cfg: &Config, post: &Post) -> Result<()> {
    log_line(cfg, post, "posted-manual", "", "")?;
    append_index(cfg, post)?;
    Ok(())
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

/// Drop the index row that points at this urn.
fn remove_index_row(cfg: &Config, urn: &str) -> Result<()> {
    let Some(index) = &cfg.index else {
        return Ok(());
    };
    if !index.exists() {
        return Ok(());
    }
    let url = api::public_url(urn);
    let raw = std::fs::read_to_string(index)?;
    let kept: Vec<&str> = raw.lines().filter(|line| !line.contains(&url)).collect();
    std::fs::write(index, format!("{}\n", kept.join("\n")))?;
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
