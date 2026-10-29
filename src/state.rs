//! The queue is a directory of Markdown files. No database.
//!
//! Why files: the agent already writes files well, git diffs are the audit
//! trail, and the human can edit a draft with $EDITOR while a daemon polls the
//! same directory. A database would be a second source of truth for content
//! that already has one.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const STATES: [&str; 8] = [
    "draft",
    "ready",
    "scheduled",
    "attempting",
    "posted",
    "failed",
    "unknown",
    "cancelled",
];

#[derive(Debug, Clone)]
pub struct Post {
    pub id: String,
    pub path: PathBuf,
    pub state: String,
    pub hook: String,
    pub artifact: String,
    pub mode: String,
    pub at: Option<DateTime<Utc>>,
    pub at_raw: String,
    pub visibility: String,
    pub media: Vec<String>,
    pub urn: String,
    pub posted_at: String,
    pub note: String,
    pub body: String,
}

impl Post {
    pub fn new(id: &str, hook: &str, artifact: &str, body: &str, mode: &str) -> Self {
        Self {
            id: id.to_string(),
            path: PathBuf::new(),
            state: "ready".into(),
            hook: hook.to_string(),
            artifact: artifact.to_string(),
            mode: mode.to_string(),
            at: None,
            at_raw: String::new(),
            visibility: "PUBLIC".into(),
            media: Vec::new(),
            urn: String::new(),
            posted_at: String::new(),
            note: String::new(),
            body: body.to_string(),
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state.as_str(), "draft" | "ready" | "scheduled")
    }

    /// Characters LinkedIn will count against the 3000 limit.
    pub fn body_chars(&self) -> usize {
        self.body.chars().count()
    }

    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("id: {}\n", quote(&self.id)));
        out.push_str(&format!("hook: {}\n", quote(&self.hook)));
        out.push_str(&format!("artifact: {}\n", quote(&self.artifact)));
        out.push_str(&format!("state: {}\n", quote(&self.state)));
        out.push_str(&format!("mode: {}\n", quote(&self.mode)));
        out.push_str(&format!("at: {}\n", quote(&self.at_raw)));
        out.push_str(&format!("visibility: {}\n", quote(&self.visibility)));
        if self.media.is_empty() {
            out.push_str("media: []\n");
        } else {
            out.push_str("media:\n");
            for item in &self.media {
                out.push_str(&format!("  - {}\n", quote(item)));
            }
        }
        out.push_str(&format!("urn: {}\n", quote(&self.urn)));
        out.push_str(&format!("posted_at: {}\n", quote(&self.posted_at)));
        out.push_str(&format!("note: {}\n", quote(&self.note)));
        out.push_str("---\n");
        out.push_str(self.body.trim_end());
        out.push('\n');
        out
    }

    pub fn parse(path: &Path, raw: &str) -> Result<Self> {
        let rest = raw
            .strip_prefix("---\n")
            .or_else(|| raw.strip_prefix("---\r\n"))
            .with_context(|| format!("{}: missing frontmatter", path.display()))?;
        let end = rest
            .find("\n---")
            .with_context(|| format!("{}: unterminated frontmatter", path.display()))?;
        let head = &rest[..end];
        // Content, not file formatting: normalise the edges so a save/parse
        // round trip is byte-stable.
        let body = rest[end + 4..].trim_start_matches('\n').trim_end().to_string();

        let mut post = Post::new("", "", "", &body, "confirm");
        post.path = path.to_path_buf();
        let mut last_list: Option<String> = None;

        for line in head.lines() {
            if let Some(item) = line.strip_prefix("  - ") {
                if last_list.as_deref() == Some("media") {
                    post.media.push(unquote(item));
                }
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            last_list = None;
            if key == "media" && value.is_empty() {
                last_list = Some("media".to_string());
                continue;
            }
            if key == "media" && (value == "[]" || value.is_empty()) {
                continue;
            }
            let value = unquote(value);
            match key {
                "id" => post.id = value,
                "hook" => post.hook = value,
                "artifact" => post.artifact = value,
                "state" => post.state = value,
                "mode" => post.mode = value,
                "at" => {
                    post.at_raw = value.clone();
                    post.at = parse_instant(&value).ok().map(|d| d.with_timezone(&Utc));
                }
                "visibility" => post.visibility = value,
                "urn" => post.urn = value,
                "posted_at" => post.posted_at = value,
                "note" => post.note = value,
                _ => {}
            }
        }
        if post.id.is_empty() {
            post.id = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        if post.mode.is_empty() {
            post.mode = "confirm".into();
        }
        Ok(post)
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return trimmed[1..trimmed.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    trimmed.to_string()
}

/// Absolute, relative (`+2h`) and naive (`2026-11-02 09:10`, assumed local).
pub fn parse_instant(text: &str) -> Result<DateTime<chrono::FixedOffset>> {
    use chrono::TimeZone;
    let text = text.trim();
    let tz: chrono_tz::Tz = crate::config::Config::load(None)
        .map(|c| c.timezone)
        .unwrap_or_else(|_| "America/Lima".to_string())
        .parse()
        .unwrap_or(chrono_tz::America::Lima);

    if let Some(rest) = text.strip_prefix('+') {
        let (num, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: i64 = num.trim().parse().context("relative time needs a number")?;
        let delta = match unit {
            "m" => chrono::Duration::minutes(n),
            "h" => chrono::Duration::hours(n),
            "d" => chrono::Duration::days(n),
            _ => anyhow::bail!("relative time unit must be m, h or d"),
        };
        return Ok((Utc::now() + delta).with_timezone(&tz).fixed_offset());
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Ok(dt);
    }
    for format in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            let local = tz
                .from_local_datetime(&naive)
                .single()
                .context("ambiguous local time")?;
            return Ok(local.fixed_offset());
        }
    }
    for format in ["%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            let local = chrono_tz::America::Lima
                .from_local_datetime(&naive)
                .single()
                .context("ambiguous local time")?;
            return Ok(local.fixed_offset());
        }
    }
    anyhow::bail!("could not parse time {text:?} (try \"2026-11-02 09:10\" or \"+2h\")")
}

pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.chars() {
        let ch = ch.to_lowercase().next().unwrap_or(ch);
        if ch.is_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    // Never end mid-word: cut back to the last separator, not through a letter.
    if out.chars().count() >= 48 {
        if let Some(cut) = out.rfind('-') {
            out.truncate(cut);
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "post".into()
    } else {
        out
    }
}

pub struct Queue {
    pub dir: PathBuf,
}

pub struct LockGuard {
    _file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

impl Queue {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating queue dir {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn posts(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Drafts start with a digit or a letter; STYLE.md and notes do not count.
                if name.eq_ignore_ascii_case("STYLE.md") || name.eq_ignore_ascii_case("README.md") {
                    continue;
                }
            }
            out.push(path);
        }
        out.sort();
        Ok(out)
    }

    pub fn list(&self) -> Result<Vec<Post>> {
        let mut posts = Vec::new();
        for path in self.posts()? {
            let raw = std::fs::read_to_string(&path)?;
            if !raw.starts_with("---") {
                continue;
            }
            posts.push(Post::parse(&path, &raw)?);
        }
        posts.sort_by(|a, b| {
            rank(&a.state)
                .cmp(&rank(&b.state))
                .then_with(|| a.at.cmp(&b.at))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(posts)
    }

    pub fn get(&self, id: &str) -> Result<Post> {
        let path = self.dir.join(format!("{id}.md"));
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no post {id:?} in {}", self.dir.display()))?;
        Post::parse(&path, &raw)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.dir.join(format!("{id}.md")).exists()
    }

    pub fn save(&self, post: &Post) -> Result<()> {
        let path = if post.path.as_os_str().is_empty() {
            self.dir.join(format!("{}.md", post.id))
        } else {
            post.path.clone()
        };
        let tmp = path.with_extension("md.tmp");
        {
            let mut file = File::create(&tmp)?;
            file.write_all(post.render().as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// One lock covers TUI-vs-daemon in the same second. Restart safety is a
    /// different mechanism: state is written to disk before any POST.
    pub fn lock(&self) -> Result<LockGuard> {
        let path = self.dir.join(".mk-feed.lock");
        let file = File::create(&path)?;
        for _ in 0..50 {
            if file.try_lock_exclusive().is_ok() {
                return Ok(LockGuard { _file: file });
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        anyhow::bail!("queue is locked by another mk-feed process ({})", path.display())
    }

    pub fn style(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("STYLE.md")).ok()
    }
}

fn rank(state: &str) -> u8 {
    match state {
        "attempting" => 0,
        "unknown" => 1,
        "failed" => 2,
        "scheduled" => 3,
        "ready" => 4,
        "draft" => 5,
        "posted" => 6,
        _ => 7,
    }
}
