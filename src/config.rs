//! Machine-local configuration.
//!
//! Everything machine-specific lives in `~/.config/mk-feed/config.toml` — never
//! in the skill, never in the repo. Paths belong here and only here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// LinkedIn app client id (public, not a secret).
    pub client_id: String,
    /// The drafts directory. Files are the queue.
    pub queue: PathBuf,
    /// Optional index file (e.g. `PUBLISHED.md`) that `send` appends to.
    pub index: Option<PathBuf>,
    /// Pinned LinkedIn API version. It rots; `doctor` enforces the deadline.
    pub api_version: String,
    /// After this date the pinned version must be bumped.
    pub api_version_stale_after: String,
    /// Loopback port for the OAuth redirect.
    pub redirect_port: u16,
    /// Overrides the derived redirect URI. Needed when LinkedIn refuses an
    /// `http://127.0.0.1` redirect: register any https URL and use the paste
    /// flow (`mk-feed auth --url '<redirected URL>'`).
    pub redirect_uri: Option<String>,
    /// Hard ceiling on posts per calendar day. 0 disables the guard. A circuit
    /// breaker against a pathological loop, not a scheduling policy.
    pub max_posts_per_day: u32,
    /// `PUBLIC` or `CONNECTIONS`.
    pub visibility: String,
    /// IANA zone used to interpret naive timestamps in `schedule --at`.
    pub timezone: String,
    /// macOS notification when a `confirm`-mode post comes due.
    pub notify: bool,
    /// Runtime state: `publish.jsonl`, `daemon.log`, the queue lock.
    pub state_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs_home();
        Self {
            client_id: String::new(),
            queue: home.join(".config/mk-feed/drafts"),
            index: None,
            api_version: "202601".into(),
            api_version_stale_after: "2026-12-01".into(),
            redirect_port: 8787,
            redirect_uri: None,
            max_posts_per_day: 3,
            visibility: "PUBLIC".into(),
            timezone: "America/Lima".into(),
            notify: true,
            state_dir: home.join(".local/state/mk-feed"),
        }
    }
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn expand(path: &Path) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix("~/") {
        return dirs_home().join(rest);
    }
    if text == "~" {
        return dirs_home();
    }
    path.to_path_buf()
}

pub fn config_path() -> PathBuf {
    dirs_home().join(".config/mk-feed/config.toml")
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => expand(p),
            None => config_path(),
        };
        if !path.exists() {
            anyhow::bail!(
                "no config at {} (create it: `mk-feed config --init`)",
                path.display()
            );
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.queue = expand(&cfg.queue);
        cfg.state_dir = expand(&cfg.state_dir);
        cfg.index = cfg.index.as_ref().map(|p| expand(p));
        Ok(cfg)
    }

    pub fn write_default(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cfg = Config::default();
        let body = format!(
            "# mk-feed — machine-local config. Paths live here, never in the skill.\n\
             client_id = \"\"                       # LinkedIn app client id (public)\n\
             queue = \"{}\"  # drafts dir: the queue is files\n\
             # index = \"{}\"  # optional: appended on every publish\n\
             api_version = \"{}\"                # pinned; rots every year\n\
             api_version_stale_after = \"{}\"\n\
             redirect_port = {}
             # redirect_uri = \"https://...\"   # only if LinkedIn refuses loopback (paste flow)\n\
             max_posts_per_day = 3                 # hard stop; 0 disables
             visibility = \"PUBLIC\"                 # PUBLIC | CONNECTIONS\n\
             timezone = \"{}\"\n\
             notify = true\n\
             state_dir = \"{}\"\n",
            cfg.queue.display(),
            cfg.queue.join("PUBLISHED.md").display(),
            cfg.api_version,
            cfg.api_version_stale_after,
            cfg.redirect_port,
            cfg.timezone,
            cfg.state_dir.display(),
        );
        std::fs::write(path, body)?;
        Ok(())
    }

    pub fn state_file(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }

    pub fn redirect_uri(&self) -> String {
        if let Some(uri) = &self.redirect_uri {
            return uri.clone();
        }
        format!("http://127.0.0.1:{}/callback", self.redirect_port)
    }

    pub fn style_path(&self) -> PathBuf {
        self.queue.join("STYLE.md")
    }
}
