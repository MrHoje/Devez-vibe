//! Session rollout files hold what `thread/resume` leaves out — shell runs above
//! all. This module reads one rollout into timestamped events so the transcript
//! can be rebuilt with those runs back in place.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

/// One rollout entry worth showing, in file order.
pub struct RolloutEvent {
    /// RFC 3339, always UTC — sorts correctly as a plain string.
    pub ts: String,
    pub kind: RolloutKind,
}

pub enum RolloutKind {
    /// A shell run. `thread/resume` drops these entirely.
    Exec {
        command: String,
        output: String,
        exit_code: Option<i64>,
        duration_ms: Option<u64>,
    },
    /// A reasoning summary. Present-day rollouts leave `summary` empty and keep
    /// only `encrypted_content`, so this rarely fires.
    Reasoning { summary: String },
    /// Anchors an app-server `fileChange` item to a timestamp: its `call_id`
    /// equals the item's id.
    PatchApplied { call_id: String },
    /// Anchors an app-server `agentMessage` item to a timestamp.
    AssistantMessage { text: String },
}

pub struct Rollout {
    pub events: Vec<RolloutEvent>,
}

/// The rollout for `thread_id`, or `None` when there is none to read — a session
/// created on another machine, or a schema this parser no longer recognises.
pub fn load(codex_home: &Path, thread_id: &str) -> Option<Rollout> {
    let path = find_rollout(&codex_home.join("sessions"), thread_id)?;
    let text = fs::read_to_string(path).ok()?;
    Some(parse(&text))
}

/// Rollout file names end in the thread id, so the whole session tree is walked
/// looking for that suffix rather than guessing the date directory.
fn find_rollout(root: &Path, thread_id: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread_id}.jsonl");
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(suffix.as_str()) {
                return Some(path);
            }
        }
    }
    None
}

/// One JSONL line per entry. A line that fails to parse — or an entry type this
/// build does not know — is dropped on its own; the rest of the file still reads.
pub fn parse(text: &str) -> Rollout {
    let mut events = Vec::new();
    // Exec calls whose output has not arrived yet: `call_id` → index in `events`.
    let mut pending: Vec<(String, usize)> = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        match payload.get("type").and_then(Value::as_str).unwrap_or_default() {
            "custom_tool_call" => {
                let input = payload.get("input").and_then(Value::as_str).unwrap_or_default();
                let Some(command) = shell_command(input) else {
                    continue;
                };
                let call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                pending.push((call_id, events.len()));
                events.push(RolloutEvent {
                    ts,
                    kind: RolloutKind::Exec {
                        command,
                        output: String::new(),
                        exit_code: None,
                        duration_ms: None,
                    },
                });
            }
            "custom_tool_call_output" => {
                let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or_default();
                let Some(position) = pending.iter().position(|(id, _)| id == call_id) else {
                    continue;
                };
                let (_, index) = pending.remove(position);
                let text = output_text(payload.get("output"));
                if let RolloutKind::Exec {
                    output,
                    exit_code,
                    duration_ms,
                    ..
                } = &mut events[index].kind
                {
                    *exit_code = parse_exit_code(&text);
                    *duration_ms = parse_wall_time_ms(&text);
                    *output = text;
                }
            }
            "reasoning" => {
                let summary = summary_text(payload.get("summary"));
                if summary.is_empty() {
                    continue;
                }
                events.push(RolloutEvent {
                    ts,
                    kind: RolloutKind::Reasoning { summary },
                });
            }
            "patch_apply_end" => {
                let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                events.push(RolloutEvent {
                    ts,
                    kind: RolloutKind::PatchApplied {
                        call_id: call_id.to_owned(),
                    },
                });
            }
            // The `event_msg` form carries `message`; the `response_item` twin has
            // no such field and is skipped, so each message anchors exactly once.
            "agent_message" => {
                let Some(text) = payload.get("message").and_then(Value::as_str) else {
                    continue;
                };
                events.push(RolloutEvent {
                    ts,
                    kind: RolloutKind::AssistantMessage {
                        text: text.to_owned(),
                    },
                });
            }
            _ => {}
        }
    }
    Rollout { events }
}

fn summary_text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The shell command inside an `exec` call's script, or `None` when the script
/// is doing something else — applying a patch, above all.
fn shell_command(input: &str) -> Option<String> {
    let shell = input.find("tools.shell_command(")?;
    // A patch body can quote anything, including this module's own markers, so the
    // first tool call in the script decides what the call was for.
    if input
        .find("tools.apply_patch(")
        .is_some_and(|patch| patch < shell)
    {
        return None;
    }
    let arguments = balanced_object(&input[shell..])?;
    match serde_json::from_str::<Value>(arguments).ok()?.get("command")? {
        Value::String(command) => Some(command.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// The first `{…}` run in `text`, counting nesting and ignoring braces inside
/// strings. The call's arguments are hand-written JavaScript, so a plain
/// `find('}')` would stop at the first brace a command happens to contain.
fn balanced_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, character) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + character.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

fn output_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// The inner `Exit code: N` the code-mode wrapper prints for the command itself.
fn parse_exit_code(output: &str) -> Option<i64> {
    let index = output.find("Exit code:")?;
    let rest = output[index + "Exit code:".len()..].trim_start();
    let digits = rest
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '-')
        .collect::<String>();
    digits.parse().ok()
}

/// The wrapper's own `Wall time 1.6 seconds` line — the first of the two, which
/// covers the whole script rather than one command inside it.
fn parse_wall_time_ms(output: &str) -> Option<u64> {
    let index = output.find("Wall time")?;
    let rest = output[index + "Wall time".len()..]
        .trim_start()
        .trim_start_matches(':')
        .trim_start();
    let number = rest
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();
    let seconds = number.parse::<f64>().ok()?;
    Some((seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_and_patch_events_keep_their_file_order() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"event_msg","payload":{"type":"agent_message","message":"first"}}
{"timestamp":"2026-07-25T15:09:40.539Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc","success":true}}
{"timestamp":"2026-07-25T15:09:58.000Z","type":"event_msg","payload":{"type":"agent_message","message":"second"}}"#,
        );

        let summary = rollout
            .events
            .iter()
            .map(|event| match &event.kind {
                RolloutKind::AssistantMessage { text } => format!("assistant:{text}"),
                RolloutKind::PatchApplied { call_id } => format!("patch:{call_id}"),
                _ => "other".to_owned(),
            })
            .collect::<Vec<_>>();

        assert_eq!(summary, ["assistant:first", "patch:exec-abc", "assistant:second"]);
        assert_eq!(rollout.events[1].ts, "2026-07-25T15:09:40.539Z");
    }

    #[test]
    fn a_broken_line_costs_only_that_line() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"event_msg","payload":{"type":"agent_message","message":"kept"}}
{ this is not json
{"timestamp":"2026-07-25T15:08:34.000Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc"}}"#,
        );

        assert_eq!(rollout.events.len(), 2);
    }

    #[test]
    fn reasoning_without_a_summary_is_dropped() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:32.476Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"gAAA"}}
{"timestamp":"2026-07-25T15:08:33.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"둘러본다"}]}}"#,
        );

        assert_eq!(rollout.events.len(), 1);
        assert!(matches!(
            &rollout.events[0].kind,
            RolloutKind::Reasoning { summary } if summary == "둘러본다"
        ));
    }

    #[test]
    fn load_picks_the_file_whose_name_ends_with_the_thread_id() {
        let home = std::env::temp_dir().join("dvz-rollout-load-test");
        let day = home.join("sessions").join("2026").join("07").join("26");
        fs::create_dir_all(&day).expect("test directory");
        fs::write(
            day.join("rollout-2026-07-26T00-08-28-thread-wanted.jsonl"),
            "{\"timestamp\":\"2026-07-26T00:08:33.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"wanted\"}}\n",
        )
        .expect("wanted rollout");
        fs::write(
            day.join("rollout-2026-07-26T00-09-28-thread-other.jsonl"),
            "{\"timestamp\":\"2026-07-26T00:09:33.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"other\"}}\n",
        )
        .expect("other rollout");

        let rollout = load(&home, "thread-wanted").expect("rollout for the thread");
        let missing = load(&home, "thread-absent");
        fs::remove_dir_all(&home).ok();

        assert!(matches!(
            &rollout.events[0].kind,
            RolloutKind::AssistantMessage { text } if text == "wanted"
        ));
        assert!(missing.is_none());
    }

    /// Helper for the exec tests: the single `Exec` event a rollout produced.
    fn only_exec(rollout: &Rollout) -> (&str, &str, Option<i64>, Option<u64>) {
        rollout
            .events
            .iter()
            .find_map(|event| match &event.kind {
                RolloutKind::Exec {
                    command,
                    output,
                    exit_code,
                    duration_ms,
                } => Some((command.as_str(), output.as_str(), *exit_code, *duration_ms)),
                _ => None,
            })
            .expect("one exec event")
    }

    #[test]
    fn a_string_shell_command_becomes_an_exec_event() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"const r = await tools.shell_command({\"command\":\"cargo test --quiet\",\"workdir\":\"C:\\\\Source\"});\nconsole.log(r);"}}"#,
        );

        let (command, _, _, _) = only_exec(&rollout);
        assert_eq!(command, "cargo test --quiet");
    }

    #[test]
    fn an_array_shell_command_is_joined_with_spaces() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":[\"bash\",\"-lc\",\"ls\"]});"}}"#,
        );

        let (command, _, _, _) = only_exec(&rollout);
        assert_eq!(command, "bash -lc ls");
    }

    #[test]
    fn an_apply_patch_exec_call_makes_no_exec_event() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:09:40.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_patch","input":"const patch = \"*** Begin Patch\";\nawait tools.apply_patch({\"patch\":patch});"}}"#,
        );

        assert!(rollout.events.is_empty());
    }

    #[test]
    fn output_is_paired_by_call_id_with_its_exit_code_and_wall_time() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg TODO\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Script completed\nWall time 1.6 seconds\nOutput:\n"},{"type":"input_text","text":"Exit code: 3\nWall time: 0.5 seconds\nOutput:\nsrc/main.rs:12\n"}]}}"#,
        );

        let (command, output, exit_code, duration_ms) = only_exec(&rollout);
        assert_eq!(command, "rg TODO");
        assert!(output.contains("src/main.rs:12"));
        assert_eq!(exit_code, Some(3));
        assert_eq!(duration_ms, Some(1600));
    }

    #[test]
    fn an_exec_without_its_output_still_produces_an_event() {
        let rollout = parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg TODO\"});"}}"#,
        );

        let (_, output, exit_code, duration_ms) = only_exec(&rollout);
        assert!(output.is_empty());
        assert_eq!(exit_code, None);
        assert_eq!(duration_ms, None);
    }
}
