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

/// Release notes kept with the build for the changelog, but not shown at startup.
#[allow(dead_code)]
pub const RELEASE_NOTES: &[&str] = &[
    "선택지 창이 뜨는 순간 한글이 다른 글자로 보이던 문제를 수정했습니다.",
    "이모지가 들어간 줄 때문에 화면이 한 줄씩 어긋나 겹쳐 보이던 문제를 수정했습니다.",
    "화면 출력 기록 스위치를 추가했습니다.",
    "가운뎃점과 화살표, 이모지의 칸 수를 실제 화면과 맞춰 줄이 밀리고 겹치던 문제를 수정했습니다.",
    "카드와 선택지 창의 오른쪽 세로선이 어긋나 보이던 문제를 수정했습니다.",
    "이모지가 많은 줄에서 카드 배경이 오른쪽부터 사라지던 문제를 수정했습니다.",
    "새 대화를 열 때 현재 공급자의 모델과 스킬, 플러그인 목록을 다시 불러오도록 개선했습니다.",
    "이모지 행의 오른쪽 배경 경계와 상태줄의 effort 안내가 잘리던 문제를 수정했습니다.",
    "구버전 DevezCode에서는 이모지 행의 배경을 직접 채우고, 새 폭 프로필에서는 발바닥 이모지를 2칸으로 맞췄습니다.",
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
    let script_path = env::temp_dir().join(format!(
        "devez-vibe-update-{}-{}.cmd",
        std::process::id(),
        now_secs()
    ));
    fs::write(&script_path, update_script()).context("업데이트 스크립트를 만들지 못했습니다.")?;

    println!("Devez Vibe v{CURRENT_VERSION} · 최신 버전 설치를 시작합니다.");
    println!("새 창이 열린 뒤 실행 중인 모든 Devez Vibe 세션이 종료되기를 기다립니다.");
    println!("모든 세션이 닫히면 `npm install -g {PACKAGE}@latest`가 자동으로 실행됩니다.");
    println!("설치가 끝나면 `dvz`를 다시 실행하세요.");

    let result = Command::new("cmd")
        .args([
            "/C",
            "start",
            "Devez Vibe update",
            "cmd",
            "/C",
            &script_path.to_string_lossy(),
        ])
        .spawn();
    if let Err(error) = result {
        let _ = fs::remove_file(&script_path);
        return Err(error)
            .context("업데이트 프로세스를 시작하지 못했습니다. npm이 설치되어 있는지 확인하세요.");
    }
    Ok(())
}

fn update_script() -> String {
    format!(
        "@echo off\r\n\
chcp 65001 >nul\r\n\
setlocal\r\n\
title Devez Vibe update\r\n\
where npm.cmd >nul 2>nul\r\n\
if errorlevel 1 (\r\n\
  echo npm을 찾지 못했습니다. Node.js와 npm 설치를 확인하세요.\r\n\
  echo.\r\n\
  pause\r\n\
  del \"%~f0\"\r\n\
  exit /b 1\r\n\
)\r\n\
echo 실행 중인 모든 Devez Vibe 세션이 종료되기를 기다리는 중입니다.\r\n\
echo 열린 세션을 모두 닫으면 업데이트가 자동으로 시작됩니다.\r\n\
:wait_for_dvz\r\n\
tasklist /FI \"IMAGENAME eq dvz.exe\" /NH 2>nul | find /I \"dvz.exe\" >nul\r\n\
if not errorlevel 1 (\r\n\
  timeout /t 1 /nobreak >nul\r\n\
  goto wait_for_dvz\r\n\
)\r\n\
echo.\r\n\
echo Devez Vibe 최신 버전을 설치합니다.\r\n\
set \"install_try=0\"\r\n\
:install\r\n\
set /a install_try+=1\r\n\
call npm.cmd install -g {PACKAGE}@latest --prefer-online\r\n\
if not errorlevel 1 goto update_done\r\n\
if %install_try% GEQ 12 goto update_failed\r\n\
echo npm 레지스트리 전파를 기다린 뒤 다시 시도합니다. ^(%install_try%/12^)\r\n\
timeout /t 5 /nobreak >nul\r\n\
goto install\r\n\
:update_done\r\n\
set \"update_exit=0\"\r\n\
goto update_finished\r\n\
:update_failed\r\n\
set \"update_exit=1\"\r\n\
:update_finished\r\n\
echo.\r\n\
if not \"%update_exit%\"==\"0\" (\r\n\
  echo 업데이트에 실패했습니다. 위 오류를 확인하세요.\r\n\
) else (\r\n\
  echo 업데이트가 완료되었습니다. dvz를 다시 실행하세요.\r\n\
)\r\n\
echo.\r\n\
pause\r\n\
del \"%~f0\"\r\n\
exit /b %update_exit%\r\n"
    )
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

    #[test]
    fn updater_waits_for_every_dvz_process_before_running_npm() {
        let script = update_script();
        let wait = script.find("IMAGENAME eq dvz.exe").expect("dvz wait loop");
        let install = script
            .find("npm.cmd install -g devez-vibe@latest --prefer-online")
            .expect("npm install");

        assert!(wait < install);
        assert!(script.contains("goto wait_for_dvz"));
        assert!(script.contains("2>nul | find /I \"dvz.exe\""));
        assert!(script.contains("if %install_try% GEQ 12 goto update_failed"));
        assert!(script.contains("goto install"));
        assert!(script.contains("del \"%~f0\""));
    }
}
