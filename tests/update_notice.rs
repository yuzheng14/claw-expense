#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let curl = bin.join("curl");
        fs::write(
            &curl,
            r#"#!/bin/sh
printf 'called\n' >> "$MOCK_CURL_CALLS"
printf '%s\n' "$@" > "$MOCK_CURL_ARGS"
case "$MOCK_CURL_MODE" in
  fail) exit 22 ;;
  slow) exec /bin/sleep 10 ;;
esac
printf '%s' "$MOCK_CURL_RESPONSE"
"#,
        )
        .unwrap();
        fs::set_permissions(curl, fs::Permissions::from_mode(0o755)).unwrap();
        let cache = root.path().join("cache");
        Self { root, cache }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claw-expense"));
        command
            .env("PATH", self.root.path().join("bin"))
            .env("CLAW_EXPENSE_UPDATE_CACHE_DIR", &self.cache)
            .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "0")
            .env("MOCK_CURL_CALLS", self.root.path().join("calls"))
            .env("MOCK_CURL_ARGS", self.root.path().join("args"))
            .env("MOCK_CURL_MODE", "")
            .env("MOCK_CURL_RESPONSE", release("v999.0.0"))
            .args(["--json", "--db"])
            .arg(self.root.path().join("ledger.sqlite"));
        command
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.root.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn seed_cache(&self) {
        fs::create_dir_all(&self.cache).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        fs::write(
            self.cache.join("update-check.json"),
            json!({"schema": 1, "checked_at": now, "latest_tag": "v999.0.0"}).to_string(),
        )
        .unwrap();
    }
}

fn release(tag: &str) -> String {
    format!(
        "{}\n200",
        json!({"tag_name": tag, "draft": false, "prerelease": false})
    )
}

fn success(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).expect("exactly one JSON response");
    assert_eq!(value["ok"], true);
    value["data"].clone()
}

#[test]
fn warns_on_stderr_and_uses_cache_without_affecting_json() {
    let fixture = Fixture::new();
    let output = fixture.command().arg("init").output().unwrap();
    success(&output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1);
    assert!(stderr.contains("[WARN] 有更新可用"));
    assert!(stderr.contains("https://github.com/yuzheng14/claw-expense/releases/tag/v999.0.0"));
    assert_eq!(fixture.calls(), 1);

    let output = fixture.command().arg("list").output().unwrap();
    assert_eq!(success(&output)["total"], 0);
    assert!(String::from_utf8(output.stderr).unwrap().contains("[WARN]"));
    assert_eq!(fixture.calls(), 1, "fresh cache must avoid a new request");

    let args = fs::read_to_string(fixture.root.path().join("args")).unwrap();
    assert_eq!(args.lines().next(), Some("--disable"));
    assert!(args.contains("=https"));
    assert!(args.contains("--max-time\n1.5"));
    assert!(args.contains("https://api.github.com/repos/yuzheng14/claw-expense/releases/latest"));
    assert!(!args.contains("Authorization"));
    assert!(!args.contains("ledger.sqlite"));
}

#[test]
fn warn_is_also_available_for_human_output() {
    let fixture = Fixture::new();
    fixture.seed_cache();
    let output = Command::new(env!("CARGO_BIN_EXE_claw-expense"))
        .env("CLAW_EXPENSE_NO_UPDATE_CHECK", "0")
        .env("CLAW_EXPENSE_UPDATE_CACHE_DIR", &fixture.cache)
        .args(["--db"])
        .arg(fixture.root.path().join("ledger.sqlite"))
        .arg("init")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8(output.stdout).unwrap().contains("WARN"));
    assert!(String::from_utf8(output.stderr).unwrap().contains("[WARN]"));
}

#[test]
fn flag_and_environment_disable_both_requests_and_cached_notices() {
    for seeded in [false, true] {
        for setting in ["flag", "1", "true"] {
            let fixture = Fixture::new();
            if seeded {
                fixture.seed_cache();
            }
            let before = fs::read(fixture.cache.join("update-check.json")).ok();
            let mut command = fixture.command();
            command.arg("init");
            if setting == "flag" {
                // Global flags must also work after the subcommand.
                command.arg("--no-update-check");
            } else {
                command.env("CLAW_EXPENSE_NO_UPDATE_CHECK", setting);
            }
            let output = command.output().unwrap();
            success(&output);
            assert!(output.stderr.is_empty());
            assert_eq!(fixture.calls(), 0);
            assert_eq!(
                fs::read(fixture.cache.join("update-check.json")).ok(),
                before
            );
            assert!(!fixture.cache.join("update-check.lock").exists());
        }
    }
}

#[test]
fn help_version_and_failed_commands_do_not_check_or_warn() {
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["typo"],
        vec!["list"],
    ] {
        let fixture = Fixture::new();
        fixture.seed_cache();
        let output = fixture.command().args(&args).output().unwrap();
        assert_eq!(
            output.status.success(),
            matches!(args[0], "--help" | "--version")
        );
        assert!(!String::from_utf8(output.stderr).unwrap().contains("[WARN]"));
        assert_eq!(fixture.calls(), 0);
        assert!(!fixture.cache.join("update-check.lock").exists());
        assert!(!fixture.root.path().join("ledger.sqlite").exists());
    }
}

#[test]
fn latest_equal_older_and_invalid_releases_are_silent() {
    for tag in [
        env!("CARGO_PKG_VERSION"),
        "v0.0.0",
        "v999.0.0-rc.1",
        "invalid",
    ] {
        let fixture = Fixture::new();
        let output = fixture
            .command()
            .env("MOCK_CURL_RESPONSE", release(tag))
            .arg("init")
            .output()
            .unwrap();
        success(&output);
        assert!(output.stderr.is_empty(), "{tag}: {output:?}");
    }
}

#[test]
fn failed_and_timed_out_checks_preserve_success_and_are_throttled() {
    for mode in ["fail", "slow"] {
        let fixture = Fixture::new();
        let started = Instant::now();
        let output = fixture
            .command()
            .env("MOCK_CURL_MODE", mode)
            .arg("init")
            .output()
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "check must be bounded"
        );
        success(&output);
        assert!(output.stderr.is_empty());
        let output = fixture.command().arg("list").output().unwrap();
        assert_eq!(success(&output)["total"], 0);
        assert!(output.stderr.is_empty());
        assert_eq!(fixture.calls(), 1);
    }
}

#[test]
fn stdin_records_and_idempotency_are_unaffected_by_a_notice() {
    let fixture = Fixture::new();
    success(&fixture.command().arg("init").output().unwrap());
    let mut original_id = None;
    for replayed in [false, true] {
        let mut child = fixture
            .command()
            .args(["record", "--request-id", "update-notice-record"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"kind":"expense","amount":"0.10","date":"2026-09-24"}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let data = success(&output);
        assert_eq!(data["transaction"]["amount"], "0.10");
        assert_eq!(data["replayed"], replayed);
        if let Some(id) = &original_id {
            assert_eq!(&data["transaction"]["id"], id);
        } else {
            original_id = Some(data["transaction"]["id"].clone());
        }
        assert!(String::from_utf8(output.stderr).unwrap().contains("[WARN]"));
    }
    let output = fixture.command().arg("summary").output().unwrap();
    assert_eq!(success(&output)["count"], 1);
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn an_unavailable_cache_does_not_fail_a_successful_operation() {
    let fixture = Fixture::new();
    fs::write(&fixture.cache, "not a directory").unwrap();
    let output = fixture.command().arg("init").output().unwrap();
    success(&output);
    assert!(output.stderr.is_empty());
    assert_eq!(fixture.calls(), 0);
}
