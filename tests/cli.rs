//! The invariants that must never regress, proven without the network.
//!
//! Every test runs the real binary against a throwaway config, so nothing
//! touches the Keychain, the real queue or LinkedIn.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_mk-feed");

struct Sandbox {
    dir: PathBuf,
    config: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mk-feed-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("drafts")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(
            dir.join("drafts/STYLE.md"),
            "# STYLE\n\nFour parts, 3000 chars, 3 images.\n",
        )
        .unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "client_id = \"test\"\n\
                 queue = \"{}\"\n\
                 default_mode = \"confirm\"\n\
                 api_version = \"202601\"\n\
                 api_version_stale_after = \"2099-01-01\"\n\
                 redirect_port = 8799\n\
                 visibility = \"PUBLIC\"\n\
                 timezone = \"America/Lima\"\n\
                 notify = false\n\
                 state_dir = \"{}\"\n",
                dir.join("drafts").display(),
                dir.join("state").display()
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .expect("mk-feed runs")
    }

    fn drafts(&self) -> PathBuf {
        self.dir.join("drafts")
    }

    fn write_post(&self, id: &str, frontmatter_extra: &str, body: &str) {
        let body = body.to_string();
        let content = format!(
            "---\nid: \"{id}\"\nhook: \"un hook\"\nartifact: \"https://example.com/commit/abc123\"\n\
             state: \"ready\"\nmode: \"confirm\"\nat: \"\"\nvisibility: \"PUBLIC\"\nmedia: []\n\
             urn: \"\"\nposted_at: \"\"\nnote: \"\"\n{frontmatter_extra}---\n{body}\n"
        );
        std::fs::write(self.drafts().join(format!("{id}.md")), content).unwrap();
    }

    fn read_post(&self, id: &str) -> String {
        std::fs::read_to_string(self.drafts().join(format!("{id}.md"))).unwrap()
    }
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn add_then_check_is_clean_with_an_artifact() {
    let sandbox = Sandbox::new("add");
    let body = "El delta fue 40% a 96% en 3 corridas.";
    let added = sandbox.run(&[
        "add",
        "--hook",
        "un hook discutible",
        "--artifact",
        "https://example.com/commit/abc123",
        "--body",
        body,
    ]);
    assert_eq!(code(&added), 0, "{}", stdout(&added));

    let checked = sandbox.run(&["check", "un-hook-discutible"]);
    // 4 = warnings only (the artifact is not linked in the body); never 3.
    assert!(matches!(code(&checked), 0 | 4), "{}", stdout(&checked));
}

#[test]
fn a_post_without_an_artifact_is_refused() {
    let sandbox = Sandbox::new("artifact");
    sandbox.write_post("no-artifact", "", "Cuerpo con un número 42 y nada más.");
    let path = sandbox.drafts().join("no-artifact.md");
    let raw = std::fs::read_to_string(&path).unwrap().replace(
        "artifact: \"https://example.com/commit/abc123\"",
        "artifact: \"\"",
    );
    std::fs::write(&path, raw).unwrap();

    let checked = sandbox.run(&["check", "no-artifact"]);
    assert_eq!(code(&checked), 3, "the artifact rule is a hard fail");

    let sent = sandbox.run(&["send", "no-artifact", "--dry-run"]);
    assert_eq!(code(&sent), 3, "send must refuse before any network call");
}

#[test]
fn boot_reconcile_turns_attempting_into_unknown() {
    let sandbox = Sandbox::new("reconcile");
    sandbox.write_post("mid-flight", "state: \"attempting\"\n", "cuerpo 12");
    // The helper wrote state: ready first; overwrite the whole file to be sure.
    sandbox.write_post(
        "mid-flight",
        "",
        "cuerpo 12",
    );
    let path = sandbox.drafts().join("mid-flight.md");
    let raw = std::fs::read_to_string(&path)
        .unwrap()
        .replace("state: \"ready\"", "state: \"attempting\"");
    std::fs::write(&path, raw).unwrap();

    let out = sandbox.run(&["daemon", "--once"]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));

    let after = sandbox.read_post("mid-flight");
    assert!(
        after.contains("state: \"unknown\""),
        "a dead attempt becomes unknown, never retried:\n{after}"
    );
    assert!(after.contains("outcome unknown"));
}

#[test]
fn confirm_mode_waits_for_a_human() {
    let sandbox = Sandbox::new("confirm");
    sandbox.write_post("due-now", "", "cuerpo 7");
    let path = sandbox.drafts().join("due-now.md");
    let raw = std::fs::read_to_string(&path).unwrap().replace(
        "state: \"ready\"",
        "state: \"scheduled\"",
    ).replace(
        "at: \"\"",
        "at: \"2020-01-01T09:00:00-05:00\"",
    );
    std::fs::write(&path, raw).unwrap();

    let out = sandbox.run(&["daemon", "--once"]);
    assert_eq!(code(&out), 0);

    let after = sandbox.read_post("due-now");
    assert!(
        after.contains("state: \"scheduled\""),
        "confirm mode must not publish on its own:\n{after}"
    );
    let log = std::fs::read_to_string(sandbox.dir.join("state/publish.jsonl")).unwrap();
    assert!(log.contains("awaiting-confirm"), "{log}");

    // A second tick must not notify twice for the same instant.
    let out = sandbox.run(&["daemon", "--once"]);
    assert_eq!(code(&out), 0);
    let log = std::fs::read_to_string(sandbox.dir.join("state/publish.jsonl")).unwrap();
    assert_eq!(log.matches("awaiting-confirm").count(), 1, "{log}");
}

#[test]
fn style_is_data_beside_the_drafts() {
    let sandbox = Sandbox::new("style");
    let out = sandbox.run(&["style"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("Four parts"));

    std::fs::remove_file(sandbox.drafts().join("STYLE.md")).unwrap();
    let missing = sandbox.run(&["style"]);
    assert_eq!(code(&missing), 3, "a missing STYLE.md is a refusal, not a crash");
}

#[test]
fn an_unknown_state_filter_is_a_usage_error() {
    let sandbox = Sandbox::new("usage");
    let out = sandbox.run(&["list", "--state", "published"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn queue_reads_round_trip_through_frontmatter() {
    let sandbox = Sandbox::new("roundtrip");
    sandbox.write_post("round-trip", "", "línea uno\n\nlínea dos 33");
    let shown = sandbox.run(&["show", "round-trip", "--json"]);
    assert_eq!(code(&shown), 0);
    let json: serde_json::Value = serde_json::from_str(&stdout(&shown)).unwrap();
    assert_eq!(json["id"], "round-trip");
    assert_eq!(json["body"], "línea uno\n\nlínea dos 33");
    assert_eq!(json["state"], "ready");

    // Scheduling rewrites the file and keeps the body byte-identical.
    let scheduled = sandbox.run(&["schedule", "round-trip", "--at", "+2h"]);
    assert_eq!(code(&scheduled), 0);
    let after = sandbox.read_post("round-trip");
    assert!(after.contains("state: \"scheduled\""));
    assert!(after.contains("línea uno\n\nlínea dos 33"));
}

#[test]
fn doctor_fails_when_the_linkedin_app_is_missing() {
    let sandbox = Sandbox::new("doctor");
    let out = sandbox.run(&["doctor", "--json"]);
    // client_id is present in the sandbox, but there are no tokens: still a fail.
    assert_eq!(code(&out), 1);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json["ok"], false);
    let names: Vec<&str> = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["ok"] == false)
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"access_token"), "{names:?}");
}

#[allow(dead_code)]
fn exists(path: &Path) -> bool {
    path.exists()
}

#[test]
fn marking_a_manual_post_closes_the_loop() {
    let sandbox = Sandbox::new("mark");
    sandbox.write_post("pasted", "", "cuerpo 9");
    sandbox.run(&["schedule", "pasted", "--at", "+1h"]);

    let out = sandbox.run(&["mark", "pasted", "--url", "https://www.linkedin.com/feed/"]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));

    let after = sandbox.read_post("pasted");
    assert!(after.contains("state: \"posted\""), "{after}");
    assert!(after.contains("published by hand"), "{after}");

    let log = std::fs::read_to_string(sandbox.dir.join("state/publish.jsonl")).unwrap();
    assert!(log.contains("posted-manual"), "{log}");

    // Marking twice is a refusal, not a second publish.
    let again = sandbox.run(&["mark", "pasted"]);
    assert_eq!(code(&again), 3);
}

#[test]
fn a_publish_that_cannot_start_leaves_the_post_where_it_was() {
    let sandbox = Sandbox::new("arm-last");
    sandbox.write_post("no-creds", "", "cuerpo 5");
    sandbox.run(&["schedule", "no-creds", "--at", "+1h"]);

    // No tokens in the Keychain for this sandbox: the send fails before the POST.
    let out = sandbox.run(&["send", "no-creds"]);
    assert_eq!(code(&out), 1, "a missing token is an error, not a refusal");

    let after = sandbox.read_post("no-creds");
    assert!(
        after.contains("state: \"scheduled\""),
        "a send that never reached the network must not strand the post:\n{after}"
    );
    assert!(!after.contains("attempting"), "{after}");
}
