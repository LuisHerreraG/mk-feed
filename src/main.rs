mod api;
mod auth;
mod check;
mod config;
mod daemon;
mod keychain;
mod output;
mod publish;
mod state;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use output::{Ctx, EXIT_ERROR, EXIT_OK, EXIT_PARTIAL, EXIT_REFUSED};
use state::{Post, Queue};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mk-feed",
    version,
    about = "Plan, schedule and publish posts from the terminal",
    long_about = "Files are the queue, the Keychain holds the token, and nothing is\n\
                  published twice. `mk-feed doctor --json` first, always."
)]
struct Cli {
    /// One JSON document on stdout
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    no_color: bool,
    /// Alternate config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Health: config, token age, scopes, author, API version deadline
    Doctor {
        /// Also call LinkedIn (network)
        #[arg(long)]
        online: bool,
    },
    /// Authorize once; stores tokens in the Keychain
    Auth {
        /// Finish with a pasted redirect URL instead of the loopback listener
        #[arg(long)]
        url: Option<String>,
        /// Do not try to open a browser
        #[arg(long)]
        no_open: bool,
        /// Delete the stored tokens and exit (before re-authorizing)
        #[arg(long)]
        forget: bool,
        /// Read the client secret from stdin and store it in the Keychain
        /// (`pbpaste | mk-feed auth --store-secret`)
        #[arg(long)]
        store_secret: bool,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Create a draft (body from --body, --body-file or stdin)
    Add {
        #[arg(long)]
        hook: String,
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// List the queue
    List {
        /// draft|ready|scheduled|attempting|posted|failed|unknown|cancelled
        #[arg(long)]
        state: Option<String>,
    },
    /// Show one post in full
    Show { id: String },
    /// Print the house format (STYLE.md beside the drafts)
    Style,
    /// Lint a draft against the house format (0 clean, 3 fail, 4 warnings)
    Check { id: String },
    /// Put a post in the calendar
    Schedule {
        id: String,
        /// "2026-11-02 09:10", RFC3339, or "+2h"
        #[arg(long)]
        at: String,
        /// auto | confirm (default: config default_mode)
        #[arg(long)]
        mode: Option<String>,
    },
    /// Publish now
    Send {
        id: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Take a post out of the calendar
    Cancel { id: String },
    /// Record that a post was published by hand (paste path): closes the loop
    Mark {
        id: String,
        /// The post URL, if you have it
        #[arg(long)]
        url: Option<String>,
    },
    /// Delete a published post and put the draft back in the queue
    Unpublish { id: String },
    /// Publish log
    Log {
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
    /// Interactive queue
    Tui,
    /// Poll the queue and publish what is due
    Daemon {
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 60)]
        interval: u64,
    },
    /// Print the resolved config
    Cfg {
        /// Write a starter config if none exists
        #[arg(long)]
        init: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let ctx = Ctx::new(cli.json, cli.no_color);
    let code = match run(&cli, &ctx) {
        Ok(code) => code,
        Err(err) => {
            if let Some(refusal) = err.downcast_ref::<publish::Refusal>() {
                ctx.refuse("E_REFUSED", &refusal.0, None)
            } else {
                ctx.fail("E_INTERNAL", &format!("{err:#}"))
            }
        }
    };
    std::process::exit(code);
}

fn load(cli: &Cli) -> Result<config::Config> {
    config::Config::load(cli.config.as_deref())
}

fn run(cli: &Cli, ctx: &Ctx) -> Result<i32> {
    match &cli.cmd {
        Cmd::Cfg { init } => {
            let path = cli.config.clone().unwrap_or_else(config::config_path);
            if *init && !path.exists() {
                config::Config::write_default(&path)?;
            }
            if !path.exists() {
                println!("no config at {} — run `mk-feed cfg --init`", path.display());
                return Ok(EXIT_REFUSED);
            }
            let cfg = load(cli)?;
            ctx.emit(
                serde_json::to_value(&cfg)?,
                || println!("{}", toml::to_string_pretty(&cfg).unwrap_or_default()),
            );
            Ok(EXIT_OK)
        }

        Cmd::Doctor { online } => doctor(cli, ctx, *online),

        Cmd::Auth {
            url,
            no_open,
            forget,
            store_secret,
            timeout,
        } => {
            if *store_secret {
                let mut raw = String::new();
                std::io::stdin().read_to_string(&mut raw)?;
                let secret = raw.trim().to_string();
                if secret.is_empty() {
                    return Err(anyhow::Error::new(publish::Refusal(
                        "nothing on stdin — copy the secret in the portal, then:
                           pbpaste | mk-feed auth --store-secret"
                            .into(),
                    )));
                }
                keychain::set(keychain::ACC_SECRET, &secret)?;
                // Never echo it: the length is all the confirmation anyone needs.
                ctx.emit(
                    serde_json::json!({"ok": true, "stored": "linkedin.client_secret", "chars": secret.chars().count()}),
                    || println!("client secret stored in the Keychain ({} chars)", secret.chars().count()),
                );
                return Ok(EXIT_OK);
            }
            if *forget {
                keychain::forget(keychain::ACC_TOKEN)?;
                keychain::forget(keychain::ACC_REFRESH)?;
                keychain::forget(keychain::ACC_META)?;
                ctx.emit(
                    serde_json::json!({"ok": true, "forgotten": ["access_token", "refresh_token", "meta"]}),
                    || println!("tokens removed from the Keychain (client_secret kept)"),
                );
                return Ok(EXIT_OK);
            }
            do_auth(cli, ctx, url.as_deref(), *no_open, *timeout)
        }

        Cmd::Add {
            hook,
            artifact,
            body,
            body_file,
            id,
            mode,
        } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let mut text = String::new();
            if let Some(b) = body {
                text = b.clone();
            } else if let Some(path) = body_file {
                text = std::fs::read_to_string(path)?;
            } else if !std::io::stdin().is_terminal() {
                std::io::stdin().read_to_string(&mut text)?;
            }
            let id = id.clone().unwrap_or_else(|| state::slugify(hook));
            if queue.exists(&id) {
                return Err(anyhow::Error::new(publish::Refusal(format!(
                    "{id} already exists in the queue"
                ))));
            }
            let mode = mode.clone().unwrap_or_else(|| cfg.default_mode.clone());
            let mut post = Post::new(&id, hook, artifact, &text, &mode);
            post.path = queue.dir.join(format!("{id}.md"));
            post.state = if text.trim().is_empty() { "draft" } else { "ready" }.into();
            queue.save(&post)?;
            let issues = check::run(&post, &queue.dir);
            ctx.emit(
                serde_json::json!({
                    "ok": true,
                    "id": post.id,
                    "path": post.path,
                    "state": post.state,
                    "fails": issues.iter().filter(|i| i.is_fail()).count(),
                    "warns": issues.iter().filter(|i| i.is_warn()).count(),
                }),
                || {
                    println!("{} → {} ({})", post.id, post.path.display(), post.state);
                    for issue in &issues {
                        println!("  {} {}: {}", tag(issue), issue.rule, issue.message);
                    }
                },
            );
            Ok(EXIT_OK)
        }

        Cmd::List { state } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let mut posts = queue.list()?;
            if let Some(want) = state {
                if !state::STATES.contains(&want.as_str()) {
                    eprintln!("unknown state {want:?}; one of: {}", state::STATES.join(", "));
                    return Ok(output::EXIT_USAGE);
                }
                posts.retain(|p| &p.state == want);
            }
            ctx.emit(
                serde_json::json!({
                    "ok": true,
                    "queue": queue.dir,
                    "posts": posts.iter().map(|p| serde_json::json!({
                        "id": p.id,
                        "state": p.state,
                        "mode": p.mode,
                        "at": p.at_raw,
                        "hook": p.hook,
                        "artifact": p.artifact,
                        "chars": p.body_chars(),
                        "urn": p.urn,
                    })).collect::<Vec<_>>(),
                }),
                || {
                    if posts.is_empty() {
                        println!("(empty queue at {})", queue.dir.display());
                        return;
                    }
                    for post in &posts {
                        println!(
                            "{:<10} {:<34} {:<16} {}",
                            post.state,
                            truncate(&post.id, 34),
                            when(post),
                            truncate(&post.hook, 48)
                        );
                    }
                },
            );
            Ok(EXIT_OK)
        }

        Cmd::Show { id } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let post = queue.get(id)?;
            let issues = check::run(&post, &queue.dir);
            ctx.emit(
                serde_json::json!({
                    "ok": true,
                    "id": post.id,
                    "state": post.state,
                    "mode": post.mode,
                    "at": post.at_raw,
                    "visibility": post.visibility,
                    "hook": post.hook,
                    "artifact": post.artifact,
                    "media": post.media,
                    "urn": post.urn,
                    "posted_at": post.posted_at,
                    "note": post.note,
                    "body": post.body,
                    "chars": post.body_chars(),
                    "issues": issues.iter().map(|i| serde_json::json!({
                        "level": if i.is_fail() { "fail" } else { "warn" },
                        "rule": i.rule,
                        "message": i.message,
                    })).collect::<Vec<_>>(),
                    "path": post.path,
                }),
                || {
                    println!("id         {}", post.id);
                    println!("state      {}  mode {}", post.state, post.mode);
                    println!("when       {}", when(&post));
                    println!("artifact   {}", post.artifact);
                    println!("chars      {}", post.body_chars());
                    if !post.urn.is_empty() {
                        println!("url        {}", api::public_url(&post.urn));
                    }
                    if !post.note.is_empty() {
                        println!("note       {}", post.note);
                    }
                    println!("\n{}\n", post.hook);
                    println!("{}", post.body);
                    if !issues.is_empty() {
                        println!();
                        for issue in &issues {
                            println!("  {} {}: {}", tag(issue), issue.rule, issue.message);
                        }
                    }
                },
            );
            Ok(EXIT_OK)
        }

        Cmd::Style => {
            let cfg = load(cli)?;
            let path = cfg.style_path();
            let queue = Queue::open(&cfg.queue)?;
            let content = queue.style().ok_or_else(|| {
                anyhow::Error::new(publish::Refusal(format!(
                    "no STYLE.md at {} — the house format lives beside the drafts",
                    path.display()
                )))
            })?;
            ctx.emit(
                serde_json::json!({"ok": true, "path": path, "content": content}),
                || print!("{content}"),
            );
            Ok(EXIT_OK)
        }

        Cmd::Check { id } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let post = queue.get(id)?;
            let issues = check::run(&post, &queue.dir);
            let fails = issues.iter().filter(|i| i.is_fail()).count();
            let warns = issues.iter().filter(|i| i.is_warn()).count();
            ctx.emit(
                serde_json::json!({
                    "ok": fails == 0,
                    "id": post.id,
                    "chars": post.body_chars(),
                    "fails": fails,
                    "warns": warns,
                    "issues": issues.iter().map(|i| serde_json::json!({
                        "level": if i.is_fail() { "fail" } else { "warn" },
                        "rule": i.rule,
                        "message": i.message,
                    })).collect::<Vec<_>>(),
                }),
                || {
                    if issues.is_empty() {
                        println!("{}: clean ({} chars)", post.id, post.body_chars());
                    }
                    for issue in &issues {
                        println!("{} {}: {}", tag(issue), issue.rule, issue.message);
                    }
                },
            );
            Ok(if fails > 0 {
                EXIT_REFUSED
            } else if warns > 0 {
                EXIT_PARTIAL
            } else {
                EXIT_OK
            })
        }

        Cmd::Schedule { id, at, mode } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let mut post = queue.get(id)?;
            if matches!(post.state.as_str(), "posted" | "cancelled" | "attempting") {
                return Err(anyhow::Error::new(publish::Refusal(format!(
                    "{} is {} — cannot schedule it",
                    post.id, post.state
                ))));
            }
            let instant = state::parse_instant(at)?;
            if instant.with_timezone(&chrono::Utc) < chrono::Utc::now() {
                return Err(anyhow::Error::new(publish::Refusal(format!(
                    "{at} is in the past"
                ))));
            }
            post.state = "scheduled".into();
            post.note.clear();
            post.at_raw = instant.to_rfc3339();
            post.at = Some(instant.with_timezone(&chrono::Utc));
            if let Some(mode) = mode {
                post.mode = mode.clone();
            }
            queue.save(&post)?;
            ctx.emit(
                serde_json::json!({
                    "ok": true,
                    "id": post.id,
                    "at": post.at_raw,
                    "mode": post.mode,
                }),
                || {
                    println!(
                        "{} scheduled for {} ({})",
                        post.id,
                        instant.format("%Y-%m-%d %H:%M %z"),
                        post.mode
                    )
                },
            );
            Ok(EXIT_OK)
        }

        Cmd::Send { id, dry_run, force } => send(cli, ctx, id, *dry_run, *force),

        Cmd::Cancel { id } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let mut post = queue.get(id)?;
            if post.state == "posted" {
                return Err(anyhow::Error::new(publish::Refusal(format!(
                    "{} is already published",
                    post.id
                ))));
            }
            post.state = "cancelled".into();
            post.note = format!("cancelled {}", chrono::Local::now().to_rfc3339());
            queue.save(&post)?;
            ctx.emit(
                serde_json::json!({"ok": true, "id": post.id, "state": post.state}),
                || println!("{} cancelled", post.id),
            );
            Ok(EXIT_OK)
        }

        Cmd::Mark { id, url } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let guard = queue.lock()?;
            let mut post = queue.get(id)?;
            if post.state == "posted" {
                drop(guard);
                return Err(anyhow::Error::new(publish::Refusal(format!(
                    "{} is already recorded as posted",
                    post.id
                ))));
            }
            post.state = "posted".into();
            post.urn.clear();
            post.posted_at = chrono::Local::now().to_rfc3339();
            post.note = match url {
                Some(url) => format!("published by hand: {url}"),
                None => "published by hand (no URL recorded)".into(),
            };
            queue.save(&post)?;
            drop(guard);
            publish::record_manual(&cfg, &post)?;
            ctx.emit(
                serde_json::json!({"ok": true, "id": post.id, "state": post.state, "note": post.note}),
                || println!("{} recorded as posted — {}", post.id, post.note),
            );
            Ok(EXIT_OK)
        }

        Cmd::Unpublish { id } => {
            let cfg = load(cli)?;
            let queue = Queue::open(&cfg.queue)?;
            let urn = publish::unpublish(&cfg, &queue, id)?;
            ctx.emit(
                serde_json::json!({"ok": true, "id": id, "deleted": urn}),
                || println!("{id}: deleted {urn} — the draft is back to `ready`"),
            );
            Ok(EXIT_OK)
        }

        Cmd::Log { tail } => {
            let cfg = load(cli)?;
            let path = cfg.state_file("publish.jsonl");
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = raw.lines().rev().take(*tail).collect();
            ctx.emit(
                serde_json::json!({
                    "ok": true,
                    "path": path,
                    "entries": lines.iter().rev().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>(),
                }),
                || {
                    if lines.is_empty() {
                        println!("(no entries in {})", path.display());
                    }
                    for line in lines.iter().rev() {
                        println!("{line}");
                    }
                },
            );
            Ok(EXIT_OK)
        }

        Cmd::Tui => {
            let cfg = load(cli)?;
            tui::run(&cfg)?;
            Ok(EXIT_OK)
        }

        Cmd::Daemon { once, interval } => {
            let cfg = load(cli)?;
            daemon::run(&cfg, *once, *interval, ctx)?;
            Ok(EXIT_OK)
        }
    }
}

fn send(cli: &Cli, ctx: &Ctx, id: &str, dry_run: bool, force: bool) -> Result<i32> {
    let cfg = load(cli)?;
    let queue = Queue::open(&cfg.queue)?;
    let report = publish::execute(&cfg, &queue, id, force, dry_run)?;
    let code = match report.state_after.as_str() {
        "posted" => EXIT_OK,
        "failed" => EXIT_ERROR,
        "unknown" => EXIT_PARTIAL,
        _ => EXIT_OK,
    };
    ctx.emit(
        serde_json::json!({
            "ok": report.state_after == "posted",
            "state_after": report.state_after,
            "state_before": report.state_before,
            "id": report.id,
            "urn": report.urn,
            "url": report.url,
            "chars": report.chars,
            "message": report.message,
            "payload": report.payload,
            "media": report.media,
            "warnings": report.warnings,
        }),
        || {
            for warning in &report.warnings {
                eprintln!("warn: {warning}");
            }
            match report.state_after.as_str() {
                "posted" => println!("published {} ({} chars)\n{}", report.id, report.chars, report.url),
                "failed" => println!("{} FAILED: {}\nnothing was published", report.id, report.message),
                "unknown" => println!(
                    "{} UNKNOWN: {}\ncheck LinkedIn before retrying — a retry can double-post",
                    report.id, report.message
                ),
                _ => println!("{}: {}", report.id, report.message),
            }
        },
    );
    Ok(code)
}

fn doctor(cli: &Cli, ctx: &Ctx, online: bool) -> Result<i32> {
    let mut checks: Vec<(String, bool, String)> = Vec::new();
    let mut push = |name: &str, ok: bool, detail: String| checks.push((name.to_string(), ok, detail));

    let cfg = config::Config::load(cli.config.as_deref());
    let cfg = match cfg {
        Ok(cfg) => {
            push("config", true, config::config_path().display().to_string());
            Some(cfg)
        }
        Err(err) => {
            push("config", false, format!("{err}"));
            None
        }
    };

    if let Some(cfg) = &cfg {
        push(
            "client_id",
            !cfg.client_id.is_empty(),
            if cfg.client_id.is_empty() {
                "empty — set it in ~/.config/mk-feed/config.toml".into()
            } else {
                cfg.client_id.clone()
            },
        );
        push(
            "queue",
            cfg.queue.is_dir(),
            cfg.queue.display().to_string(),
        );
        push(
            "style",
            cfg.style_path().is_file(),
            cfg.style_path().display().to_string(),
        );
        push("redirect", true, cfg.redirect_uri());
        let budget = publish::posted_today(cfg);
        push(
            "daily_budget",
            cfg.max_posts_per_day == 0 || budget < cfg.max_posts_per_day,
            if cfg.max_posts_per_day == 0 {
                format!("{budget} posted today, no cap")
            } else {
                format!("{budget}/{} posted today", cfg.max_posts_per_day)
            },
        );
        push(
            "default_mode",
            matches!(cfg.default_mode.as_str(), "auto" | "confirm"),
            cfg.default_mode.clone(),
        );

        let stale = chrono::NaiveDate::parse_from_str(&cfg.api_version_stale_after, "%Y-%m-%d")
            .map(|date| chrono::Local::now().date_naive() > date)
            .unwrap_or(true);
        push(
            "api_version",
            !stale,
            format!("{} (stale after {})", cfg.api_version, cfg.api_version_stale_after),
        );

        push(
            "client_secret",
            keychain::has(keychain::ACC_SECRET),
            if keychain::has(keychain::ACC_SECRET) {
                "in Keychain".into()
            } else {
                "missing — copy it in the portal, then: pbpaste | mk-feed auth --store-secret".into()
            },
        );

        let meta = auth::load_meta();
        let expires = chrono::DateTime::parse_from_rfc3339(&meta.expires_at)
            .map(|d| d.with_timezone(&chrono::Local))
            .ok();
        push(
            "access_token",
            keychain::has(keychain::ACC_TOKEN),
            match expires {
                Some(at) if at > chrono::Local::now() => format!("valid until {}", at.format("%Y-%m-%d %H:%M")),
                Some(at) => format!("EXPIRED {} — run `mk-feed auth`", at.format("%Y-%m-%d %H:%M")),
                None => "unknown expiry — run `mk-feed auth`".into(),
            },
        );
        // A missing refresh token is not a failure — the self-serve tier does
        // not issue one. What matters is the deadline it implies: the access
        // token is the only credential, and it dies in 60 days.
        let refresh_at = chrono::DateTime::parse_from_rfc3339(&meta.refresh_expires_at)
            .map(|d| d.with_timezone(&chrono::Local))
            .ok();
        if keychain::has(keychain::ACC_REFRESH) {
            push(
                "refresh_token",
                true,
                match refresh_at {
                    Some(at) => format!("rotates on use; re-auth needed by {}", at.format("%Y-%m-%d")),
                    None => "present".into(),
                },
            );
        } else {
            push(
                "refresh_token",
                true,
                match expires {
                    Some(at) => format!(
                        "none issued at this app tier — access token is the only credential, re-auth by {}",
                        at.format("%Y-%m-%d")
                    ),
                    None => "none issued at this app tier".into(),
                },
            );
        }

        let days_left = expires.map(|at| (at - chrono::Local::now()).num_days());
        match days_left {
            Some(days) if days <= 7 => push(
                "reauth",
                false,
                format!("{days} day(s) left on the access token — run `mk-feed auth`"),
            ),
            Some(days) => push("reauth", true, format!("{days} days left on the access token")),
            None => push("reauth", false, "no token — run `mk-feed auth`".into()),
        }
        push(
            "author",
            !meta.sub.is_empty(),
            if meta.sub.is_empty() {
                "unknown — run `mk-feed auth`".into()
            } else {
                format!("urn:li:person:{} ({})", meta.sub, meta.name)
            },
        );

        if online {
            match auth::access_token(cfg).and_then(|token| auth::userinfo(&token)) {
                Ok((sub, name)) => push("online", true, format!("userinfo ok: {sub} {name}")),
                Err(err) => push("online", false, format!("{err:#}")),
            }
        }
    }

    let failed = checks.iter().filter(|(_, ok, _)| !ok).count();
    ctx.emit(
        serde_json::json!({
            "ok": failed == 0,
            "failed": failed,
            "checks": checks.iter().map(|(name, ok, detail)| serde_json::json!({
                "check": name, "ok": ok, "detail": detail
            })).collect::<Vec<_>>(),
        }),
        || {
            for (name, ok, detail) in &checks {
                let tag = if *ok { "ok  " } else { "FAIL" };
                if ctx.color() && !*ok {
                    println!(
                        "\x1b[31m{tag}\x1b[0m {:<14} {detail}",
                        name
                    );
                } else {
                    println!("{tag} {:<14} {detail}", name);
                }
            }
            if failed == 0 {
                println!("\nall checks passed");
            } else {
                println!("\n{failed} check(s) failed");
            }
        },
    );
    Ok(if failed == 0 { EXIT_OK } else { EXIT_ERROR })
}

fn do_auth(
    cli: &Cli,
    ctx: &Ctx,
    url: Option<&str>,
    no_open: bool,
    timeout: u64,
) -> Result<i32> {
    let cfg = load(cli)?;
    if cfg.client_id.is_empty() {
        return Err(anyhow::Error::new(publish::Refusal(
            "client_id is empty in ~/.config/mk-feed/config.toml".into(),
        )));
    }
    if !keychain::has(keychain::ACC_SECRET) {
        return Err(anyhow::Error::new(publish::Refusal(
            "no client secret in the Keychain:\n  \
             security add-generic-password -U -a linkedin.client_secret -s mk-feed -w '<secret>'"
                .into(),
        )));
    }

    let code = match url {
        Some(pasted) => auth::code_from_url(pasted)?,
        None => {
            let state = auth::random_state();
            let link = auth::authorize_url(&cfg, &state);
            if !ctx.json {
                println!("Open this URL and approve:\n\n{link}\n");
                println!(
                    "Waiting on {} for up to {timeout}s…\n(paste flow instead: mk-feed auth --url '<redirected URL>')",
                    cfg.redirect_uri()
                );
            }
            if !no_open {
                let _ = std::process::Command::new("/usr/bin/open")
                    .arg(&link)
                    .status();
            }
            auth::wait_for_code(cfg.redirect_port, &state, std::time::Duration::from_secs(timeout))?
        }
    };

    let tokens = auth::exchange_code(&cfg, &code)?;
    let meta = auth::store_tokens(&cfg, &tokens)?;
    ctx.emit(
        serde_json::json!({
            "ok": true,
            "author": format!("urn:li:person:{}", meta.sub),
            "name": meta.name,
            "scope": meta.scope,
            "expires_at": meta.expires_at,
            "refresh_expires_at": meta.refresh_expires_at,
            "has_refresh_token": tokens.refresh_token.is_some(),
        }),
        || {
            println!("authorized as {} ({})", meta.name, meta.sub);
            println!("scope: {}", meta.scope);
            println!("access token expires {}", meta.expires_at);
            println!("refresh token expires {}", meta.refresh_expires_at);
            if tokens.refresh_token.is_none() {
                println!("note: no refresh token returned — you will re-auth in 60 days");
            }
        },
    );
    Ok(EXIT_OK)
}

fn tag(issue: &check::Issue) -> &'static str {
    if issue.is_fail() {
        "FAIL"
    } else {
        "warn"
    }
}

fn when(post: &Post) -> String {
    match post.at {
        Some(at) => at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => post.at_raw.clone(),
    }
}

fn truncate(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        out.push('…');
    }
    out
}
