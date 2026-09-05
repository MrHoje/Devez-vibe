use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::Result;
use tokio::{process::Command, time::timeout};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const CODEX_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Level {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "정상",
            Self::Warn => "경고",
            Self::Fail => "오류",
            Self::Skip => "건너뜀",
        }
    }
}

#[derive(Debug)]
struct Check {
    level: Level,
    name: &'static str,
    detail: String,
}

impl Check {
    fn new(level: Level, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level,
            name,
            detail: detail.into(),
        }
    }
}

#[derive(Debug)]
struct Probe {
    found: bool,
    success: bool,
    detail: String,
}

pub async fn run(
    codex_path: &Path,
    open_code_path: &Path,
    node_path: &Path,
    claude_path: &Path,
    cwd: &Path,
) -> Result<bool> {
    let mut checks = vec![
        Check::new(
            Level::Pass,
            "버전",
            format!("Devez Vibe v{}", env!("CARGO_PKG_VERSION")),
        ),
        executable_check(),
        working_directory_check(cwd),
        update_pointer_check(),
    ];

    let (codex, node, claude, open_code) = tokio::join!(
        probe_command(codex_path, &["--version"]),
        probe_command(node_path, &["--version"]),
        probe_command(claude_path, &["--version"]),
        probe_command(open_code_path, &["--version"]),
    );

    let mut usable_providers = 0;
    checks.push(command_check("Codex", &codex));
    if codex.success {
        let (check, usable) = codex_runtime_check(codex_path).await;
        checks.push(check);
        usable_providers += usize::from(usable);
    }

    checks.push(command_check("Claude Code", &claude));
    if claude.found {
        let node_check = node_check(&node);
        let node_ok = node_check.level == Level::Pass;
        checks.push(node_check);
        let bridge_ok = if node_ok {
            let check = bridge_check(node_path, cwd).await;
            let passed = check.level == Level::Pass;
            checks.push(check);
            passed
        } else {
            false
        };
        if claude.success && node_ok && bridge_ok {
            usable_providers += 1;
        }
    } else {
        checks.push(Check::new(
            Level::Skip,
            "Node.js와 Claude 연결 파일",
            "Claude Code를 사용하지 않아 확인하지 않음",
        ));
    }

    checks.push(command_check("OpenCode", &open_code));
    if open_code.success {
        usable_providers += 1;
    }

    if usable_providers == 0 {
        checks.push(Check::new(
            Level::Fail,
            "사용 가능한 제공자",
            "Codex, Claude Code, OpenCode 중 실행 가능한 항목이 없음",
        ));
    } else {
        checks.push(Check::new(
            Level::Pass,
            "사용 가능한 제공자",
            format!("{usable_providers}개"),
        ));
    }

    print_report(&checks);
    Ok(!checks.iter().any(|check| check.level == Level::Fail))
}

fn executable_check() -> Check {
    match env::current_exe() {
        Ok(path) => Check::new(Level::Pass, "실행 파일", path.display().to_string()),
        Err(error) => Check::new(Level::Fail, "실행 파일", error.to_string()),
    }
}

fn working_directory_check(cwd: &Path) -> Check {
    if cwd.is_dir() {
        Check::new(Level::Pass, "작업 폴더", cwd.display().to_string())
    } else {
        Check::new(Level::Fail, "작업 폴더", "폴더를 찾을 수 없음")
    }
}

fn update_pointer_check() -> Check {
    let Some(root) = env::var_os("LOCALAPPDATA") else {
        return Check::new(Level::Warn, "업데이트 전환", "LOCALAPPDATA를 찾을 수 없음");
    };
    let pointer = PathBuf::from(root)
        .join("DevezVibe")
        .join("current-executable.txt");
    if !pointer.exists() {
        return Check::new(Level::Pass, "업데이트 전환", "별도 활성 버전 없음");
    }
    match fs::read_to_string(&pointer) {
        Ok(value) => {
            let target = PathBuf::from(value.trim());
            if target.is_absolute() && target.is_file() {
                Check::new(Level::Pass, "업데이트 전환", target.display().to_string())
            } else {
                Check::new(
                    Level::Warn,
                    "업데이트 전환",
                    "활성 버전 경로가 유효하지 않음",
                )
            }
        }
        Err(error) => Check::new(Level::Warn, "업데이트 전환", error.to_string()),
    }
}

fn command_check(name: &'static str, probe: &Probe) -> Check {
    if probe.success {
        Check::new(Level::Pass, name, &probe.detail)
    } else if probe.found {
        Check::new(Level::Warn, name, &probe.detail)
    } else {
        Check::new(Level::Skip, name, &probe.detail)
    }
}

fn node_check(probe: &Probe) -> Check {
    if !probe.success {
        return Check::new(Level::Warn, "Node.js", &probe.detail);
    }
    match node_major(&probe.detail) {
        Some(version) if version >= 18 => Check::new(Level::Pass, "Node.js", &probe.detail),
        Some(version) => Check::new(
            Level::Warn,
            "Node.js",
            format!("v{version}: Claude 연결에는 v18 이상 필요"),
        ),
        None => Check::new(Level::Warn, "Node.js", "버전을 판별할 수 없음"),
    }
}

async fn bridge_check(node_path: &Path, cwd: &Path) -> Check {
    let bridge = match crate::claude::resolve_bridge_path(cwd) {
        Ok(path) => path,
        Err(error) => return Check::new(Level::Warn, "Claude 연결 파일", error.to_string()),
    };
    let probe = probe_command_paths(node_path, &[Path::new("--check"), &bridge]).await;
    if probe.success {
        Check::new(
            Level::Pass,
            "Claude 연결 파일",
            bridge.display().to_string(),
        )
    } else {
        Check::new(Level::Warn, "Claude 연결 파일", probe.detail)
    }
}

async fn codex_runtime_check(codex_path: &Path) -> (Check, bool) {
    let server = match crate::app_server::AppServer::spawn(codex_path, None).await {
        Ok(server) => server,
        Err(error) => {
            return (
                Check::new(Level::Warn, "Codex 연결", one_line(&error.to_string())),
                false,
            );
        }
    };
    let initialized = timeout(CODEX_TIMEOUT, server.initialize()).await;
    let check = match initialized {
        Ok(Ok(_)) => Check::new(Level::Pass, "Codex 연결", "app-server 초기화 성공"),
        Ok(Err(error)) => Check::new(Level::Warn, "Codex 연결", one_line(&error.to_string())),
        Err(_) => Check::new(Level::Warn, "Codex 연결", "초기화 시간이 10초를 초과함"),
    };
    let usable = check.level == Level::Pass;
    server.shutdown().await;
    (check, usable)
}

async fn probe_command(path: &Path, args: &[&str]) -> Probe {
    let path_args = args.iter().map(Path::new).collect::<Vec<_>>();
    probe_command_paths(path, &path_args).await
}

async fn probe_command_paths(path: &Path, args: &[&Path]) -> Probe {
    let resolved = crate::app_server::resolve_command(path);
    let found = resolved.is_file();
    let mut command = command_for(&resolved);
    command.args(args).stdin(Stdio::null());
    match timeout(COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Probe {
            found: true,
            success: output.status.success(),
            detail: output_detail(&output.stdout, &output.stderr, output.status.success()),
        },
        Ok(Err(error)) => Probe {
            found,
            success: false,
            detail: if found {
                one_line(&error.to_string())
            } else {
                format!("명령을 찾을 수 없음: {}", path.display())
            },
        },
        Err(_) => Probe {
            found: true,
            success: false,
            detail: "응답 시간이 5초를 초과함".to_owned(),
        },
    }
}

fn command_for(path: &Path) -> Command {
    #[cfg(windows)]
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
    {
        let shell = env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.args(["/d", "/s", "/c"]).arg(path);
        return command;
    }
    Command::new(path)
}

fn output_detail(stdout: &[u8], stderr: &[u8], success: bool) -> String {
    let preferred = if success && !stdout.is_empty() {
        stdout
    } else if !stderr.is_empty() {
        stderr
    } else {
        stdout
    };
    let detail = one_line(&String::from_utf8_lossy(preferred));
    if detail.is_empty() {
        if success {
            "실행 성공".to_owned()
        } else {
            "실행에 실패했으나 오류 메시지가 없음".to_owned()
        }
    } else {
        detail
    }
}

fn one_line(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(240).collect()
}

fn node_major(version: &str) -> Option<u64> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn print_report(checks: &[Check]) {
    println!("Devez Vibe 진단");
    for check in checks {
        println!(
            "{} · {} · {}",
            check.level.label(),
            check.name,
            check.detail
        );
    }
    let count = |level| checks.iter().filter(|check| check.level == level).count();
    println!();
    println!(
        "요약 · 정상 {} · 경고 {} · 오류 {} · 건너뜀 {}",
        count(Level::Pass),
        count(Level::Warn),
        count(Level::Fail),
        count(Level::Skip),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_node_versions() {
        assert_eq!(node_major("v22.14.0"), Some(22));
        assert_eq!(node_major("18.20.1"), Some(18));
        assert_eq!(node_major("unknown"), None);
    }

    #[test]
    fn collapses_and_limits_command_output() {
        assert_eq!(one_line("first\r\n second\tthird"), "first second third");
        assert_eq!(one_line(&"가".repeat(300)).chars().count(), 240);
    }
}
