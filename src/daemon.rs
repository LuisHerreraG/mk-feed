//! The daemon: poll the queue, publish what is due, notify what needs a human.
//!
//! Started through `ops/daemon.sh` by whatever supervisor you run, so it must
//! survive restarts without ever re-publishing. That is the whole reason state
//! is written to disk *before* the POST: on boot, `attempting` becomes
//! `unknown` and is never retried automatically.

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

    // `scheduled` means approved and timed: it fires. There is no confirmation
    // step at publish time — approval happened before the schedule existed.
    for id in due {
        match publish::execute(cfg, queue, &id, false, false) {
            Ok(report) => eprintln!(
                "[mk-feed] {} → {} ({})",
                id, report.state_after, report.message
            ),
            Err(err) => {
                eprintln!("[mk-feed] {id} refused: {err:#}");
                let mut held = queue.get(&id)?;
                held.note = format!("refused at publish time: {err:#}");
                queue.save(&held)?;
            }
        }
    }
    Ok(())
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
