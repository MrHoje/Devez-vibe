//! Best-effort cleanup for the legacy, explicitly configured Windows CU notifier.
//!
//! Codex 0.163.0 invokes legacy `notify` on the normal stop path, not on every
//! failed/interrupted turn. We reuse its argv/JSON shape only for the configured
//! native `turn-ended` command. The helper's parser is not a public contract:
//! successful command exit does not prove that an overlay or cursor was restored.
//! New plugin hooks and their trust configuration are deliberately untouched.

use std::path::Path;

#[cfg(windows)]
use anyhow::Context;
#[cfg(any(windows, test))]
use anyhow::{Result, ensure};
#[cfg(any(windows, test))]
use serde_json::json;

/// The caller must supply the Codex-home config, an owned terminal turn and its
/// captured cwd, and deduplicate calls. `true` means only that the notifier
/// process exited successfully; `false` means no supported notifier was run.
#[cfg(windows)]
pub async fn notify(
    config_path: &Path,
    thread_id: &str,
    turn_id: &str,
    cwd: &str,
    launch: impl FnOnce(&mut tokio::process::Command) -> std::io::Result<Option<tokio::process::Child>>,
) -> Result<bool> {
    let config = match std::fs::read_to_string(config_path) {
        Ok(config) => config,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).context("Could not read Codex computer-use notifier config");
        }
    };
    let Some(notification) = prepare(&config, thread_id, turn_id, cwd)? else {
        return Ok(false);
    };
    run(notification, launch).await
}

#[cfg(not(windows))]
pub async fn notify(
    _config_path: &Path,
    _thread_id: &str,
    _turn_id: &str,
    _cwd: &str,
    _launch: impl FnOnce(&mut tokio::process::Command) -> std::io::Result<Option<tokio::process::Child>>,
) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(any(windows, test))]
struct Notification {
    program: String,
    cwd: String,
    payload: String,
}

#[cfg(any(windows, test))]
fn prepare(
    config: &str,
    thread_id: &str,
    turn_id: &str,
    cwd: &str,
) -> Result<Option<Notification>> {
    // Parse TOML rather than recognizing text in comments, nested tables, or
    // unrelated commands. A nonmatching notify setting is not ours to execute.
    // TOML diagnostics can quote configuration lines containing credentials.
    // Report the category without placing any configuration text in logs.
    let config: toml::Value = toml::from_str(config)
        .map_err(|_| anyhow::anyhow!("Could not parse Codex computer-use notifier config"))?;
    let Some(argv) = config.get("notify").and_then(toml::Value::as_array) else {
        return Ok(None);
    };
    if argv.len() != 2 || argv[1].as_str() != Some("turn-ended") {
        return Ok(None);
    }
    let Some(program) = argv[0]
        .as_str()
        .filter(|program| is_native_notifier(program))
    else {
        return Ok(None);
    };
    for (name, value) in [("thread ID", thread_id), ("turn ID", turn_id), ("cwd", cwd)] {
        ensure!(
            !value.trim().is_empty() && !value.contains('\0'),
            "Computer-use notifier requires a nonempty {name} without NUL"
        );
    }
    Ok(Some(Notification {
        program: program.to_owned(),
        cwd: cwd.to_owned(),
        // Codex's legacy_notify.rs uses kebab-case fields. Do not forward user
        // messages or assistant output to a cleanup-only process.
        payload: json!({
            "type": "agent-turn-complete",
            "thread-id": thread_id,
            "turn-id": turn_id,
            "cwd": cwd,
            "client": "devez-vibe",
            "input-messages": [],
            "last-assistant-message": null,
        })
        .to_string(),
    }))
}

#[cfg(any(windows, test))]
fn is_native_notifier(program: &str) -> bool {
    // Keep this lexical check portable so Linux tests exercise the Windows
    // allowlist. Only local drive-rooted paths are supported, not PATH lookup,
    // drive-relative paths, UNC shares, device namespaces, or shell expansion.
    let path = program.replace('\\', "/");
    let bytes = path.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'/'
        || path.contains('\0')
    {
        return false;
    }
    let components: Vec<_> = path[3..].split('/').collect();
    if components.iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || part.ends_with([' ', '.'])
            || part.contains([':', '"', '<', '>', '|', '?', '*'])
            || part.chars().any(char::is_control)
    }) {
        return false;
    }
    components.last().is_some_and(|name| {
        name.eq_ignore_ascii_case("codex-computer-use.exe")
            || name.eq_ignore_ascii_case("codex-computer-use-arm64.exe")
    })
}

#[cfg(any(windows, test))]
const NON_INHERITABLE_ENV_VARS: &[&str] = &[
    "CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN",
    "NODE_REPL_AUTH_TOKEN",
    "CODEX_GUARDIAN_DECISIONS_API_KEY",
    "OPENAI_FEDERATION_RULE_ID",
    "OPENAI_IDENTITY_TOKEN_FILE",
    "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
];

#[cfg(any(windows, test))]
fn is_non_inheritable_env_var(name: &str) -> bool {
    NON_INHERITABLE_ENV_VARS
        .iter()
        .any(|restricted| name.eq_ignore_ascii_case(restricted))
}

#[cfg(any(windows, test))]
fn command(notification: Notification) -> tokio::process::Command {
    use std::process::Stdio;

    // Use structured argv, never cmd.exe/PowerShell or a shell command string.
    let mut command = std::process::Command::new(notification.program);
    command
        .arg("turn-ended")
        .arg(notification.payload)
        .current_dir(notification.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::child_process::isolate_launcher(&mut command);
    // Match Codex legacy notify's restricted launch-context scrubbing, while
    // retaining CODEX_HOME and normal Windows user/session environment.
    for name in NON_INHERITABLE_ENV_VARS {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(is_non_inheritable_env_var) {
            command.env_remove(name);
        }
    }
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    command
}

#[cfg(windows)]
async fn run(
    notification: Notification,
    launch: impl FnOnce(&mut tokio::process::Command) -> std::io::Result<Option<tokio::process::Child>>,
) -> Result<bool> {
    use std::time::Duration;
    use tokio::time::timeout;

    const EXIT_TIMEOUT: Duration = Duration::from_secs(2);
    const REAP_TIMEOUT: Duration = Duration::from_secs(1);

    let Some(mut child) = launch(&mut command(notification))
        .context("Could not start configured computer-use turn-ended notifier")?
    else {
        return Ok(false);
    };
    let failure = match timeout(EXIT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            ensure!(
                status.success(),
                "Computer-use turn-ended notifier failed: {status}"
            );
            return Ok(true);
        }
        Ok(Err(error)) => format!("Could not wait for computer-use turn-ended notifier: {error}"),
        Err(_) => "Computer-use turn-ended notifier timed out".to_owned(),
    };
    // Kill/reap only our short-lived notification child, never the Desktop's
    // shared helper or any process tree. Bound cleanup even after a wait error.
    let kill_error = child.start_kill().err();
    match timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => anyhow::bail!("{failure}"),
        Ok(Err(error)) => {
            anyhow::bail!("{failure}; could not reap notifier: {error}; kill error: {kill_error:?}")
        }
        Err(_) => anyhow::bail!(
            "{failure}; notifier did not exit after termination; kill error: {kill_error:?}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(program: &str) -> String {
        format!("notify = ['{program}', 'turn-ended']")
    }

    fn notification(config: &str) -> Option<Notification> {
        prepare(config, "thread-id", "turn-id", r"C:\work\한글 project").unwrap()
    }

    #[test]
    fn accepts_only_observed_native_executables_at_local_absolute_paths() {
        for path in [
            r"C:\Program Files\Codex\codex-computer-use.exe",
            r"D:\앱\codex-computer-use-arm64.exe",
            "c:/Codex/CODEX-COMPUTER-USE.EXE",
        ] {
            assert_eq!(notification(&config(path)).unwrap().program, path);
        }
    }

    #[test]
    fn rejects_shells_wrappers_relative_remote_and_ambiguous_paths() {
        for path in [
            "codex-computer-use.exe",
            r"C:codex-computer-use.exe",
            r"\Codex\codex-computer-use.exe",
            r"\\server\share\codex-computer-use.exe",
            r"\\?\C:\Codex\codex-computer-use.exe",
            r"C:\Codex\..\codex-computer-use.exe",
            r"C:\Codex\.\codex-computer-use.exe",
            r"C:\Codex \codex-computer-use.exe",
            r"C:\Codex:stream\codex-computer-use.exe",
            r"C:\Codex\\codex-computer-use.exe",
            r"C:\Codex\codex-computer-use.exe.",
            r"C:\Codex\codex-computer-use.exe.bat",
            r"C:\Codex\codex-computer-use.exe\",
            r"C:\Codex\codex-computer-use-swift.exe",
            r"C:\Windows\System32\cmd.exe",
            r"%LOCALAPPDATA%\codex-computer-use.exe",
        ] {
            assert!(notification(&config(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn accepts_toml_multiline_arrays_without_matching_comments_or_nested_keys() {
        let valid = r#"
            # notify = ["unrelated-command"]
            notify = [
                'C:\Codex\codex-computer-use.exe',
                "turn-ended", # native mode
            ]
            [unrelated]
            notify = ["something-else"]
        "#;
        assert!(notification(valid).is_some());
        assert!(
            notification("# notify = ['C:\\Codex\\codex-computer-use.exe', 'turn-ended']")
                .is_none()
        );
        assert!(
            notification(
                "[unrelated]\nnotify = ['C:\\Codex\\codex-computer-use.exe', 'turn-ended']"
            )
            .is_none()
        );
    }

    #[test]
    fn ignores_missing_nonstring_and_nonexact_notify_arrays() {
        for source in [
            "",
            "notify = 'C:\\Codex\\codex-computer-use.exe'",
            "notify = []",
            "notify = ['C:\\Codex\\codex-computer-use.exe']",
            "notify = [1, 'turn-ended']",
            "notify = ['C:\\Codex\\codex-computer-use.exe', 1]",
            "notify = ['C:\\Codex\\codex-computer-use.exe', 'turn-ended', '--extra']",
            "notify = ['C:\\Codex\\codex-computer-use.exe', 'TURN-ENDED']",
            "notify = ['C:\\Codex\\codex-computer-use.exe', 'turn-ended ']",
            "notify = ['C:\\Codex\\codex-computer-use.exe', 'serve']",
        ] {
            assert!(notification(source).is_none(), "{source}");
        }
    }

    #[test]
    fn malformed_toml_is_a_diagnostic_not_an_execution_candidate() {
        assert!(prepare("notify = [", "thread", "turn", r"C:\work").is_err());
        assert!(
            prepare(
                "notify = []\nnotify = ['C:\\Codex\\codex-computer-use.exe', 'turn-ended']",
                "thread",
                "turn",
                r"C:\work",
            )
            .is_err()
        );
    }

    #[test]
    fn payload_uses_exact_legacy_keys_and_has_no_conversation_content() {
        let prepared = prepare(
            &config(r"C:\Codex\codex-computer-use.exe"),
            "root-thread",
            "turn-42",
            r"C:\한글 project",
        )
        .unwrap()
        .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&prepared.payload).unwrap();
        assert_eq!(
            payload,
            json!({
                "type": "agent-turn-complete",
                "thread-id": "root-thread",
                "turn-id": "turn-42",
                "cwd": r"C:\한글 project",
                "client": "devez-vibe",
                "input-messages": [],
                "last-assistant-message": null,
            })
        );
    }

    #[test]
    fn refuses_empty_or_nul_identifiers_and_cwd() {
        let config = config(r"C:\Codex\codex-computer-use.exe");
        for (thread, turn, cwd) in [
            ("", "turn", r"C:\work"),
            ("thread", " \t", r"C:\work"),
            ("thread", "turn", ""),
            ("th\0read", "turn", r"C:\work"),
            ("thread", "turn", "C:\\wo\0rk"),
        ] {
            assert!(prepare(&config, thread, turn, cwd).is_err());
        }
    }

    #[test]
    fn builds_direct_argv_and_preserves_cwd_without_launching_helper() {
        let prepared =
            notification(&config(r"C:\Program Files\Codex\codex-computer-use.exe")).unwrap();
        let payload = prepared.payload.clone();
        let command = command(prepared);
        let command = command.as_std();
        assert_eq!(
            command.get_program(),
            r"C:\Program Files\Codex\codex-computer-use.exe"
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["turn-ended", payload.as_str()]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new(r"C:\work\한글 project"))
        );
        for name in NON_INHERITABLE_ENV_VARS {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == *name && value.is_none())
            );
        }
        assert!(!command.get_envs().any(|(key, _)| key == "CODEX_HOME"));
    }

    #[test]
    fn environment_filter_is_case_insensitive_without_dropping_codex_home() {
        assert!(is_non_inheritable_env_var("node_repl_auth_token"));
        assert!(is_non_inheritable_env_var("OpenAI_Identity_Token_File"));
        assert!(!is_non_inheritable_env_var("CODEX_HOME"));
        assert!(!is_non_inheritable_env_var("USERPROFILE"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn unsupported_platform_does_not_read_config_or_spawn() {
        assert!(
            !notify(
                Path::new("/nonexistent/config.toml"),
                "",
                "",
                "",
                |_| panic!("must not launch")
            )
            .await
            .unwrap()
        );
    }
}
