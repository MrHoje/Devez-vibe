# resume 롤아웃 셸 기록 병합 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `thread/resume`가 돌려주지 않는 셸 실행 기록을 세션 롤아웃 JSONL에서 읽어, resume한 트랜스크립트의 원래 위치에 접힌 `▸ Bash …` 블록으로 끼워 넣는다.

**Architecture:** 새 모듈 `src/rollout.rs`가 롤아웃 파일을 찾아 타임스탬프가 붙은 이벤트 목록으로 파싱한다. `state.rs::load_history`는 턴마다 app-server 아이템에 타임스탬프를 부여하고(파일 변경은 `patch_apply_end.call_id`로, assistant 메시지는 순서 매칭으로, 나머지는 직전 값 계승) 롤아웃의 셸 이벤트와 함께 안정 정렬한다. `main.rs`는 resume 스피너가 도는 동안 `spawn_blocking`으로 파싱한다.

**Tech Stack:** Rust edition 2024, `serde_json`, `chrono`(이미 의존성에 있음, `parse_from_rfc3339`만 사용), `tokio::task::spawn_blocking`, 테스트는 각 모듈의 `#[cfg(test)] mod tests`.

**Spec:** `docs/superpowers/specs/2026-07-26-resume-rollout-merge-design.md`

## Global Constraints

- 셸 블록 제목은 라이브 경로와 **정확히 같은 포맷**이어야 한다: `Bash · {compact_command(command, 88)}{ · exit N}{ · 1.6s}`. 이 포맷이 아니면 `renderer.rs:2152`의 `is_bash_block`을 통과하지 못해 접힘·클릭 펼침이 깨진다.
- 본문은 `collapse_output(&strip_ansi(output), 400)`을 통과시킨다 (`state.rs:5934`, `state.rs:5969`).
- 종료 코드가 `0` 또는 알 수 없음이면 `BlockKind::Tool`, 그 밖이면 `BlockKind::Warning`.
- 롤아웃이 없거나 파싱에 실패하면 `None`으로 조용히 폴백한다. 사용자에게 알리는 블록을 만들지 않는다.
- 롤아웃 타임스탬프는 `2026-07-25T15:09:40.539Z` 형태로 전부 UTC 동일 포맷이므로, 정렬은 문자열 사전순으로 한다. 턴 시간창 비교에만 `chrono::DateTime::parse_from_rfc3339`로 유닉스 초를 얻는다.
- `reasoning.summary`가 비어 있으면 블록을 만들지 않는다 (현재 롤아웃은 전부 비어 있다).
- 새 의존성을 추가하지 않는다.

## File Structure

- **Create** `src/rollout.rs` — 롤아웃 파일 탐색, JSONL 파싱, 셸 커맨드/출력 추출. 렌더러와 상태를 모르며 순수 데이터만 돌려준다.
- **Modify** `src/state.rs` — `load_history`가 롤아웃을 받아 병합 정렬한다. 블록 생성 헬퍼 추가.
- **Modify** `src/main.rs` — `mod rollout;` 선언, `spawn_blocking` 로더, 두 호출부(`main.rs:261`, `main.rs:1768`) 갱신.

---

### Task 1: 롤아웃 파일 탐색과 이벤트 파싱 골격

**Files:**
- Create: `src/rollout.rs`
- Modify: `src/main.rs:1-11` (모듈 선언 추가)

**Interfaces:**
- Consumes: 없음
- Produces:
  - `pub struct Rollout { pub events: Vec<RolloutEvent> }`
  - `pub struct RolloutEvent { pub ts: String, pub kind: RolloutKind }`
  - `pub enum RolloutKind { Exec { command: String, output: String, exit_code: Option<i64>, duration_ms: Option<u64> }, Reasoning { summary: String }, PatchApplied { call_id: String }, AssistantMessage { text: String } }`
  - `pub fn parse(text: &str) -> Rollout`
  - `pub fn load(codex_home: &Path, thread_id: &str) -> Option<Rollout>`

`Exec` 변형의 내용물은 Task 2에서 채운다. Task 1에서는 타입만 선언하고 `parse`는 `Reasoning`/`PatchApplied`/`AssistantMessage`만 만든다.

- [ ] **Step 1: 실패하는 테스트 작성**

`src/rollout.rs`를 만들고 아래를 그대로 넣는다 (구현은 Step 3에서 채운다).

```rust
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
}
```

`src/main.rs`의 모듈 선언에 알파벳 순서를 지켜 한 줄을 넣는다 (`mod renderer;` 다음).

```rust
mod rollout;
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test rollout:: 2>&1 | tail -20`
Expected: 컴파일 실패 — `cannot find function 'parse' in this scope`, `cannot find function 'load' in this scope`

- [ ] **Step 3: 최소 구현**

`src/rollout.rs`의 `pub struct Rollout { … }` 정의 바로 뒤, `#[cfg(test)] mod tests` 앞에 넣는다.

```rust
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
```

- [ ] **Step 4: 통과 확인**

Run: `cargo test rollout:: 2>&1 | tail -20`
Expected: `test result: ok. 4 passed`

- [ ] **Step 5: 커밋**

```bash
git add src/rollout.rs src/main.rs
git commit -m "feat: read session rollout events"
```

---

### Task 2: 셸 실행 커맨드와 출력 추출

**Files:**
- Modify: `src/rollout.rs` (`parse`에 `custom_tool_call` / `custom_tool_call_output` 분기 추가, 헬퍼 추가, 테스트 추가)

**Interfaces:**
- Consumes: Task 1의 `parse`, `RolloutEvent`, `RolloutKind::Exec`
- Produces: `parse`가 `RolloutKind::Exec { command, output, exit_code, duration_ms }`를 채워서 돌려준다. 다른 모듈에 새 공개 함수는 없다.

실측한 롤아웃 모양은 다음과 같다. 코드 모드는 셸을 `exec`라는 커스텀 툴 안의 자바스크립트로 실행한다.

```
payload.type = "custom_tool_call", name = "exec", call_id = "call_Zxzo…",
input = "const r = await tools.shell_command({\"command\":\"cargo test\",\"workdir\":\"C:\\\\Source\"}); …"

payload.type = "custom_tool_call_output", call_id = "call_Zxzo…",
output = [{"type":"input_text","text":"Script completed\nWall time 1.6 seconds\nOutput:\n"},
          {"type":"input_text","text":"Exit code: 0\nWall time: 0.5 seconds\nOutput:\n…"}]
```

- [ ] **Step 1: 실패하는 테스트 작성**

`src/rollout.rs`의 `mod tests` 안, 마지막 테스트 뒤에 넣는다.

```rust
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
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test rollout:: 2>&1 | tail -25`
Expected: 5개 새 테스트가 `one exec event` panic으로 FAIL (`an_apply_patch_exec_call_makes_no_exec_event`는 통과할 수 있다)

- [ ] **Step 3: 최소 구현**

`parse`의 `match` 안, `"reasoning"` 분기 앞에 두 분기를 넣는다. 짝을 기다리는 호출을 추적해야 하므로 루프 앞에 `pending`도 함께 선언한다.

```rust
    let mut events = Vec::new();
    // Exec calls whose output has not arrived yet: `call_id` → index in `events`.
    let mut pending: Vec<(String, usize)> = Vec::new();
```

```rust
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
```

`summary_text` 뒤에 헬퍼를 넣는다.

```rust
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
```

- [ ] **Step 4: 통과 확인**

Run: `cargo test rollout:: 2>&1 | tail -20`
Expected: `test result: ok. 9 passed`

- [ ] **Step 5: 경고 확인**

Run: `cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | head`
Expected: `load` / `Rollout` 계열의 `dead_code` 경고만 남는다 — Task 4에서 배선되면 사라진다. 그 밖의 새 경고는 없어야 한다.

- [ ] **Step 6: 커밋**

```bash
git add src/rollout.rs
git commit -m "feat: lift shell runs out of rollout exec calls"
```

---

### Task 3: 히스토리 병합 정렬

**Files:**
- Modify: `src/state.rs:2713-2731` (`load_history`), 그리고 `completed_item_block` 근처(`src/state.rs:5587` 뒤)에 헬퍼 추가
- Modify: `src/state.rs` 상단 `use` 블록 (`crate::rollout::{Rollout, RolloutEvent, RolloutKind}` 추가)
- Test: `src/state.rs`의 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 1·2의 `crate::rollout::{parse, Rollout, RolloutEvent, RolloutKind}`
- Produces: `pub fn load_history(&mut self, thread: &Value, rollout: Option<&Rollout>)` — Task 4가 이 시그니처로 호출한다.

병합 규칙:

1. 턴의 시간창 `startedAt..=completedAt`(유닉스 초)으로 롤아웃 이벤트를 고른다. 실측상 `startedAt`은 롤아웃의 `task_started` 타임스탬프와 정확히 같다.
2. app-server 아이템에 타임스탬프를 붙인다. `fileChange`는 `PatchApplied.call_id == item.id`로, `agentMessage`는 그 턴의 다음 `AssistantMessage`를 텍스트로 매칭해서, 나머지는 직전에 확정된 값을 물려받는다.
3. `(타임스탬프, 원래 순서)`로 안정 정렬한다. 셸 이벤트는 자기 타임스탬프로 제자리에 들어간다.

- [ ] **Step 1: 실패하는 테스트 작성**

`src/state.rs`의 `mod tests` 안, `command_block_identity_survives_active_to_completed_transition` 테스트 뒤에 넣는다.

```rust
    /// A turn covering 15:08:28–15:12:59 UTC on 2026-07-25, matching the
    /// timestamps the rollout literals below use.
    fn history_thread() -> Value {
        json!({
            "turns": [{
                "id": "turn-1",
                "startedAt": 1_784_992_108_i64,
                "completedAt": 1_784_992_379_i64,
                "items": [
                    { "type": "agentMessage", "id": "item-1", "text": "확인해봤습니다" },
                    { "type": "fileChange", "id": "exec-abc", "changes": [] },
                    { "type": "agentMessage", "id": "item-2", "text": "고쳤습니다" }
                ]
            }]
        })
    }

    #[test]
    fn resumed_shell_runs_land_between_the_messages_they_ran_under() {
        let mut state = test_state();
        // 15:08:33 message, 15:08:36 shell run, 15:09:40 patch, 15:09:58 message.
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"event_msg","payload":{"type":"agent_message","message":"확인해봤습니다"}}
{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"cargo test\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Script completed\nWall time 1.6 seconds\nOutput:\n"},{"type":"input_text","text":"Exit code: 0\nWall time: 0.5 seconds\nOutput:\nok\n"}]}}
{"timestamp":"2026-07-25T15:09:40.539Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc"}}
{"timestamp":"2026-07-25T15:09:58.000Z","type":"event_msg","payload":{"type":"agent_message","message":"고쳤습니다"}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        let titles = state
            .committed
            .iter()
            .map(|block| block.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles[0], "Codex");
        assert_eq!(titles[1], "Bash · cargo test · exit 0 · 1.6s");
        assert_eq!(titles[3], "Codex");
        assert!(matches!(state.committed[1].kind, BlockKind::Tool));
        assert_eq!(state.committed[1].body, "ok");
        // The file change sorts by its `patch_apply_end` time: after the shell run
        // at 15:08:36, before the message at 15:09:58.
        assert!(matches!(state.committed[2].kind, BlockKind::FileChange));
    }

    #[test]
    fn a_failed_shell_run_is_resumed_as_a_warning() {
        let mut state = test_state();
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"cargo test\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Wall time 2.0 seconds\n"},{"type":"input_text","text":"Exit code: 101\nOutput:\nfailed\n"}]}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        let bash = state
            .committed
            .iter()
            .find(|block| block.title.starts_with("Bash ·"))
            .expect("bash block");
        assert_eq!(bash.title, "Bash · cargo test · exit 101 · 2.0s");
        assert!(matches!(bash.kind, BlockKind::Warning));
    }

    #[test]
    fn history_without_a_rollout_keeps_the_server_item_order() {
        let mut state = test_state();

        state.load_history(&history_thread(), None);

        let titles = state
            .committed
            .iter()
            .map(|block| block.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles.len(), 3);
        assert_eq!(titles[0], "Codex");
        assert_eq!(titles[2], "Codex");
    }

    #[test]
    fn rollout_events_outside_the_turn_window_are_left_out() {
        let mut state = test_state();
        // 15:20:00 is past this turn's 15:12:59 end.
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:20:00.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_late","input":"await tools.shell_command({\"command\":\"git status\"});"}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        assert!(
            !state
                .committed
                .iter()
                .any(|block| block.title.starts_with("Bash ·"))
        );
    }
```

- [ ] **Step 2: 실패 확인**

Run: `cargo test resumed_ history_without 2>&1 | tail -20`
Expected: 컴파일 실패 — `load_history` takes 1 argument but 2 were supplied

- [ ] **Step 3: 최소 구현**

`src/state.rs`의 `use` 블록에 한 줄을 넣는다 (`use crate::renderer::…` 근처, 알파벳 순서).

```rust
use crate::rollout::{Rollout, RolloutEvent, RolloutKind};
```

`load_history`(`src/state.rs:2713`)를 아래로 바꾼다.

```rust
    /// Rebuilds the transcript from a resumed thread. `rollout` fills in what
    /// `thread/resume` omits — shell runs above all — placing each one back where
    /// it ran rather than at the end of its turn.
    pub fn load_history(&mut self, thread: &Value, rollout: Option<&Rollout>) {
        let Some(turns) = thread.get("turns").and_then(Value::as_array) else {
            return;
        };
        for turn in turns {
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };
            for block in merged_turn_blocks(&self.cwd, turn, items, rollout) {
                if matches!(block.kind, BlockKind::Assistant) {
                    self.last_assistant_markdown = Some(block.body.clone());
                }
                self.committed.push(block);
            }
        }
        self.show_welcome = false;
    }
```

`completed_item_block` 함수 뒤(`permission_detail` 앞)에 헬퍼를 넣는다.

```rust
/// One turn's blocks, server items and rollout events interleaved by time.
fn merged_turn_blocks(
    cwd: &str,
    turn: &Value,
    items: &[Value],
    rollout: Option<&Rollout>,
) -> Vec<Block> {
    let events = rollout
        .map(|rollout| turn_events(turn, rollout))
        .unwrap_or_default();
    let mut rows: Vec<(String, usize, Block)> = Vec::new();
    let mut order = 0usize;
    // Items the rollout cannot date — user messages, MCP calls — inherit the last
    // known time so they keep their place relative to what surrounds them.
    let mut last_ts = String::new();
    let mut assistant_cursor = 0usize;
    for item in items {
        if let Some(ts) = item_timestamp(item, &events, &mut assistant_cursor) {
            last_ts = ts;
        }
        if let Some(block) = completed_item_block(cwd, item) {
            rows.push((last_ts.clone(), order, block));
            order += 1;
        }
    }
    for event in &events {
        if let Some(block) = event_block(event) {
            rows.push((event.ts.clone(), order, block));
            order += 1;
        }
    }
    // The timestamps are a fixed-width UTC format, so string order is time order.
    rows.sort_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)));
    rows.into_iter().map(|(_, _, block)| block).collect()
}

/// The rollout events belonging to one turn. Only `patch_apply_end` carries a
/// `turn_id`, so the turn's own window is what scopes the rest.
fn turn_events<'a>(turn: &Value, rollout: &'a Rollout) -> Vec<&'a RolloutEvent> {
    let started = turn.get("startedAt").and_then(Value::as_i64);
    let completed = turn.get("completedAt").and_then(Value::as_i64);
    let (Some(started), Some(completed)) = (started, completed) else {
        return rollout.events.iter().collect();
    };
    rollout
        .events
        .iter()
        .filter(|event| {
            unix_seconds(&event.ts)
                .is_some_and(|seconds| seconds >= started && seconds <= completed)
        })
        .collect()
}

fn unix_seconds(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|moment| moment.timestamp())
}

/// When the rollout can date a server item. `cursor` walks the turn's assistant
/// messages so a repeated message text still anchors to its own occurrence.
fn item_timestamp(
    item: &Value,
    events: &[&RolloutEvent],
    cursor: &mut usize,
) -> Option<String> {
    match item.get("type").and_then(Value::as_str)? {
        "fileChange" => {
            let id = item.get("id").and_then(Value::as_str)?;
            events
                .iter()
                .find(|event| {
                    matches!(&event.kind, RolloutKind::PatchApplied { call_id } if call_id == id)
                })
                .map(|event| event.ts.clone())
        }
        "agentMessage" => {
            let text = item.get("text").and_then(Value::as_str)?;
            let offset = events.iter().skip(*cursor).position(|event| {
                matches!(&event.kind, RolloutKind::AssistantMessage { text: message } if message == text)
            })?;
            let index = *cursor + offset;
            *cursor = index + 1;
            Some(events[index].ts.clone())
        }
        _ => None,
    }
}

/// The block a rollout-only event becomes. Anchors produce nothing: the server
/// item they date is already in the transcript.
fn event_block(event: &RolloutEvent) -> Option<Block> {
    match &event.kind {
        RolloutKind::Exec {
            command,
            output,
            exit_code,
            duration_ms,
        } => {
            let suffix = exit_code
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            let duration = duration_ms
                .map(|duration| format!(" · {}", format_duration(duration)))
                .unwrap_or_default();
            Some(Block::new(
                if exit_code.unwrap_or(0) == 0 {
                    BlockKind::Tool
                } else {
                    BlockKind::Warning
                },
                format!(
                    "Bash · {}{suffix}{duration}",
                    compact_command(command, 88)
                ),
                collapse_output(&strip_ansi(output), 400),
            ))
        }
        RolloutKind::Reasoning { summary } => {
            Some(Block::new(BlockKind::Reasoning, "Thinking…", summary))
        }
        RolloutKind::PatchApplied { .. } | RolloutKind::AssistantMessage { .. } => None,
    }
}
```

- [ ] **Step 4: 통과 확인**

Run: `cargo test resumed_ history_without rollout_events_outside 2>&1 | tail -20`
Expected: `test result: ok. 4 passed`

- [ ] **Step 5: 전체 테스트**

Run: `cargo test 2>&1 | tail -15`
Expected: 기존 테스트 전부 통과 (`main.rs`가 아직 인수를 안 넘겨 컴파일 오류가 나면 Task 4로 넘어가기 전 `state.load_history(thread, None)`로 임시 통과시키지 말고, Task 4를 이어서 진행한다)

- [ ] **Step 6: 커밋**

```bash
git add src/state.rs
git commit -m "feat: merge rollout shell runs into resumed history"
```

---

### Task 4: resume 경로 배선

**Files:**
- Modify: `src/main.rs:261` (시작 시 `--resume`), `src/main.rs:1768` (`/resume`와 사이드 대화 복귀)
- Modify: `src/main.rs` (`load_rollout` 헬퍼 추가, `use rollout::Rollout;`)
- Modify: `src/state.rs:6161` (`codex_home`을 `pub(crate)`로)

**Interfaces:**
- Consumes: `state::codex_home()`, `rollout::load`, `AppState::load_history(thread, Option<&Rollout>)`
- Produces: 없음 (배선이 마지막 단계다)

- [ ] **Step 1: `codex_home` 공개**

`src/state.rs:6161`을 바꾼다.

```rust
pub(crate) fn codex_home() -> Option<PathBuf> {
```

- [ ] **Step 2: 로더 추가**

`src/main.rs`의 `open_resume_picker`(`src/main.rs:433`) 바로 앞에 넣는다. `use rollout::Rollout;`도 `use renderer::…` 다음 줄에 추가한다.

```rust
/// Reads the session rollout off the event loop: the file runs to 14 MB, and the
/// resume spinner has to keep repainting while it is parsed.
async fn load_rollout(thread_id: &str) -> Option<Rollout> {
    let thread_id = thread_id.to_owned();
    tokio::task::spawn_blocking(move || rollout::load(&state::codex_home()?, &thread_id))
        .await
        .ok()
        .flatten()
}
```

- [ ] **Step 3: 두 호출부 갱신**

`src/main.rs:260-262`를 바꾼다. `thread_id`는 `attach_thread`로 넘어갔으므로 상태에 붙은 값을 읽는다.

```rust
    if is_resuming {
        let rollout = load_rollout(&state.thread_id).await;
        state.load_history(thread, rollout.as_ref());
    }
```

`src/main.rs:1768`을 바꾼다.

```rust
    let rollout = load_rollout(&state.thread_id).await;
    state.load_history(&resumed.thread, rollout.as_ref());
```

- [ ] **Step 4: 빌드와 전체 테스트**

Run: `cargo test 2>&1 | tail -15`
Expected: 전부 통과

Run: `cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | head`
Expected: 새 경고 없음

- [ ] **Step 5: 실제 세션으로 확인**

Run: `cargo run -- --resume 019f99d2-1051-7ba1-bd30-5973f459f6f9`
Expected: 트랜스크립트에 `▸ Bash · … · exit 0 · 1.6s` 줄들이 assistant 메시지와 파일 변경 사이에 섞여 나온다. 헤딩을 클릭하면 `▾`로 바뀌며 출력이 펼쳐진다. 확인 후 `/quit`.

이 세션의 롤아웃에는 `exec` 호출이 13건 있고 그중 3건은 `apply_patch`이므로, `Bash` 줄은 10건 안팎이어야 한다.

- [ ] **Step 6: 커밋**

```bash
git add src/main.rs src/state.rs
git commit -m "feat: load the rollout when resuming a session"
```
