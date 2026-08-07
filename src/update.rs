use std::{
    env, fs,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// npm package that publishes the `dvz` binary.
const PACKAGE: &str = "devez-vibe";
/// Registry lookups are cached so startup stays offline-friendly.
const CHECK_INTERVAL_SECS: u64 = 60 * 60 * 12;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Release notes kept with the build. The welcome card no longer prints them, so
/// nothing reads this today; rewrite it each release for the changelog.
#[allow(dead_code)]
pub const RELEASE_NOTES: &[&str] = &[
    "1.2.9 정기 배포: 최근 안정화 개선을 포함한 최신 실행 파일을 제공합니다.",
    "새 대화 시작과 Provider 전환 중에도 선택한 설정을 안정적으로 유지합니다.",
    "Claude 진행 안내는 요청 언어에 맞춰 자연스럽게 표시됩니다.",
];

/// Latest published version, only when it is newer than the running build.
pub async fn check_for_update() -> Option<String> {
    if env::var_os("DEVEZ_VIBE_NO_UPDATE_CHECK").is_some() {
        return None;
    }

    let cache = cache_path();
    let latest = match cache.as_ref().and_then(read_cache) {
        Some(latest) => latest,
        None => {
            let latest = fetch_latest().await?;
            if let Some(path) = cache.as_ref() {
                write_cache(path, &latest);
            }
            latest
        }
    };

    is_newer(&latest, CURRENT_VERSION).then_some(latest)
}

/// Hands the upgrade to a detached console. Windows keeps a lock on the running
/// executable, so npm can only replace it once this process has exited.
pub fn run_self_update() -> Result<()> {
    println!("Devez Vibe v{CURRENT_VERSION} · 최신 버전 설치를 시작합니다.");
    println!("새 창에서 `npm install -g {PACKAGE}@latest`가 실행됩니다.");
    println!("설치가 끝나면 `dvz`를 다시 실행하세요.");

    Command::new("cmd")
        .args([
            "/C",
            "start",
            "Devez Vibe update",
            "cmd",
            "/C",
            &format!(
                "timeout /t 1 /nobreak >nul & npm install -g {PACKAGE}@latest & echo. & pause"
            ),
        ])
        .spawn()
        .context("업데이트 프로세스를 시작하지 못했습니다. npm이 설치되어 있는지 확인하세요.")?;
    Ok(())
}

async fn fetch_latest() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("devez-vibe/{CURRENT_VERSION}"))
        .build()
        .ok()?;
    let body = client
        .get(format!("https://registry.npmjs.org/{PACKAGE}/latest"))
        .header("accept", "application/vnd.npm.install-v1+json")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    serde_json::from_str::<Value>(&body)
        .ok()?
        .get("version")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn cache_path() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("DevezCode")
            .join("update-check.json")
    })
}

/// Cached version, or `None` when the entry is missing or stale.
fn read_cache(path: &PathBuf) -> Option<String> {
    let root = fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())?;
    let checked_at = root.get("checkedAt").and_then(Value::as_u64)?;
    let latest = root.get("latest").and_then(Value::as_str)?;
    (now_secs().saturating_sub(checked_at) < CHECK_INTERVAL_SECS).then(|| latest.to_owned())
}

fn write_cache(path: &PathBuf, latest: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = json!({ "checkedAt": now_secs(), "latest": latest });
    let _ = fs::write(path, payload.to_string());
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// Numeric `major.minor.patch` core; pre-release and build metadata are ignored.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_newer_versions() {
        assert!(is_newer("0.11.9", "0.4.9"));
        assert!(is_newer("1.0.0", "0.11.9"));
        assert!(is_newer("0.4.10", "0.4.9"));
    }

    #[test]
    fn ignores_same_or_older_versions() {
        assert!(!is_newer("0.4.9", "0.4.9"));
        assert!(!is_newer("0.4.8", "0.4.9"));
        assert!(!is_newer("not-a-version", "0.4.9"));
    }

    #[test]
    fn parses_prerelease_core() {
        assert_eq!(parse_version("v1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
    }
}
