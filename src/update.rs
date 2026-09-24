//! Best-effort release notices. This module never opens a ledger or installs code.

use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command};

const RELEASE_API: &str = "https://api.github.com/repos/yuzheng14/claw-expense/releases/latest";
const RELEASE_PAGE: &str = "https://github.com/yuzheng14/claw-expense/releases/tag/";
const CHECK_INTERVAL: u64 = 24 * 60 * 60;
const CHECK_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_RESPONSE: u64 = 64 * 1024;
const MAX_CACHE: u64 = 4096;
const CACHE_FILE: &str = "update-check.json";

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    schema: u8,
    checked_at: u64,
    latest_tag: Option<String>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

pub async fn warn_if_available() {
    let Some(directory) = cache_directory() else {
        return;
    };
    let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return;
    };
    if let Some(message) = check(
        &directory,
        env!("CARGO_PKG_VERSION"),
        elapsed.as_secs(),
        fetch_latest(),
    )
    .await
    {
        // Do not panic (or fail the command) if the consumer closed stderr.
        let _ = writeln!(io::stderr().lock(), "{message}");
    }
}

fn cache_directory() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("CLAW_EXPENSE_UPDATE_CACHE_DIR") {
        return (!directory.is_empty()).then(|| PathBuf::from(directory));
    }
    directories::ProjectDirs::from("", "", "claw-expense").map(|dirs| dirs.cache_dir().to_owned())
}

async fn check(
    directory: &Path,
    current: &str,
    now: u64,
    fetch: impl Future<Output = Option<String>>,
) -> Option<String> {
    fs::create_dir_all(directory).ok()?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(directory.join("update-check.lock")).ok()?;
    // No waiting for another CLI process. The OS releases the lock even on a crash.
    lock.try_lock().ok()?;
    let mut cache = read_cache(directory).unwrap_or_default();
    if cache.schema != 1
        || !now
            .checked_sub(cache.checked_at)
            .is_some_and(|age| age < CHECK_INTERVAL)
    {
        cache.schema = 1;
        cache.checked_at = now;
        // Persist the attempt first: offline/failed checks and interrupted commands
        // must not retry on every invocation. If caching fails, skip networking.
        write_cache(directory, &cache).ok()?;
        if let Some(tag) = fetch.await {
            cache.latest_tag = Some(tag);
            let _ = write_cache(directory, &cache);
        }
    }
    notice(current, cache.latest_tag.as_deref()?)
}

fn read_cache(directory: &Path) -> Option<Cache> {
    let path = directory.join(CACHE_FILE);
    if !path.symlink_metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_CACHE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CACHE {
        return None;
    }
    let cache: Cache = serde_json::from_slice(&bytes).ok()?;
    (cache.schema == 1).then_some(cache)
}

fn write_cache(directory: &Path, cache: &Cache) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, cache)?;
    file.flush()?;
    file.persist(directory.join(CACHE_FILE))?;
    Ok(())
}

fn stable_version(tag: &str) -> Option<Version> {
    if tag.len() > 128 {
        return None;
    }
    let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    version.pre.is_empty().then_some(version)
}

fn notice(current: &str, latest_tag: &str) -> Option<String> {
    let current = Version::parse(current).ok()?;
    let latest = stable_version(latest_tag)?;
    if !latest.cmp_precedence(&current).is_gt() {
        return None;
    }
    Some(format!(
        "[WARN] 有更新可用：claw-expense {current} → {latest}；请手动更新：{RELEASE_PAGE}{latest_tag}（不会自动升级）"
    ))
}

async fn fetch_latest() -> Option<String> {
    // macOS includes curl; missing curl on other source-build platforms simply
    // disables online checks. No shell, GitHub token, ledger data or curlrc is used.
    let mut command = Command::new("curl");
    command.args([
        "--disable", // Must be first: do not load ~/.curlrc.
        "--silent",
        "--fail",
        "--proto",
        "=https",
        "--connect-timeout",
        "1",
        "--max-time",
        "1.5",
        "--max-filesize",
        "65536",
        "--header",
        "Accept: application/vnd.github+json",
        "--header",
        "X-GitHub-Api-Version: 2022-11-28",
        "--user-agent",
        concat!("claw-expense/", env!("CARGO_PKG_VERSION")),
        "--write-out",
        "\n%{http_code}",
        RELEASE_API,
    ]);
    fetch_with(command, CHECK_TIMEOUT).await
}

async fn fetch_with(mut command: Command, budget: Duration) -> Option<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let response = tokio::time::timeout(budget, async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()?
            .take(MAX_RESPONSE + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() as u64 > MAX_RESPONSE || !child.wait().await.ok()?.success() {
            return None;
        }
        let separator = bytes.iter().rposition(|byte| *byte == b'\n')?;
        if &bytes[separator + 1..] != b"200" {
            return None;
        }
        let release: Release = serde_json::from_slice(&bytes[..separator]).ok()?;
        if release.draft || release.prerelease {
            return None;
        }
        stable_version(&release.tag_name)?;
        Some(release.tag_name)
    })
    .await
    .ok()
    .flatten();
    // Also clean up a process that returned too much data or never closed stdout.
    // start_kill is nonblocking; kill_on_drop remains a fallback on every exit.
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn compares_semver_precedence_not_strings_or_build_metadata() {
        assert!(notice("0.9.0", "v0.10.0").is_some());
        assert!(notice("1.0.0-rc.1", "v1.0.0").is_some());
        assert!(notice("1.0.0", "1.1.0").is_some());
        for (current, latest) in [
            ("1.0.0", "v1.0.0"),
            ("2.0.0", "v1.9.9"),
            ("1.0.0+build.1", "v1.0.0+build.9"),
            ("1.0.0", "v2.0.0-rc.1"),
            ("1.0.0", "v2.0"),
            ("1.0.0", "v2.0.0\nspoof"),
            ("1.0.0", "v2.0.0\u{1b}[31m"),
            ("invalid", "v2.0.0"),
        ] {
            assert!(notice(current, latest).is_none(), "{current} {latest}");
        }
    }

    #[tokio::test]
    async fn caches_a_success_and_stops_warning_after_upgrade() {
        let directory = tempfile::tempdir().unwrap();
        let message = check(directory.path(), "0.1.0", NOW, async {
            Some("v0.2.0".to_owned())
        })
        .await
        .unwrap();
        assert!(message.contains("[WARN] 有更新可用"));
        assert!(message.contains(&format!("{RELEASE_PAGE}v0.2.0")));
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            let result = check(directory.path(), version, NOW + 1, async {
                panic!("fresh cache must not fetch")
            })
            .await;
            assert_eq!(result.is_some(), version == "0.1.0");
        }
    }

    #[tokio::test]
    async fn failed_checks_are_throttled_and_retry_at_expiry() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            check(directory.path(), "0.1.0", NOW, async { None })
                .await
                .is_none()
        );
        assert!(
            check(directory.path(), "0.1.0", NOW + CHECK_INTERVAL - 1, async {
                panic!("failed attempt still has a cooldown")
            })
            .await
            .is_none()
        );
        assert!(
            check(directory.path(), "0.1.0", NOW + CHECK_INTERVAL, async {
                Some("v0.2.0".to_owned())
            })
            .await
            .is_some()
        );
    }

    #[tokio::test]
    async fn refresh_failure_retains_the_last_known_release() {
        let directory = tempfile::tempdir().unwrap();
        check(directory.path(), "0.1.0", NOW, async {
            Some("v0.2.0".to_owned())
        })
        .await;
        assert!(
            check(directory.path(), "0.1.0", NOW + CHECK_INTERVAL, async {
                None
            })
            .await
            .is_some()
        );
        assert_eq!(
            read_cache(directory.path()).unwrap().checked_at,
            NOW + CHECK_INTERVAL
        );
    }

    #[tokio::test]
    async fn invalid_or_future_cache_does_not_disable_checks_forever() {
        let directory = tempfile::tempdir().unwrap();
        for content in [
            "not json".to_owned(),
            format!(r#"{{"schema":99,"checked_at":{NOW},"latest_tag":"v99.0.0"}}"#),
            format!(
                r#"{{"schema":1,"checked_at":{},"latest_tag":null}}"#,
                NOW + 1
            ),
            "x".repeat(MAX_CACHE as usize + 1),
        ] {
            fs::write(directory.path().join(CACHE_FILE), content).unwrap();
            assert!(
                check(directory.path(), "0.1.0", NOW, async {
                    Some("v0.2.0".to_owned())
                })
                .await
                .is_some()
            );
        }
    }

    #[tokio::test]
    async fn unwritable_cache_and_concurrent_checker_skip_network() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            check(file.path(), "0.1.0", NOW, async {
                panic!("cache unavailable")
            })
            .await
            .is_none()
        );
        let directory = tempfile::tempdir().unwrap();
        let lock = File::create(directory.path().join("update-check.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(
            check(directory.path(), "0.1.0", NOW, async {
                panic!("already checking")
            })
            .await
            .is_none()
        );
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn validates_transport_status_release_flags_and_version() {
        for (body, status, expected) in [
            (
                r#"{"tag_name":"v0.2.0","draft":false,"prerelease":false}"#,
                "200",
                Some("v0.2.0"),
            ),
            (
                r#"{"tag_name":"v0.2.0","draft":false,"prerelease":false}"#,
                "302",
                None,
            ),
            (
                r#"{"tag_name":"v0.2.0","draft":false,"prerelease":false}"#,
                "403",
                None,
            ),
            (
                r#"{"tag_name":"v0.2.0","draft":true,"prerelease":false}"#,
                "200",
                None,
            ),
            (
                r#"{"tag_name":"v0.2.0","draft":false,"prerelease":true}"#,
                "200",
                None,
            ),
            (
                r#"{"tag_name":"v0.2.0-rc.1","draft":false,"prerelease":false}"#,
                "200",
                None,
            ),
            (r#"{"tag_name":"v0.2.0"}"#, "200", None),
            ("not-json", "200", None),
        ] {
            let result = fetch_with(
                shell(&format!("printf '%s\\n%s' '{body}' '{status}'")),
                CHECK_TIMEOUT,
            )
            .await;
            assert_eq!(result.as_deref(), expected);
        }
        assert!(fetch_with(shell("exit 22"), CHECK_TIMEOUT).await.is_none());
        assert!(
            fetch_with(
                Command::new("/nonexistent/claw-expense-curl"),
                CHECK_TIMEOUT
            )
            .await
            .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounds_slow_or_oversized_responses() {
        let started = std::time::Instant::now();
        assert!(
            fetch_with(shell("exec sleep 10"), Duration::from_millis(50))
                .await
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            fetch_with(shell("exec head -c 65537 /dev/zero"), CHECK_TIMEOUT)
                .await
                .is_none()
        );
    }
}
