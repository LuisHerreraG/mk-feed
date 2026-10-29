//! Secrets live in the macOS Keychain, never in a file and never in git.
//!
//! We shell out to `security(1)` on purpose: it is the platform's own tool,
//! it needs no crate, and it keeps the secret out of our process environment.
//! (The value is passed on argv, which is readable by the same user only —
//! an acceptable trade on a single-user workstation; it is not readable by
//! other users, and `ps` shows it for milliseconds.)

use anyhow::{bail, Result};
use std::process::Command;

const SERVICE: &str = "mk-feed";

fn run(args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("/usr/bin/security").args(args).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.status.success(), stdout))
}

pub fn set(account: &str, secret: &str) -> Result<()> {
    let (ok, err) = run(&[
        "add-generic-password",
        "-U",
        "-a",
        account,
        "-s",
        SERVICE,
        "-w",
        secret,
    ])?;
    if !ok {
        bail!("keychain write failed for {account}: {err}");
    }
    Ok(())
}

pub fn get(account: &str) -> Option<String> {
    match run(&["find-generic-password", "-a", account, "-s", SERVICE, "-w"]) {
        Ok((true, value)) if !value.is_empty() => Some(value),
        _ => None,
    }
}

pub fn has(account: &str) -> bool {
    get(account).is_some()
}

pub fn forget(account: &str) -> Result<()> {
    let _ = run(&["delete-generic-password", "-a", account, "-s", SERVICE])?;
    Ok(())
}

pub const ACC_TOKEN: &str = "linkedin.access_token";
pub const ACC_REFRESH: &str = "linkedin.refresh_token";
pub const ACC_META: &str = "linkedin.meta";
pub const ACC_SECRET: &str = "linkedin.client_secret";
