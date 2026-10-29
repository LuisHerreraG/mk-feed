//! The lint. Rules come from the practice (`STYLE.md` beside the drafts), and
//! this file is where they become mechanical so an agent cannot talk its way
//! past them at 2am.

use crate::state::Post;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Fail,
    Warn,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub level: Level,
    pub rule: &'static str,
    pub message: String,
}

impl Issue {
    pub fn is_fail(&self) -> bool {
        self.level == Level::Fail
    }
    pub fn is_warn(&self) -> bool {
        self.level == Level::Warn
    }
}

pub const MAX_CHARS: usize = 3000;

fn compose_has(post: &Post, needle: &str) -> bool {
    post.hook.contains(needle)
}

pub fn run(post: &Post, queue_dir: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    macro_rules! fail {
        ($rule:expr, $msg:expr $(,)?) => {
            issues.push(Issue {
                level: Level::Fail,
                rule: $rule,
                message: $msg,
            })
        };
    }
    macro_rules! warn {
        ($rule:expr, $msg:expr $(,)?) => {
            issues.push(Issue {
                level: Level::Warn,
                rule: $rule,
                message: $msg,
            })
        };
    }

    // The artifact rule, as code.
    if post.artifact.trim().is_empty() {
        fail!(
            "artifact-missing",
            "no `artifact:` — a commit, a measurement or a bug is required".into(),
        );
    } else {
        let artifact = post.artifact.trim();
        let has_url = artifact.starts_with("http://") || artifact.starts_with("https://");
        let has_number = artifact.chars().any(|c| c.is_ascii_digit());
        if !has_url && !has_number {
            fail!(
                "artifact-vague",
                format!("artifact {artifact:?} names neither a URL nor a measurement"),
            );
        }
    }

    let commentary = crate::publish::compose_commentary(post);
    let chars = commentary.chars().count();
    if chars > MAX_CHARS {
        fail!(
            "too-long",
            format!("{chars} characters; LinkedIn accepts {MAX_CHARS}"),
        );
    } else if chars > MAX_CHARS - 200 {
        warn!(
            "near-limit",
            format!("{chars} characters; within 200 of the {MAX_CHARS} limit"),
        );
    }

    if post.body.trim().is_empty() {
        fail!("body-empty", "the body is empty".into());
    }

    let lowered = post.body.to_uppercase();
    if lowered.contains("TODO") || lowered.contains("{...}") || lowered.contains("LOREM IPSUM") {
        fail!("placeholder", "the body still contains a placeholder".into());
    }

    if !post.media.is_empty() {
        let over = post.media.len() > 3;
        if over {
            fail!(
                "media-count",
                format!("{} media files; the format allows 3", post.media.len()),
            );
        }
        for item in &post.media {
            let path = if std::path::Path::new(item).is_absolute() {
                std::path::PathBuf::from(item)
            } else {
                queue_dir.join(item)
            };
            if !path.exists() {
                fail!("media-missing", format!("media file not found: {}", path.display()));
            }
        }
        fail!(
            "media-unsupported",
            "publishing with media is not implemented yet — post text only".into(),
        );
    }

    // LinkedIn renders no markdown: asterisks and backticks arrive verbatim.
    let markdown: Vec<&str> = [
        ("**", "**bold**"),
        ("`", "`code`"),
        ("##", "## heading"),
        ("> ", "> quote"),
    ]
    .iter()
    .filter(|(needle, _)| post.body.contains(needle) || compose_has(post, needle))
    .map(|(_, label)| *label)
    .collect();
    if !markdown.is_empty() {
        warn!(
            "markdown-literal",
            format!(
                "LinkedIn renders markdown literally — {} will show as raw characters",
                markdown.join(", ")
            )
        );
    }

    if !post.body.chars().any(|c| c.is_ascii_digit()) {
        warn!(
            "no-numbers",
            "no number anywhere in the body — the practice asks for a measured delta".into(),
        );
    }

    let hook_len = post.hook.chars().count();
    if hook_len == 0 {
        fail!("hook-empty", "no hook".into());
    } else if hook_len > 210 {
        warn!(
            "hook-long",
            format!("hook is {hook_len} chars; the feed truncates around 210"),
        );
    }

    let first_body_line = post.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if !first_body_line.is_empty() && first_body_line.trim() == post.hook.trim() {
        warn!(
            "hook-repeated",
            "the body repeats the hook — the hook is already the first line".into(),
        );
    }

    if post.body.lines().any(|l| !l.is_empty() && l.trim().is_empty() && l != " ") {
        warn!(
            "whitespace-line",
            "a line contains only spaces — use an empty line or a zero-width spacer".into(),
        );
    }

    if !post.artifact.trim().is_empty() && !post.body.contains(post.artifact.trim()) {
        warn!(
            "artifact-not-linked",
            "the artifact is not linked in the body".into(),
        );
    }

    issues
}
