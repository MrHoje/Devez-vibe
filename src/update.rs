use std::{
    env, fs,
    fs::File,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
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
    "업데이트를 새 창 없이 진행하고 다운로드 검증·복구·진행 표시를 추가했습니다.",
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

/// Downloads and validates the release before asking npm to replace the global
/// package. npm stages package changes itself, so the active console can stay open.
pub fn run_self_update() -> Result<()> {
    let stage_path = env::temp_dir().join(format!(
        "devez-vibe-update-stage-{}-{}",
        std::process::id(),
        now_secs()
    ));
    fs::create_dir_all(&stage_path).context("업데이트 임시 폴더를 만들지 못했습니다.")?;

    let result = install_update(&stage_path);
    let _ = fs::remove_dir_all(&stage_path);
    result
}

fn install_update(stage_path: &PathBuf) -> Result<()> {
    let stage = stage_path.to_string_lossy();
    let latest_package = format!("{PACKAGE}@latest");
    let staged = run_npm_with_progress(
        stage_path,
        "download",
        "새 버전 다운로드와 무결성 검사",
        &[
            "install",
            "--prefix",
            &stage,
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--prefer-online",
            &latest_package,
        ],
    )?;
    if !staged {
        anyhow::bail!("새 버전을 내려받지 못해 기존 설치를 유지합니다.");
    }

    let staged_exe = stage_path
        .join("node_modules")
        .join(PACKAGE)
        .join("bin")
        .join("dvz.exe");
    let version = Command::new(&staged_exe)
        .arg("--version")
        .output()
        .context("내려받은 실행 파일을 실행하지 못해 기존 설치를 유지합니다.")?;
    if !version.status.success() {
        anyhow::bail!("내려받은 실행 파일을 검증하지 못해 기존 설치를 유지합니다.");
    }
    let latest = executable_version(&version.stdout)
        .filter(|candidate| parse_version(candidate).is_some())
        .context("내려받은 실행 파일의 버전을 확인하지 못해 기존 설치를 유지합니다.")?;
    println!("Devez Vibe v{CURRENT_VERSION} → v{latest}");

    println!("완료: 실행 파일 검증");
    let package = format!("{PACKAGE}@{latest}");
    let installed = run_npm_with_progress(
        stage_path,
        "install",
        "기존 설치 교체",
        &[
            "install",
            "-g",
            &package,
            "--prefer-online",
            "--no-audit",
            "--no-fund",
        ],
    )?;
    if !installed {
        let current = format!("{PACKAGE}@{CURRENT_VERSION}");
        let restored = run_npm_with_progress(
            stage_path,
            "restore",
            "현재 버전 복구",
            &[
                "install",
                "-g",
                &current,
                "--prefer-online",
                "--no-audit",
                "--no-fund",
            ],
        )?;
        if restored {
            anyhow::bail!("업데이트에 실패해 v{CURRENT_VERSION}으로 복구했습니다.");
        }
        anyhow::bail!("업데이트와 자동 복구에 실패했습니다. 위 npm 오류를 확인하세요.");
    }

    println!("Devez Vibe v{latest} 설치를 마쳤습니다. `dvz`를 다시 실행하세요.");
    Ok(())
}

fn executable_version(output: &[u8]) -> Option<&str> {
    std::str::from_utf8(output).ok()?.split_whitespace().last()
}

fn npm_command() -> Command {
    let mut command = Command::new("npm.cmd");
    if env::var_os("FORCE_COLOR").is_some() {
        command.env_remove("NO_COLOR");
    }
    command
}

fn run_npm_with_progress(
    stage_path: &PathBuf,
    log_name: &str,
    label: &str,
    args: &[&str],
) -> Result<bool> {
    let log_path = stage_path.join(format!("npm-{log_name}.log"));
    let log = File::create(&log_path).context("업데이트 로그를 만들지 못했습니다.")?;
    let mut command = npm_command();
    command
        .args(args)
        .stdout(Stdio::from(
            log.try_clone().context("업데이트 로그를 열지 못했습니다.")?,
        ))
        .stderr(Stdio::from(log));
    let mut child = command
        .spawn()
        .context("npm을 실행하지 못했습니다. Node.js와 npm 설치를 확인하세요.")?;

    let interactive = io::stdout().is_terminal();
    let frames = ['|', '/', '-', '\\'];
    let mut frame = 0;
    loop {
        if let Some(status) = child.try_wait().context("npm 실행 상태를 확인하지 못했습니다.")? {
            if interactive {
                print!("\r");
            }
            if status.success() {
                println!("완료: {label}");
                let _ = fs::remove_file(&log_path);
                return Ok(true);
            }
            println!("실패: {label}");
            if let Ok(details) = fs::read_to_string(&log_path)
                && !details.trim().is_empty()
            {
                eprintln!("오류 상세:\n{}", details.trim());
            }
            return Ok(false);
        }

        if interactive {
            print!("\r진행 중: {label} {}", frames[frame % frames.len()]);
            io::stdout().flush().context("진행 상태를 표시하지 못했습니다.")?;
            frame += 1;
        } else if frame == 0 {
            println!("진행 중: {label}");
            frame = 1;
        }
        thread::sleep(Duration::from_millis(100));
    }
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
    fn extracts_version_from_staged_executable() {
        assert_eq!(executable_version(b"dvz 1.7.73\r\n"), Some("1.7.73"));
        assert_eq!(executable_version(b""), None);
    }
}
