# resume 히스토리에 롤아웃 셸 기록 병합

## 문제

`thread/resume`가 돌려주는 `thread.turns[].items`에는 셸 실행 항목이 없다. 실측
결과 돌아오는 타입은 `agentMessage`, `fileChange`, `userMessage`, `webSearch`,
`mcpToolCall`, `contextCompaction`, `subAgentActivity`뿐이다.

| 세션 | 롤아웃에 있는 셸 실행 | resume 응답의 셸 항목 |
| --- | --- | --- |
| `019f99d2` | `custom_tool_call(exec)` 13건 | 0건 |
| `019f943e` (80턴) | 다수 | 0건 (`fileChange` 258, `agentMessage` 229) |

렌더 경로는 라이브와 resume이 공용이므로(`block_lines_with_expansion` →
`is_bash_block` → `bash_lines`) 접힘 규칙 차이는 없다. 순수하게 항목이 유실된다.
그래서 resume 후에는 접힌 `▸ Bash …` 줄이 사라지고 원문 그대로 긴 `fileChange`
diff와 assistant 메시지만 남아, 트랜스크립트 전체가 펼쳐진 것처럼 보인다.

## 목표

resume한 트랜스크립트가 라이브 세션과 같은 모양이 되도록, 롤아웃 JSONL에서 셸
실행을 읽어 원래 위치에 끼워 넣는다.

## 범위 밖

추론 요약(`reasoning`)은 복원하지 않는다. 최근 롤아웃 6개에서 `reasoning.summary`가
모두 빈 배열이고 내용은 `encrypted_content`뿐이며 `agent_reasoning` 이벤트도 없다.
파서는 summary가 채워진 롤아웃을 만나면 블록을 만들지만, 현재 데이터로는 아무것도
복원되지 않는다.

`apply_patch` 실행의 stdout, `token_count` 등 나머지 롤아웃 이벤트도 다루지 않는다.
패치는 이미 `fileChange`가 담당한다.

## 병합 키 (실측)

- `turn.id` == 롤아웃 `payload.turn_id`
- `fileChange.id`(`exec-<uuid>`) == `patch_apply_end.call_id`
- 모든 롤아웃 이벤트에 `timestamp`(`2026-07-25T15:09:40.539Z`)가 있다. 전부 UTC
  동일 포맷이라 문자열 사전순 정렬이 시간순 정렬과 같다 — 날짜 크레이트가 필요 없다.
- app-server `turn`은 `startedAt`/`completedAt`를 유닉스 초로 갖는다.

## 설계

### 1. 새 모듈 `src/rollout.rs`

```rust
pub struct Rollout { events: Vec<RolloutEvent> }

struct RolloutEvent { turn_id: Option<String>, ts: String, kind: RolloutKind }

enum RolloutKind {
    Exec { command: String, output: String, exit_code: Option<i64>, duration_ms: Option<u64> },
    Reasoning { summary: String },
    PatchApplied { call_id: String },
    AssistantMessage { text: String },
}

pub fn load(codex_home: &Path, thread_id: &str) -> Option<Rollout>
```

- 파일 탐색: `<codex_home>/sessions` 아래를 걸어 파일명이 `rollout-`으로 시작하고
  `-<thread_id>.jsonl`로 끝나는 파일을 찾는다. `codex_home`은 기존
  `state.rs::codex_home()`과 동일한 규칙(`CODEX_HOME` → `USERPROFILE/.codex`)을 쓴다.
- 라인별로 파싱하고, JSON 오류나 모르는 타입은 그 라인만 버린다.
- 셸 커맨드 추출: `custom_tool_call.input`에서 `tools.shell_command(` 뒤의 균형 잡힌
  JSON 객체를 잘라 `serde_json`으로 읽고 `command`를 꺼낸다. 문자열과 배열 모두
  받는다(배열은 공백으로 잇는다). 입력에 `apply_patch`가 있으면 건너뛴다.
- 출력: `custom_tool_call_output`을 `call_id`로 짝지어 `output[].text`를 이어 붙이고,
  거기서 `Exit code: N`과 `Wall time: N seconds`를 읽는다.

### 2. 병합 (`state.rs::load_history`)

시그니처를 `load_history(&mut self, thread: &Value, rollout: Option<&Rollout>)`로
넓힌다. 턴마다:

1. 그 턴에 속한 롤아웃 이벤트를 고른다 — `turn_id`가 있으면 그것으로, 없으면
   `startedAt..completedAt` 시간창으로.
2. app-server 아이템에 타임스탬프를 붙인다.
   - `fileChange` → `PatchApplied.call_id == item.id`인 이벤트의 ts
   - `agentMessage` → 그 턴의 n번째 `AssistantMessage` ts
   - 그 밖의 아이템 → 직전에 확정된 ts를 물려받는다
3. `(ts, 원래 순서)`로 안정 정렬한 뒤 블록을 만든다. `Exec` 이벤트는 이 정렬로 제
   위치에 들어간다.

롤아웃이 `None`이면 지금과 똑같이 app-server 아이템만으로 블록을 만든다.

### 3. 블록 매핑

`Exec` → 라이브 경로와 같은 제목 포맷을 쓴다.

```
Bash · {compact_command(command, 88)} · exit {code} · {duration}
```

본문은 `collapse_output(strip_ansi(output), 400)`, 종료 코드가 0이 아니면
`BlockKind::Warning`, 아니면 `BlockKind::Tool`. 이 포맷이라야 기존
`is_bash_block`을 통과해 `▸` 접힘과 클릭 펼침이 그대로 동작한다.

### 4. 성능과 실패

파싱은 resume 스피너가 도는 동안 `spawn_blocking`에서 한다. 로컬 롤아웃은 평균
0.9 MB, 최대 14 MB이므로 수백 밀리초다.

파일이 없거나(다른 PC에서 만든 세션) 스키마가 바뀌면 `load`가 `None`을 돌려주고
resume은 지금과 동일하게 진행한다. 사용자에게 알리지 않는다.

## 테스트

`rollout.rs` 단위 테스트

- `tools.shell_command`의 `command`가 문자열일 때와 배열일 때 모두 추출한다
- `apply_patch` 입력은 `Exec`를 만들지 않는다
- `custom_tool_call_output`이 `call_id`로 짝지어져 출력과 종료 코드가 붙는다
- 종료 코드와 소요 시간을 출력 텍스트에서 읽는다
- 깨진 JSON 라인은 그 라인만 버리고 나머지를 계속 읽는다
- `summary`가 비어 있으면 `Reasoning` 블록을 만들지 않는다

`state.rs` 통합 테스트

- 합성 thread JSON + 합성 롤아웃 → 블록 순서가 assistant → bash → fileChange로
  나온다
- `fileChange`가 `patch_apply_end`로 정확히 정렬된다
- `rollout`이 `None`이면 기존 블록 구성이 그대로 유지된다
