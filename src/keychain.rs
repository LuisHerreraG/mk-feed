//! Secrets live in the macOS Keychain, never in a file and never in git.
//!
//! We shell out to `security(1)` on purpose: it is the platform's own tool,
//! it needs no crate, and it keeps the secret out of our process environment.
//! (The value is passed on argv, which is readable by the same user only —
//! an acceptable trade on a single-user workstation; it is not readable by
//! other users, and `ps` shows it for milliseconds.)

use anyhow::{bail, Result};
use std::process::Command;

/// `MK_FEED_KEYCHAIN_SERVICE` exists so tests can point at a throwaway service
/// instead of the real one — a sandboxed config with a live Keychain is how a
/// test published to a real account on 2026-10-28.
fn service() -> String {
    std::env::var("MK_FEED_KEYCHAIN_SERVICE").unwrap_or_else(|_| "mk-feed".to_string())
}

fn run(args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("/usr/bin/security").args(args).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success(), stdout))
}

pub fn set(account: &str, secret: &str) -> Result<()> {
    let service = service();
    let (ok, err) = run(&[
        "add-generic-password",
        "-U",
        "-a",
        account,
        "-s",
        &service,
        "-w",
        secret,
    ])?;
    if !ok {
        bail!("keychain write failed for {account}: {err}");
    }
    Ok(())
}

pub fn get(account: &str) -> Option<String> {
    let service = service();
    match run(&["find-generic-password", "-a", account, "-s", &service, "-w"]) {
        Ok((true, value)) if !value.is_empty() => Some(value),
        _ => None,
    }
}

pub fn has(account: &str) -> bool {
    get(account).is_some()
}

pub fn forget(account: &str) -> Result<()> {
    let service = service();
    let _ = run(&["delete-generic-password", "-a", account, "-s", &service])?;
    Ok(())
}

pub const ACC_TOKEN: &str = "linkedin.access_token";
pub const ACC_REFRESH: &str = "linkedin.refresh_token";
pub const ACC_META: &str = "linkedin.meta";
pub const ACC_SECRET: &str = "linkedin.client_secret";
