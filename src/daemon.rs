//! The daemon: poll the queue, publish what is due, notify what needs a human.
//!
//! Started through `ops/daemon.sh` by whatever supervisor you run, so it must
//! survive restarts without
//! ever re-publishing. That is the whole reason state is
//! written to disk *before* the POST: on boot, `attempting` becomes `unknown`
//! and is never retried automatically.

use crate::config::Config;
use crate::output::Ctx;
use crate::publish;
use crate::state::Queue;
use anyhow::Result;
use std::time::Duration;

pub fn run(cfg: &Config, once: bool, interval: u64, _ctx: &Ctx) -> Result<()> {
    let queue = Queue::open(&cfg.queue)?;
    reconcile(cfg, &queue)?;
    loop {
        if let Err(err) = tick(cfg, &queue) {
            eprintln!("[mk-feed] tick failed: {err:#}");
            if once {
                return Err(err);
            }
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(interval.max(5)));
    }
}

/// Anything caught mid-flight when the process died is now a human's problem.
pub fn reconcile(cfg: &Config, queue: &Queue) -> Result<()> {
    let _guard = queue.lock()?;
    for mut post in queue.list()? {
        if post.state != "attempting" {
            continue;
        }
        post.state = "unknown".into();
        post.note =
            "process died mid-attempt (daemon restart); outcome unknown — check LinkedIn before retrying"
                .into();
        queue.save(&post)?;
        log(cfg, &post, "unknown-on-boot", &post.note)?;
        eprintln!("[mk-feed] {} was attempting at boot → unknown", post.id);
    }
    Ok(())
}

pub fn tick(cfg: &Config, queue: &Queue) -> Result<()> {
    let now = chrono::Utc::now();
    let due: Vec<String> = queue
        .list()?
        .into_iter()
        .filter(|p| p.state == "scheduled")
        .filter(|p| p.at.map(|at| at <= now).unwrap_or(false))
        .map(|p| p.id)
        .collect();

    for id in due {
        let post = queue.get(&id)?;
        if post.mode == "auto" {
            match publish::execute(cfg, queue, &id, false, false) {
                Ok(report) => {
                    eprintln!(
                        "[mk-feed] {} → {} ({})",
                        id, report.state_after, report.message
                    );
                }
                Err(err) => {
                    eprintln!("[mk-feed] {id} refused: {err:#}");
                    let mut failed = queue.get(&id)?;
                    failed.note = format!("refused at publish time: {err:#}");
                    queue.save(&failed)?;
                }
            }
        } else if !already_notified(cfg, &post.id, &post.at_raw)? {
            if cfg.notify {
                notify(&post.hook, &post.id);
            }
            remember_notified(cfg, &post.id, &post.at_raw)?;
            log(
                cfg,
                &post,
                "awaiting-confirm",
                "due and waiting for a human keypress",
            )?;
            eprintln!("[mk-feed] {id} due → waiting for confirmation");
        }
    }
    Ok(())
}

fn notified_path(cfg: &Config) -> std::path::PathBuf {
    cfg.state_file("notified.json")
}

fn already_notified(cfg: &Config, id: &str, at: &str) -> Result<bool> {
    let key = format!("{id}@{at}");
    let raw = std::fs::read_to_string(notified_path(cfg)).unwrap_or_default();
    let list: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
    Ok(list.contains(&key))
}

fn remember_notified(cfg: &Config, id: &str, at: &str) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)?;
    let path = notified_path(cfg);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut list: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
    list.push(format!("{id}@{at}"));
    std::fs::write(path, serde_json::to_string(&list)?)?;
    Ok(())
}

fn notify(hook: &str, id: &str) {
    #[cfg(target_os = "macos")]
    {
        let safe_hook = hook.replace('"', "'").chars().take(120).collect::<String>();
        let script = format!(
            "display notification \"{}\" with title \"mk-feed\" subtitle \"{} ready to send\"",
            safe_hook, id
        );
        let _ = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (hook, id);
    }
}

fn log(cfg: &Config, post: &crate::state::Post, event: &str, message: &str) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(&cfg.state_dir)?;
    let line = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "event": event,
        "id": post.id,
        "urn": post.urn,
        "message": message,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.state_file("publish.jsonl"))?;
    writeln!(file, "{line}")?;
    Ok(())
}
