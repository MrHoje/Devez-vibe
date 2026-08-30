# DevezVibe 4종 에이전트 시스템 상세 구현 계획서

> 문서 상태: **2차 재검수 완료본**  
> 대상 기능: `Standard`, `Planner`, `Advisor`, `Finisher`  
> 기준 저장소: `MrHoje/Devez-vibe` `main`  
> 참고 구현: `MrHoje/devez-marketplace/plugins/hoje-code`  
> 명시적 제외: Automatic 라우팅, 별도 Research 에이전트, Insane Search, Hoje 런타임·상태 파일  
> 이 문서는 구현 계획만 정의하며 제품 소스 코드는 수정하지 않는다.

---

## 0. 요약

DevezVibe에 다음 네 역할을 provider 공통 기능으로 내장한다.

```text
Standard → Planner → Advisor → Finisher → Standard
```

| 역할 | 핵심 책임 | 제품 파일 수정 |
|---|---|---:|
| `Standard` | 현재 Claude/Codex/OpenCode 기본 하네스를 그대로 사용 | 가능 |
| `Planner` | 요구 명확화, 저장소 조사, 설계, 계획 자체 검토 | 금지 지침 |
| `Advisor` | 접근법 평가, 추천, 반론, 대안·트레이드오프 제시 | 금지 지침 |
| `Finisher` | 구현, 검증, 독립 리뷰·QA, blocker 해결, 완료 증거 | 가능 |

구현의 핵심은 “네 개의 별도 런타임”이 아니라 다음 두 요소다.

1. `AppState`가 현재 선택된 역할을 소유한다.
2. 다음 턴을 보낼 때 선택된 역할의 내장 prompt를 provider 공통 `additionalContext` 경로로 전달한다.

```text
AppState.agent_mode
      ↓
DevezVibe agent context
      ↓
src/backend.rs
  ├─ Claude Agent SDK
  ├─ Codex app-server
  └─ OpenCode ACP
```

`hoje-code`는 설치하거나 실행하지 않는다. Planner·Architect·Critic·Executor·Executor-QA 및 Ask·Plan·Goals 워크플로우에서 검증된 역할 원칙만 추출해 DevezVibe 전용 prompt로 재작성한다.

재검수 후 확정한 중요한 보완 사항은 다음과 같다.

- busy 상태의 `Tab`은 기존 prompt queue 동작을 유지한다.
- `/btw` split 상태의 `Tab`은 기존 pane 전환을 유지한다.
- background subagent가 살아 있거나 queued prompt가 있으면 역할 변경을 잠근다.
- 이전 역할 지침은 대화 컨텍스트에 남으므로 `Standard` 복귀 시 한 번의 **reset instruction**을 보낸다.
- resume한 세션에는 과거 specialized agent 지침이 남아 있을 수 있으므로 첫 Standard 턴에 reset을 보낸다.
- Planner/Advisor의 수정 금지는 첫 버전에서 prompt-level 계약이며 provider 공통 보안 sandbox가 아니다.
- picker는 후보를 이동하는 동안 실제 역할을 바꾸지 않고 Enter에서만 확정한다.

---

# 1. 문서 목적

이 문서는 DevezVibe에 사용자가 즉시 전환할 수 있는 네 가지 에이전트 모드를 구현하기 위한 상세 설계와 단계별 작업 계획을 제공한다.

목표는 다음과 같다.

1. 현재 Claude Agent SDK, Codex app-server, OpenCode ACP의 기본 하네스를 훼손하지 않는다.
2. 별도 marketplace, plugin, skill 설치 없이 역할 지침을 DevezVibe 바이너리에 내장한다.
3. 모델, provider, effort, Fast, Vibe, Response 설정과 에이전트 역할을 분리한다.
4. `Standard`를 기본값으로 유지해 기존 사용자의 동작을 최대한 보존한다.
5. `Planner`에서 Hoje Ask와 Hoje Plan의 핵심을 통합한다.
6. `Advisor`에서 Hoje Architect와 Critic의 근거 기반 판단 철학을 확장한다.
7. `Finisher`에서 Hoje Goals의 목표 완결·검증 철학을 가볍게 포팅한다.
8. Claude, Codex, OpenCode에서 역할명, 전환 UX, 화면 표시, 핵심 행동을 일관되게 만든다.
9. 구현 전 테스트·회귀 범위와 완료 조건을 명확하게 고정한다.

이 계획의 최우선 원칙은 다음과 같다.

> 현재 기본 하네스가 이미 잘하는 일반 개발 동작은 다시 구현하지 않고, 역할을 바꿀 가치가 있는 행동 차이만 추가한다.

---

# 2. 확정된 제품 결정

## 2.1 역할 구성

```text
Standard
Planner
Advisor
Finisher
```

다른 역할은 첫 버전에 추가하지 않는다.

## 2.2 기본값

새 프로세스는 항상 `Standard`로 시작한다.

## 2.3 전환 방식

사용자가 수동으로 선택한다.

- idle 상태의 bare `Tab`
- `/agent`
- `/agent <mode>`
- 컴포저의 agent badge 클릭

## 2.4 자동 전환 없음

다음 동작은 하지 않는다.

- 요청을 분류해 자동으로 역할 선택
- Planner 종료 후 Finisher 자동 실행
- Advisor 검토 후 Planner 자동 실행
- Finisher가 자동으로 Planner 모드로 전환
- 역할에 따라 모델/provider 자동 교체

## 2.5 역할 상태 소유자

`AppState`가 역할을 소유한다. Claude, Codex, OpenCode의 native agent mode는 역할 상태의 source of truth가 아니다.

## 2.6 역할 지속 범위

역할은 하나의 `AppState` 수명 동안 유지한다.

유지되는 경우:

- 모델 변경
- effort 변경
- provider 변경
- Vibe/Response/Shell/Diff 변경
- 같은 UI thread에 backing session을 붙이는 provider handoff

초기화되는 경우:

- 새 DevezVibe 프로세스
- 새 `AppState` 생성

파일에는 저장하지 않는다.

## 2.7 Research 기능

첫 버전에서 제외한다.

기존 provider의 일반 검색 능력은 계속 사용할 수 있지만, 별도 Research agent, Insane Search, 직접 fetch fallback은 구현하지 않는다.

---

# 3. 범위와 비범위

## 3.1 이번 구현 범위

- `AgentMode` 공통 타입
- `Standard`, `Planner`, `Advisor`, `Finisher`
- compile-time 내장 prompt
- `AppState` 역할 상태
- idle Tab 순환
- `/agent` picker
- `/agent <mode>` 직접 선택
- clickable agent badge
- role context의 provider 공통 전달
- specialized role에서 Standard로 돌아올 때 reset instruction
- resume 첫 Standard 턴의 stale-role reset
- 역할 변경 잠금 조건
- 역할별 prompt 정적 검사
- provider별 transport 회귀 테스트
- UI 폭·클릭 위치·키 충돌 테스트
- README, npm README, 도움말 업데이트

## 3.2 명시적으로 제외하는 기능

- `Automatic`
- 요청 분류용 별도 LLM 호출
- agent pipeline
- 별도 `Research` primary agent
- Insane Search
- 직접 web fetch 엔진
- Hoje marketplace/plugin 설치
- Hoje CLI 실행
- `.hoje` 디렉터리 생성
- `ralplan`, `ultragoal`
- receipt, ledger, checkpoint
- `goals.json`, `ledger.jsonl`, `stage_n`, `sha256`
- agent별 모델 지정
- agent별 provider 지정
- OpenCode `session/set_mode` 연동
- agent별 별도 비용·context 정책
- provider 공통 hard read-only sandbox
- 사용자 정의 custom agent 파일
- agent 선택 영구 저장

## 3.3 구현하지 않는 이유

### Automatic

라우팅 오판, 다중 상태, UI 표기, pipeline, 비용·지연이 추가된다. 수동 네 역할의 실제 사용성이 검증된 뒤 별도 기능으로 추가하는 것이 안전하다.

### Hoje runtime

Hoje의 durability는 강력하지만 DevezVibe 내장 역할의 첫 목표에는 과하다. `.hoje`, receipt, ledger를 가져오면 역할 전환 기능이 별도 workflow engine 개발로 커진다.

### Hard read-only

Planner/Advisor를 Claude, Codex, OpenCode 모두에서 완전히 동일하게 강제하려면 provider별 tool interception과 Bash mutation 판별이 필요하다. 첫 버전에서는 명확한 역할 prompt로 제한하고, 이를 보안 경계라고 표현하지 않는다.

---

# 4. Hoje-Code 참고 범위

## 4.1 참고 파일

`MrHoje/devez-marketplace`의 다음 파일을 기준으로 한다.

```text
plugins/hoje-code/agents/planner.md
plugins/hoje-code/agents/architect.md
plugins/hoje-code/agents/critic.md
plugins/hoje-code/agents/executor.md
plugins/hoje-code/agents/executor-qa.md
plugins/hoje-code/skills/hoje-ask/SKILL.md
plugins/hoje-code/skills/hoje-plan/SKILL.md
plugins/hoje-code/skills/hoje-goals/SKILL.md
```

## 4.2 역할별 매핑

| DevezVibe | Hoje-Code 참고 요소 |
|---|---|
| `Standard` | 참고하지 않음. 현재 provider 기본 하네스 유지 |
| `Planner` | Hoje Ask + Hoje Planner + Hoje Plan + Architect/Critic 자체 검토 |
| `Advisor` | Hoje Architect + Hoje Critic, 기술 자문용으로 재구성 |
| `Finisher` | Hoje Goals + Executor + Architect + Executor-QA |

## 4.3 가져올 Planner 원칙

- 저장소를 먼저 조사한다.
- 사실과 가정을 분리한다.
- 영향 경로, 계약, 위험, 검증 명령이 있는 bounded plan을 만든다.
- 저장소에서 확인할 수 있는 사실을 사용자에게 묻지 않는다.
- material한 모호성만 질문한다.
- 계획 중 제품 파일을 수정하지 않는다.
- 실행했다고 주장하려면 실제 실행 기록이 있어야 한다.

## 4.4 가져올 Architect 원칙

- 요청된 제품 계약과 실제 저장소 상태를 함께 검토한다.
- 아키텍처, 동작, 호환성, 보안 경계, 검증 증거를 본다.
- 근거 없이 승인하지 않는다.
- actionable blocker를 구체적인 위치·이유와 함께 제시한다.

## 4.5 가져올 Critic 원칙

- 누락된 surface를 찾는다.
- 단계 순서 오류를 찾는다.
- 숨은 의존성을 찾는다.
- 약한 acceptance criteria를 찾는다.
- 테스트가 통과하지만 실제 동작이 깨질 수 있는 경우를 찾는다.
- 문제가 있으면 정확한 보완 방법을 제시한다.

## 4.6 가져올 Executor 원칙

- 한 번에 bounded한 목표를 처리한다.
- 관련 없는 사용자 변경을 보존한다.
- 가장 단순하고 호환 가능한 구현을 사용한다.
- targeted verification을 실행한다.
- 변경 경로, 실행 명령, 결과, 남은 위험을 보고한다.

## 4.7 가져올 Executor-QA 원칙

- 구현자와 독립된 관점으로 확인한다.
- 실제 사용자-facing surface를 검증한다.
- regression과 adversarial case를 본다.
- artifact 존재를 확인한다.
- blocker를 일반 조언으로 약화하지 않는다.

## 4.8 가져오지 않을 요소

- Hoje 이름·브랜드를 사용자-facing role에 노출
- `.hoje` state contract
- CLI command syntax
- artifact writer
- receipt-only response
- persisted subagent identity
- review pass 번호
- conflict disposition schema
- 최대 5회 RALPLAN 상태 머신
- goal ledger/checkpoint
- nudge budget
- Hoje hook/plugin namespace
- Claude Task state를 canonical source로 쓰는 규칙

## 4.9 라이선스·출처 처리

역할 prompt는 Hoje-Code 문장을 장문으로 복사하지 않고 원칙을 재작성한다.

- 아이디어와 역할 구조만 참고하면 별도 runtime attribution을 요구하지 않는다.
- 문장을 실질적으로 복사하는 경우 원 저장소의 라이선스와 저작권 고지를 확인하고 `NOTICE`에 출처를 남긴다.
- 구현 단계의 code review에서 prompt가 원문을 과도하게 복제하지 않았는지 확인한다.

---

# 5. 현재 DevezVibe 구조 분석

## 5.1 턴 전달 경로

현재 흐름은 다음과 같다.

```text
AppState
  ↓ submit / steer / queued prompt
src/main.rs
  ↓ turn/start 또는 turn/steer params
additionalContext
  ↓
src/backend.rs
  ├─ Claude → session/prompt 또는 session/steer
  ├─ OpenCode → start_prompt_content
  └─ Codex → turn/start 또는 turn/steer
```

## 5.2 `src/main.rs`

현재 책임:

- `DEVEZ_INSTRUCTIONS`
- thread start/resume parameter
- turn parameter 생성
- `turn_additional_context()`
- event loop
- `/btw` split focus
- renderer pick action
- provider handoff snapshot

에이전트 구현에서 변경할 부분:

- `mod agent`
- `AgentMode` import
- turn context builder에 agent state 전달
- agent badge click action
- 도움말/Tip 문구

## 5.3 `src/state.rs`

현재 책임:

- `AppState`
- editor, queue, busy, compaction
- model/effort/provider state
- Vibe/Response/Shell/Diff
- slash commands
- overlay/picker
- `ComposerMode` 생성
- input key 처리

에이전트의 selected mode, picker, Tab cycle, 변경 잠금은 이 파일의 책임이다.

## 5.4 `src/backend.rs`

현재 책임:

- visible thread와 provider backing session 연결
- provider switch/handoff
- Claude/Codex/OpenCode request 분배
- `combined_turn_instructions()`
- `prepare_codex_turn_context()`

역할 prompt transport는 이 계층에서 provider별로 변환한다.

## 5.5 `src/renderer.rs`

현재 책임:

- `ComposerMode`
- 컴포저 badge layout
- `Pick`과 clickable column
- narrow-width badge 생략
- hover와 repaint

에이전트 badge를 기존 badge 체계에 추가한다.

## 5.6 `src/open_code.rs`

현재 `start_prompt_content()`는 전달받은 instruction을 내부 `<devez-vibe-rules>` block으로 prompt 앞에 넣고, session history 복원 시 해당 내부 block을 숨긴다.

첫 버전에서는 수정하지 않는 것을 원칙으로 한다.

## 5.7 Claude bridge

Claude bridge는 `handoffContext`를 길이 정보가 있는 내부 prefix로 user content 앞에 붙이고 history 복원 시 전체 prefix를 제거한다.

첫 버전에서는 role context를 기존 transport에 함께 태우며 bridge protocol을 바꾸지 않는다.

---

# 6. 전체 시스템 구조

```text
┌──────────────────────────────────────────┐
│                AppState                  │
│ selected_agent_mode                      │
│ last_dispatched_agent_mode               │
│ standard_reset_required                  │
└───────────────────┬──────────────────────┘
                    │
                    ▼
┌──────────────────────────────────────────┐
│              src/agent.rs                │
│ enum / labels / picker details / prompts │
│ turn context policy / reset instruction  │
└───────────────────┬──────────────────────┘
                    │
                    ▼
┌──────────────────────────────────────────┐
│       main.rs turn_additional_context    │
│ devez-vibe-agent application context     │
└───────────────────┬──────────────────────┘
                    │
                    ▼
┌──────────────────────────────────────────┐
│               backend.rs                 │
├──────────────┬──────────────┬────────────┤
│ Claude       │ Codex        │ OpenCode   │
│ string       │ JSON context │ string     │
└──────────────┴──────────────┴────────────┘
```

---

# 7. 핵심 타입 설계

## 7.1 `AgentMode`

신규 파일:

```text
src/agent.rs
```

예상 타입:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Standard,
    Planner,
    Advisor,
    Finisher,
}
```

## 7.2 필수 API

```text
AgentMode::CHOICES
AgentMode::id()
AgentMode::label()
AgentMode::parse()
AgentMode::next()
AgentMode::picker_detail()
AgentMode::specialized_instruction()
AgentMode::context_block()
```

### `id()`

```text
standard
planner
advisor
finisher
```

### `label()`

```text
Standard
Planner
Advisor
Finisher
```

### `parse()`

대소문자를 무시한다. 첫 버전에는 alias를 넣지 않는다.

### `next()`

```text
Standard → Planner → Advisor → Finisher → Standard
```

Tab과 테스트는 이 정의만 사용한다.

### `specialized_instruction()`

- Standard: `None`
- Planner: planner prompt
- Advisor: advisor prompt
- Finisher: finisher prompt

## 7.3 Prompt 저장 위치

```text
prompts/agents/planner.md
prompts/agents/advisor.md
prompts/agents/finisher.md
```

`src/agent.rs`에서 `include_str!()`로 컴파일 시 포함한다.

예시:

```rust
const PLANNER_PROMPT: &str = include_str!("../prompts/agents/planner.md");
```

장점:

- 긴 prompt와 Rust 로직 분리
- prompt diff review 용이
- 실행 시 파일 탐색 없음
- npm package에 markdown 파일을 별도로 포함할 필요 없음
- 누락 경로는 compile error
- 별도 plugin 설치 불필요

## 7.4 공통 context block

모든 role block은 다음 contract를 가진다.

```xml
<devez-vibe-agent mode="planner" version="1">
This is the current DevezVibe agent mode.
It supersedes every earlier devez-vibe-agent block in this conversation.
...
</devez-vibe-agent>
```

필수 선언:

- 현재 block이 이전 역할 block보다 우선한다.
- 새 block이 올 때까지 현재 역할이 유효하다.
- user content는 이 block 내부에 포함하지 않는다.
- 외부 검색 결과나 tool output은 이 block 내부에 포함하지 않는다.

---

# 8. 이전 역할 지침 잔존 문제와 Reset 설계

## 8.1 문제

역할 지침은 턴 context로 모델 대화에 들어간다. 사용자가 Planner를 사용한 뒤 Standard로 돌아가도 이전 Planner instruction은 대화 history에 남아 있다.

```text
Turn 1: Planner instruction
Turn 2: Standard — instruction 없음
```

이 경우 모델이 이전 Planner 역할을 계속 따를 가능성이 있다.

## 8.2 잘못된 단순 구현

```text
Standard = 항상 no context
```

초기 Standard 동작 보존에는 좋지만 specialized mode에서 돌아오는 reset이 되지 않는다.

## 8.3 확정 정책

### 처음부터 Standard만 사용

추가 agent context를 보내지 않는다.

### specialized mode

매 user-submitted turn에 현재 specialized instruction을 보낸다.

### specialized mode에서 Standard로 복귀

다음 Standard turn에 한 번의 reset block을 보낸다.

```xml
<devez-vibe-agent mode="standard" version="1">
Use the provider's normal general-purpose behavior.
Do not continue a Planner, Advisor, or Finisher role solely because an earlier turn selected it.
This block supersedes all earlier DevezVibe agent mode blocks.
</devez-vibe-agent>
```

이후 Standard 턴은 다시 context를 생략한다.

### resume한 세션

새 프로세스는 과거 마지막 agent mode를 알 수 없다. resume한 transcript에 specialized instruction이 남아 있을 수 있으므로 첫 Standard prompt에 reset block을 한 번 보낸다.

## 8.4 필요한 상태

`AppState`에 다음 의미의 상태가 필요하다.

```rust
agent_mode: AgentMode,
last_dispatched_agent_mode: AgentMode,
standard_reset_required: bool,
```

초기화:

### 새 세션

```text
agent_mode = Standard
last_dispatched_agent_mode = Standard
standard_reset_required = false
```

### resume

```text
agent_mode = Standard
last_dispatched_agent_mode = Standard
standard_reset_required = true
```

### Planner 전송 성공

```text
last_dispatched_agent_mode = Planner
standard_reset_required = true
```

### Standard reset 전송 성공

```text
last_dispatched_agent_mode = Standard
standard_reset_required = false
```

## 8.5 성공 시점

reset 상태는 request를 만들 때가 아니라 provider가 turn request를 성공적으로 받아들였을 때 갱신한다.

전송 실패 시 reset requirement를 유지한다.

## 8.6 Provider switch

visible thread가 provider를 바꿔도 `standard_reset_required`를 유지한다.

provider handoff가 이전 역할의 출력·계획을 새 provider에 전달할 수 있으므로 Standard 전환 시 reset을 보내는 것이 안전하다.

---

# 9. AppState 상세 설계

## 9.1 필드

```rust
agent_mode: AgentMode,
last_dispatched_agent_mode: AgentMode,
standard_reset_required: bool,
```

첫 버전에는 다음을 추가하지 않는다.

- automatic route
- pipeline
- agent-specific model
- persisted agent metadata
- queued prompt별 agent snapshot

## 9.2 Getter/Setter

```text
agent_mode()
set_agent_mode(mode)
cycle_agent_mode()
open_agent_mode_picker()
agent_change_blocked()
agent_turn_context()
note_agent_dispatch_succeeded(mode, reset_sent)
```

## 9.3 역할 변경 가능 조건

다음 조건에서는 변경을 막는다.

```text
busy
compacting
provider_switch_pending
host_loading(resume)
queued_prompts not empty
running foreground/background subagents not empty
pending automatic continuation known
```

이유:

- busy: 현재 turn과 steer의 역할을 고정
- compacting: queued prompt와 summary 경계 보호
- provider switch: queued handoff prompt의 역할 보호
- host loading: resume transcript hydrate 중 상태 보호
- queued prompts: queue가 문자열만 저장하므로 역할 의미를 고정
- background subagent: 완료 notification이 열 자동 main-agent turn과 UI 역할 불일치 방지

## 9.4 변경 차단 notice

```text
• 현재 작업이 끝난 뒤 Agent를 변경할 수 있습니다.
```

세부 내부 상태를 사용자에게 노출하지 않는다.

## 9.5 queued prompt별 mode를 저장하지 않는 이유

역할 변경 잠금을 통해 queue 전체가 같은 역할을 사용하도록 보장한다.

이 방식은 다음 기존 구조를 보존한다.

```rust
VecDeque<String>
```

추후 queue마다 다른 역할을 지원하려면 별도 `QueuedPrompt { text, agent_mode }` migration으로 확장한다.

## 9.6 `/btw`

`/btw`는 main과 split pane에 각각 `AppState`를 가진다.

각 pane의 역할은 독립적이다.

```text
main pane: Planner
btw pane: Standard
```

bare Tab은 pane 전환에 사용되므로 agent 전환은 `/agent` 또는 badge click으로 수행한다.

## 9.7 Draft 보존

idle Tab으로 역할을 바꿔도 다음을 변경하지 않는다.

- editor text
- cursor
- image attachments
- stash
- completion source가 없는 일반 draft

## 9.8 Notice 발생

실제 역할이 변경되었을 때만 짧게 표시한다.

```text
• Agent: Advisor
```

같은 역할을 다시 선택하면 notice를 만들지 않는다.

---

# 10. 키 입력 우선순위

## 10.1 현재 충돌 요소

현재 Tab은 이미 다음 기능에 쓰인다.

- busy prompt queue
- slash completion 확정
- model/effort picker 이동
- 각종 overlay/picker 이동
- 질문 단계 이동
- MCP form 이동
- `/btw` pane focus 전환

따라서 bare Tab을 전역에서 먼저 가로채면 안 된다.

## 10.2 확정 우선순위

```text
1. 질문/승인/overlay/picker 내부 Tab
2. slash/@/$ completion Tab
3. /btw split focus Tab
4. busy 상태 queue Tab
5. idle ordinary composer Agent cycle
```

## 10.3 구현 위치

`AppState::handle_key()`의 기존 completion/pending/busy 분기 뒤, 일반 editor fallback 전에 처리한다.

개념:

```rust
KeyCode::Tab if self.busy => self.queue_editor(),
KeyCode::Tab if self.agent_change_blocked() => blocked_notice(),
KeyCode::Tab => self.cycle_agent_mode(),
```

단, `/btw` split focus는 `main.rs` event loop에서 먼저 소비하므로 기존 분기를 유지한다.

## 10.4 Shift+Tab

Agent 역순 전환에 사용하지 않는다.

현재 Claude permission 관련 고정 처리와 기존 테스트를 그대로 유지한다.

## 10.5 Steer

busy 상태의 Enter는 현재 turn steer다.

역할 변경이 busy 중 잠기므로 steer는 turn 시작과 동일한 role을 유지한다. `turn/steer`에도 동일 specialized context를 반복 전달한다.

---

# 11. `/agent` 명령과 Picker

## 11.1 SlashCommand

```text
name: /agent
description: Choose the active DevezVibe agent
takes_argument: true
```

## 11.2 명령 형식

```text
/agent
/agent standard
/agent planner
/agent advisor
/agent finisher
```

## 11.3 직접 선택

idle이고 변경 가능하면 즉시 적용한다.

잘못된 값:

```text
Usage
/agent [standard|planner|advisor|finisher]
```

## 11.4 Busy 처리

명령을 user prompt로 steer하지 않는다.

role 변경이 잠긴 상태라면 command는 로컬에서 소비하고 notice만 표시한다.

## 11.5 PendingInteraction

```rust
AgentModePicker {
    selected: usize,
}
```

초안의 `original` 복원 구조는 불필요하다. 재검수 후 picker는 후보 이동 중 실제 `agent_mode`를 변경하지 않는 것으로 수정한다.

## 11.6 Picker 키

- Up/Left/Ctrl+P: 이전
- Down/Right/Tab/Ctrl+N: 다음
- `1`~`4`: 해당 후보 선택 후 즉시 확정 또는 Enter와 동일 처리
- Enter: 확정
- Esc: 변경 없이 닫기

## 11.7 후보 설명

```text
Standard — 기존 기본 하네스로 일반 작업을 수행합니다.
Planner — 요구를 명확히 하고 저장소 기반 구현 계획을 검증합니다.
Advisor — 접근법의 위험, 장단점, 대안과 추천을 제시합니다.
Finisher — 구현, 검증, 리뷰를 완료 상태까지 밀어붙입니다.
```

## 11.8 Preview 정책

picker 내부 선택만 이동한다. 실제 badge와 context는 Enter 전까지 바뀌지 않는다.

장점:

- Esc 복원 로직 불필요
- arrow 이동 중 notice spam 방지
- background/queue 잠금 상태와 transient role 불일치 방지

---

# 12. UI 및 Renderer 계획

## 12.1 `ComposerMode`

추가 필드:

```rust
pub agent_mode: String,
```

선택적으로 이후 tone enum을 추가할 수 있으나 첫 버전은 기존 neutral/accent 계열을 재사용한다.

## 12.2 Badge 순서

```text
[branch] [Standard] [Vibe] [Response] [Fast] ...
```

Agent는 다음 입력의 행동을 결정하므로 Vibe보다 앞에 둔다.

## 12.3 Label

폭을 줄이기 위해 접두어 없이 표시한다.

```text
Standard
Planner
Advisor
Finisher
```

## 12.4 Click mapping

`Pick`에 추가:

```rust
AgentMode
```

badge layout 결과에 `agent_mode_index`를 저장한다.

`pick_action()`:

```text
Pick::AgentMode → open_agent_mode_picker 또는 blocked notice
```

## 12.5 Hover

기존 clickable badge와 동일하게 hover highlight를 적용한다.

## 12.6 폭 우선순위

Agent badge는 높은 우선순위로 유지한다.

좁아질 때 제거 순서의 예:

1. cost
2. Fast
3. Shell/Diff
4. Response 일부
5. Vibe 세부

Agent label은 가능한 한 마지막까지 유지한다.

## 12.7 Welcome/Thread 없음

첫 prompt 전에도 `AppState`가 존재하므로 badge를 표시한다. 사용자는 새 thread가 만들어지기 전에 Planner를 선택할 수 있다.

## 12.8 Renderer 테스트 폭

- 120 columns
- 100 columns
- 80 columns
- 56 columns
- 최소 안전 폭

검증:

- rule width
- agent text 존재
- 낮은 우선순위 badge 생략
- click column
- hover repaint
- branch 유무
- cost 유무
- fullscreen/inline

---

# 13. Turn Context 정책

## 13.1 Context key

```text
devez-vibe-agent
```

형식:

```json
{
  "devez-vibe-agent": {
    "value": "<devez-vibe-agent ...>...</devez-vibe-agent>",
    "kind": "application"
  }
}
```

## 13.2 `turn_additional_context()`

현재:

```rust
fn turn_additional_context(vibe: VibeMode) -> Value
```

변경안:

```rust
fn turn_additional_context(vibe: VibeMode, agent_context: Option<&str>) -> Value
```

`AgentMode` 자체보다 이미 계산된 context option을 전달하면 다음 로직을 분리할 수 있다.

- specialized prompt
- Standard reset
- Standard no-op

## 13.3 Standard baseline

새 세션에서 Standard만 사용하면 agent key를 만들지 않는다.

기존 context와 구조적으로 동일해야 한다.

## 13.4 Specialized role

각 user-submitted `turn/start`와 `turn/steer`에 current role context를 넣는다.

## 13.5 Standard reset

`standard_reset_required`일 때만 Standard reset block을 넣는다.

## 13.6 Context 순서

`src/backend.rs`의 `combined_turn_instructions()`가 만드는 현재 순서는 다음과 같다.

```text
1. Vibe mode notice        (Claude/Codex만)
2. provider handoff context
3. Claude-only reminder    (Claude만)
```

문서 1차·2차 초안은 handoff를 첫 번째로 적었으나 이는 실제 구현과 다르다. 기존 Vibe 안내는 handoff보다 앞에 있으며, 이 순서는 이번 기능에서 바꾸지 않는다.

확정 순서:

```text
1. Vibe mode notice        (기존 위치 유지)
2. provider handoff context
3. current agent instruction or Standard reset
4. Claude-only reminder    (기존 위치 유지)
```

이유:

- 기존 Vibe 안내와 Claude reminder의 상대 위치를 바꾸지 않아야 관련 회귀와 기존 테스트가 유지된다.
- handoff는 과거 대화 데이터에 가까우므로 current agent instruction이 그 뒤에 와야 과거 role을 명시적으로 supersede한다.
- Claude reminder는 최종 출력 형식 제약이므로 마지막에 남는다.

구현 시 `combined_turn_instructions()`의 `parts` 배열에 agent 항목을 handoff 다음, Claude reminder 앞에 삽입한다. 기존 항목의 순서를 재배치하지 않는다.

Codex는 application context map을 직접 받으므로 문자열 순서에 의존하지 않는다.

## 13.7 Trusted boundary

- agent prompt는 compile-time 정적 문자열이다.
- 사용자 입력을 agent block 안에 넣지 않는다.
- tool output이나 웹 결과를 agent block 안에 넣지 않는다.
- mode attribute는 enum에서만 생성한다.
- user-provided mode 문자열을 그대로 XML attribute로 사용하지 않는다.

---

# 14. Provider별 구현

## 14.1 Codex

### 현재 구조

- thread start/resume: `developerInstructions`
- turn: `additionalContext`
- `prepare_codex_turn_context()`가 중복 standing rules 제거

### 변경

- `devez-vibe-agent`는 제거하지 않는다.
- Standard no-op이면 key가 없다.
- reset/specialized context는 `kind: application`으로 유지한다.

### 테스트

- 공통 rules 제거 확인
- Claude-only key 제거 확인
- agent key 보존 확인
- Standard key 없음 확인

## 14.2 Claude Agent SDK

### 현재 구조

- session 생성: Claude Code preset system prompt + DevezVibe append
- turn: `handoffContext`
- bridge가 length-delimited internal prefix로 첫 user content 앞에 추가
- history 복원 시 prefix 전체 제거

### 변경

- backend의 combined context에 agent block 추가
- bridge protocol 변경 없음
- system prompt 재시작 없음

### 주의

`handoffContext`라는 필드명은 실제로 provider handoff뿐 아니라 Vibe reminder도 전달하고 있다. 첫 버전에는 rename하지 않는다. 이름 변경은 bridge/backend protocol 범위를 불필요하게 확대한다.

### 자동 후속 턴

Claude background task notification이 main-agent 자동 응답을 열 수 있다. 새 host prompt가 없으므로 별도 role block을 다시 주입할 수 없다.

완화:

- specialized block은 새 block까지 유효하다고 명시한다.
- background subagent가 살아 있는 동안 role change를 잠근다.
- 자동 후속 턴은 원래 role context를 계속 따른다.

## 14.3 OpenCode

### 현재 구조

`start_prompt_content()`가 instruction을 `<devez-vibe-rules>` 내부 block으로 prepend한다.

`combined_turn_instructions()`는 runtime별로 서로 다른 항목만 통과시킨다. OpenCode는 의도적으로 공통 rules와 Vibe 안내를 받지 않으며, 현재는 provider handoff만 전달된다.

```rust
let mode = (runtime != RuntimeKind::OpenCode).then(...)
let claude_reminder = (runtime == RuntimeKind::Claude).then(...)
```

### 필수 변경

agent block은 이 필터에서 OpenCode를 제외하지 않는다. 세 provider 모두 통과해야 한다.

```text
devez-vibe-rules            → Claude/Codex는 system·thread 쪽에서 보유, OpenCode 제외 유지
devez-vibe-mode             → OpenCode 제외 유지
claude-devez-vibe-reminder  → Claude 전용 유지
devez-vibe-agent            → Claude / Codex / OpenCode 모두 전달
```

이 예외를 넣지 않으면 OpenCode에서 역할 지침과 Standard reset이 한 번도 전달되지 않고, UI badge만 바뀌는 상태가 된다. 2차 초안의 "combined context에 agent block 포함"은 이 필터 변경을 전제로 한 서술이다.

`src/open_code.rs`는 여전히 수정하지 않는다. 변경 지점은 `src/backend.rs`의 통과 조건 한 곳이다.

### 그 밖의 변경

- `session/set_mode` 호출 없음
- native primary agent 이름과 DevezVibe agent를 동기화하지 않음

### history

기존 internal rules filtering이 전체 block을 숨기는지 회귀 테스트한다.

## 14.4 Provider switch

역할은 `AppState`에 있으므로 provider switch에 따라 변하지 않는다.

```text
Advisor + Claude
   ↓ provider switch
Advisor + Codex
```

새 provider의 첫 turn에도 current specialized context 또는 Standard reset을 전달한다.

---

# 15. 보안·권한·신뢰 경계

## 15.1 Agent mode는 보안 sandbox가 아니다

Planner/Advisor prompt에 edit 금지를 적어도 모델이 절대 수정하지 않는다는 보장은 없다.

사용자 문서와 UI에서 다음처럼 표현한다.

```text
Planner — 구현하지 않고 계획에 집중하도록 지시합니다.
Advisor — 구현하지 않고 기술 판단에 집중하도록 지시합니다.
```

“수정 권한이 제거된다”라고 표현하지 않는다.

## 15.2 현재 권한 모드와 독립

Agent mode는 다음을 변경하지 않는다.

- Codex permission profile
- Claude bypass/auto fallback
- OpenCode permission flow

## 15.3 향후 hard guard

후속 기능으로 분리한다.

필요 요소:

- provider별 Edit/Write intercept
- Bash mutation 분류
- allowlist/denylist
- user override UX
- plan/read-only badge
- provider parity test

## 15.4 Prompt injection

agent block은 trusted application context로 취급한다. 역할 prompt는 외부 콘텐츠를 포함하지 않는다.

외부 tool output은 기존 provider의 untrusted-content 정책을 따른다. 이번 기능은 별도 web trust wrapper를 추가하지 않는다.

---

# 16. 공통 역할 계약

모든 specialized 역할은 다음을 공유한다.

1. 기존 DevezVibe 공통 지침과 repository instructions를 존중한다.
2. 현재 provider에 실제 존재하는 도구만 사용한다.
3. 특정 provider 전용 도구를 다른 provider에서도 있다고 가정하지 않는다.
4. 저장소에서 확인 가능한 사실은 먼저 조사한다.
5. 사실, 추정, 권고, 미확인을 구분한다.
6. 실행하지 않은 명령이나 테스트를 실행했다고 주장하지 않는다.
7. 관련 없는 사용자 변경을 되돌리지 않는다.
8. 단순 작업을 불필요한 multi-agent workflow로 확대하지 않는다.
9. subagent는 독립 검토나 병렬성의 이득이 분명할 때만 사용한다.
10. 사용자 대신 비가역 제품 결정을 몰래 내리지 않는다.
11. 역할을 자동으로 바꾸거나 다른 역할 사용을 강제하지 않는다.
12. 최신 외부 사실이 correctness에 중요하면 provider 기본 검색을 사용할 수 있으나 Research workflow는 생성하지 않는다.

---

# 17. Standard 상세 설계

## 17.1 목적

평상시 사용하는 범용 모드다.

## 17.2 동작

기존 provider 기본 하네스가 자유롭게 수행한다.

- 질문 응답
- 저장소 조사
- 구현
- 테스트
- 디버깅
- 리팩터링
- 문서 작성
- 기본 subagent/tool 사용

## 17.3 Prompt 정책

### Clean Standard

agent context 없음.

### Reset Standard

과거 specialized role을 해제해야 할 때만 minimal reset block을 한 번 보낸다.

## 17.4 핵심 회귀 조건

- 새 세션의 첫 Standard turn payload는 기존과 동일
- specialized role을 쓰지 않은 Standard turn에 agent key 없음
- model/effort/provider/Vibe/permission 변동 없음
- 기존 응답 스타일 규칙 유지

## 17.5 Finisher와 차이

```text
Standard
- 기본 하네스가 적절한 범위에서 구현·검증

Finisher
- 목표 분해, 검증, 독립 리뷰·QA, blocker 해결, 최종 rerun을 명시적 완료 계약으로 요구
```

---

# 18. Planner 상세 설계

## 18.1 역할 정의

Planner는 Hoje Ask와 Hoje Plan의 핵심을 하나로 통합한다.

```text
요구 명확성 판단
  ↓
저장소 조사
  ↓
material intent reconciliation
  ↓
설계 선택과 대안
  ↓
Architect/Critic 관점 자체 검토
  ↓
구현 가능한 최종 계획
```

## 18.2 사용 상황

- 기능 설계
- 넓은 변경 범위
- 일부 모호한 요청
- 여러 구조 선택지
- migration/호환성 영향
- 구현 전에 위험·테스트 계획 필요
- 문서형 구현 계획 요청

## 18.3 요청 명확성 분류

### Clear

목표, surface, 범위, acceptance criteria가 충분하다.

행동:

- 질문 없이 저장소 조사
- 계획 작성

### Materially ambiguous

다음 중 하나가 설계·범위·안전을 바꾼다.

- 목표 surface
- 제품 동작 계약
- 데이터 ownership
- 호환성
- migration
- 권한·보안 경계
- acceptance criteria
- 비가역 결정
- 사용자만 선택할 수 있는 제품 의도

행동:

1. repository evidence 먼저 확인
2. evidence로 해결되지 않은 항목만 질문
3. 한 번에 가장 영향이 큰 질문 하나
4. 답변을 계획에 반영

### Non-material ambiguity

- 명명
- 작은 내부 구현 세부
- 쉽게 되돌릴 수 있는 선택
- 기존 패턴으로 명확히 추론 가능한 항목

행동:

- 합리적 가정을 명시
- 계획 계속

## 18.4 질문 규칙

- 한 번에 하나
- 왜 필요한지 짧게 설명
- 저장소 근거가 있으면 함께 제시
- 이미 답한 내용을 반복하지 않음
- 선택지가 있으면 결과 차이가 분명한 선택지 제공
- 명확한 요청에 의식적인 interview ceremony를 만들지 않음

## 18.5 저장소 조사 체크리스트

- 관련 파일
- 주요 symbol
- 호출 경로
- 데이터 흐름
- 상태 ownership
- UI event path
- persistence
- config/schema
- provider 차이
- 테스트 위치
- build/release 경로
- 현재 diff와 사용자 변경
- 문서와 compatibility contract

## 18.6 계획 출력 구조

1. 목표
2. 확인한 사실
3. 가정·미확인 사항
4. 범위
5. 비범위
6. 설계 원칙
7. 대안과 선택 근거
8. 변경 예상 파일·모듈
9. 데이터·상태·이벤트 흐름
10. 단계별 구현 순서
11. provider별 차이
12. 호환성·migration
13. 위험과 완화
14. 테스트·검증
15. 완료 조건

작은 작업에는 불필요한 섹션을 축약할 수 있으나 material 항목은 빠뜨리지 않는다.

## 18.7 대안 규칙

- 실제 viable option이 2개 이상이면 비교
- 한 개뿐이면 다른 후보가 왜 부적절한지 설명
- 억지 대안 생성 금지
- 선택 근거는 현재 저장소·요구와 연결

## 18.8 Architect 관점 자체 검토

- architecture consistency
- product contract
- compatibility
- security boundary
- state ownership
- provider parity
- observable behavior
- verification evidence

## 18.9 Critic 관점 자체 검토

- omitted surface
- sequencing error
- hidden dependency
- weak acceptance criteria
- test false positive
- rollback 누락
- implementation-time surprise
- 사용자 의도와 계획 불일치

material blocker가 있으면 사용자에게 초안을 그대로 내지 않고 먼저 수정한다.

## 18.10 수정 금지 계약

Planner prompt에는 다음을 명시한다.

- 제품 source edit/write 금지
- mutation-oriented shell 금지
- commit/push/PR 금지
- implementation worker 위임 금지
- 사용자가 “구현”이라고 적어도 현재 mode에서는 구현 계획만 작성

허용:

- read/search
- 상태 확인용 non-mutating command
- test command가 저장소를 변경하지 않는다고 확신할 때 검증 목적 사용

첫 버전에는 hard enforcement가 없음을 문서에 유지한다.

## 18.11 종료

자동으로 Finisher를 호출하지 않는다.

필요한 경우에만 짧게 다음 선택을 안내한다.

```text
계획 실행은 Standard 또는 Finisher에서 진행할 수 있습니다.
```

---

# 19. Advisor 상세 설계

## 19.1 역할 정의

Advisor는 사용자의 제안에 무조건 동의하거나 무조건 반대하지 않는 기술적 판단 보조자다.

```text
제안 확인
  ↓
저장소·제약 조사
  ↓
장점·위험·대안 평가
  ↓
필요한 반론
  ↓
조건부 추천
```

## 19.2 Planner와 차이

```text
Planner: 어떤 순서와 구조로 구현할 것인가?
Advisor: 제안한 접근을 선택하는 것이 적절한가?
```

Advisor가 완전한 구현 계획을 장황하게 작성하는 것은 기본 동작이 아니다.

## 19.3 평가 축

- 요구 적합성
- 현재 구조 적합성
- 단순성
- correctness
- 유지보수성
- 확장성
- 호환성
- migration 비용
- 성능
- 보안·권한
- 운영·관측 가능성
- 테스트 가능성
- rollback 가능성
- 팀 복잡도

## 19.4 반론 조건

다음과 같은 material 근거가 있을 때 반론한다.

- correctness 위험
- 데이터 손실
- 호환성 파손
- 보안 경계 약화
- 불필요한 복잡성
- 더 단순하고 동등한 대안
- 유지보수 비용이 명백함
- 기존 contract와 불일치

## 19.5 반론하지 않을 조건

- 단순 취향 차이
- 현재 요구에는 영향 없는 미래 가능성
- 근거 없는 확장성 우려
- 원안이 가장 단순하고 안전함

원안이 적절하면 명확히 승인한다.

## 19.6 심각도

```text
Must fix
Recommended
Optional
```

blocker와 suggestion을 섞지 않는다.

## 19.7 기본 출력 구조

1. 결론
2. 근거
3. 필수 우려
4. 권장 개선
5. 대안 비교
6. 최종 추천
7. 추천이 달라지는 조건

실제 필수 우려가 없으면 빈 섹션을 억지로 만들지 않는다.

## 19.8 조사 규칙

- 판단에 repository 구조가 중요하면 먼저 조사
- 최신 외부 API 사실이 중요하면 provider 기본 search 사용 가능
- 확인하지 못한 내용은 추정으로 표시
- 외부 사실보다 현재 저장소 contract가 우선인 경우 이를 분리 설명

## 19.9 수정 금지

Advisor는 제품 파일을 직접 구현하지 않는다.

사용자가 “평가하고 구현해”라고 하더라도 현재 role에서는 판단과 권고까지만 수행한다.

이 경계는 prompt-level 계약이다.

---

# 20. Finisher 상세 설계

## 20.1 역할 정의

Finisher는 Hoje Goals의 목표 완결 책임을 가져온다.

```text
승인된 계획 또는 실행 brief
  ↓
범위·완료 조건 확인
  ↓
실행 강도 선택
  ↓
목표 분해
  ↓
구현
  ↓
검증
  ↓
독립 review/QA
  ↓
blocker 수정
  ↓
최종 전체 rerun
  ↓
증거 기반 완료 보고
```

## 20.2 입력 우선순위

1. 현재 대화에서 사용자가 승인한 구체적 계획
2. 사용자가 제공한 PRD/plan/brief
3. 현재 사용자 요청

Planner 결과가 없다는 이유로 실행을 거부하지 않는다.

## 20.3 실행 강도

가장 낮은 안전 수준을 선택한다.

### Light

적합:

- 로컬 저위험 변경
- 대략 2개 이하 파일
- 대략 200 net lines 미만
- cross-layer 아님

행동:

- 직접 구현
- targeted test
- self-review
- 최종 rerun

### Standard

적합:

- 3개 이상 파일
- 대략 200 lines 이상
- UI/backend/provider cross-layer
- 독립 slice가 있음

행동:

- 명시적 단계 계획
- 필요 시 implementation slice 위임
- independent review 또는 QA 관점
- regression 확인
- 최종 전체 rerun

### Strict

적합:

- auth/security
- 결제
- destructive data path
- migration
- concurrency
- public API
- production infrastructure
- 최대 검증 요청

행동:

- 비사소한 slice 분리
- 독립 review + QA/red-team
- adversarial case 확대
- rollback/compatibility 확인
- 전체 rerun

## 20.4 강도 승격

Light로 시작했어도 다음을 발견하면 승격한다.

- 예상보다 넓은 파일 범위
- cross-layer dependency
- migration
- auth/security
- concurrency
- public contract 변경
- 테스트 surface 확대

## 20.5 Goal 분해

분해 기준:

- 독립 구현 가능
- 독립 검증 가능
- layer가 다름
- 병렬성 이득
- review boundary가 다름

같은 acceptance surface와 final review boundary를 공유하는 validation-coupled 작업은 한 goal로 유지한다.

## 20.6 구현 규칙

- 수정 전 조사
- 가장 단순한 호환 구현
- 관련 없는 변경 보존
- 범위 밖 리팩터링 억제
- 실패 무시 금지
- 변경된 가정 기록
- target behavior 중심

## 20.7 Subagent 사용

특정 이름을 강제하지 않는다.

```text
Use the provider's available subagent capability when it creates real independence or parallelism.
```

- Claude native Agent/Task 사용 가능
- OpenCode task/subagent 사용 가능
- Codex는 현재 제공 기능에 맞춤
- 지원하지 않으면 주 에이전트가 sequential lanes 수행
- 다른 provider CLI를 shell로 강제 호출하지 않음

## 20.8 Independent review

구현과 다른 관점에서 확인한다.

- architecture/product contract
- code correctness
- compatibility
- regression
- actual user surface
- adversarial case
- artifact existence

같은 컨텍스트에서 self-review할 경우에도 “구현 완료를 증명하려는 관점”이 아니라 “깨뜨리려는 관점”으로 다시 읽도록 prompt에 명시한다.

## 20.9 검증 gate

1. slice별 targeted verification
2. 관련 regression
3. actual user-facing surface
4. artifact 존재
5. diff review
6. final full rerun

Strict 추가:

- adversarial tests
- rollback
- compatibility boundary
- failure scenario
- security boundary
- observability

## 20.10 Blocker 분류

### Resolvable

- build error
- test failure
- missing implementation
- 조사 가능한 ambiguity
- 설치 가능한 dependency

행동:

- 조사
- 수정
- 재검증
- 필요 시 subtask 추가

### Human-blocked

- credential/secret
- 외부 승인
- 접근 권한
- 물리적·수동 작업
- 비가역 제품 결정

행동:

- 완료한 범위
- blocker
- 필요한 최소 사용자 행동
- 이후 재개 지점

## 20.11 완료 조건

- 요청 범위 구현
- acceptance criteria 확인
- 필요한 test 실행
- review blocker 없음
- QA blocker 없음
- final rerun 확인
- 미검증 영역 명시

검증하지 못한 경우 완료라고 표현하지 않는다.

## 20.12 최종 보고

- 결과
- 변경 영역
- 검증 명령과 결과
- 발견·수정한 문제
- 남은 위험
- human blocker와 다음 행동

Hoje receipt 형식은 사용하지 않는다.

---

# 21. Prompt 초안 구조

실제 구현 시 다음 skeleton을 기반으로 문장을 다듬는다. 아래는 최종 prompt가 아니라 구현 contract다.

## 21.1 Planner skeleton

```text
You are the active DevezVibe Planner.
This mode supersedes earlier DevezVibe agent modes.

Mission
- Clarify material intent and produce an implementation-ready repository-grounded plan.

Boundaries
- Do not edit product files, commit, push, open PRs, or delegate implementation.
- Read and inspect before asking the user.

Process
1. Determine whether the request is clear or materially ambiguous.
2. Investigate repository facts first.
3. Ask one high-impact question only when evidence cannot resolve it.
4. Produce a bounded plan with paths, contracts, risks, and verification.
5. Review it from architecture and critic perspectives and repair material gaps.

Output
- Facts, assumptions, scope, alternatives, implementation steps, risks, verification, completion criteria.
```

## 21.2 Advisor skeleton

```text
You are the active DevezVibe Advisor.
This mode supersedes earlier DevezVibe agent modes.

Mission
- Evaluate proposed implementation choices and improve the user's technical decision.

Boundaries
- Do not implement or modify product files.
- Do not manufacture objections.

Process
1. Inspect relevant repository facts.
2. Evaluate correctness, simplicity, compatibility, security, maintenance, and testability.
3. Separate must-fix issues from recommendations and optional improvements.
4. Compare viable alternatives.
5. Recommend the best choice and state conditions that would change it.
```

## 21.3 Finisher skeleton

```text
You are the active DevezVibe Finisher.
This mode supersedes earlier DevezVibe agent modes.

Mission
- Drive the user's goal to a verified completion state.

Process
1. Use an approved plan when present; otherwise derive a bounded execution brief.
2. Choose the lowest safe intensity: light, standard, or strict.
3. Implement the smallest compatible change while preserving unrelated work.
4. Verify targeted behavior.
5. Perform independent review and QA appropriate to risk.
6. Fix resolvable blockers and rerun verification.
7. Claim completion only with evidence.

Do not over-orchestrate small tasks.
```

## 21.4 Standard reset skeleton

```text
You are now in DevezVibe Standard mode.
Use the provider's normal general-purpose behavior.
Do not continue Planner, Advisor, or Finisher behavior only because earlier turns selected it.
This block supersedes all earlier DevezVibe agent mode blocks.
```

---

# 22. Prompt 품질·크기 정책

## 22.1 크기 권장 상한

```text
Planner: 2,000~4,000 tokens
Advisor: 1,500~3,000 tokens
Finisher: 2,500~5,000 tokens
Standard reset: 100~250 tokens
```

## 22.2 중복 금지

역할 prompt에 반복하지 않는다.

- 한국어 출력 규칙
- Vibe 설명
- Response 길이 정책
- tool UI 표시 규칙
- provider 인증 설명
- Hoje CLI 설명

## 22.3 정적 금지 문자열 검사

role prompt에 다음이 남지 않는지 검사한다.

```text
.hoje
ralplan
ultragoal
hoje-code:
HOJE_SESSION_ID
ledger.jsonl
goals.json
```

## 22.4 Prompt version

wrapper에 작은 schema version을 둔다.

```text
version="1"
```

사용자 설정이 아니라 내부 protocol marker다.

---

# 23. 파일별 구현 계획

| 파일 | 변경 내용 |
|---|---|
| `src/agent.rs` | 신규 `AgentMode`, prompt include, context/reset policy, parse/cycle/detail, 정적 테스트 |
| `prompts/agents/planner.md` | Planner prompt |
| `prompts/agents/advisor.md` | Advisor prompt |
| `prompts/agents/finisher.md` | Finisher prompt |
| `src/state.rs` | agent 상태, reset 상태, change lock, `/agent`, picker, idle Tab, ComposerMode 값, tests |
| `src/main.rs` | `mod agent`, context 계산, dispatch 성공 기록, badge pick, Tip/help 갱신 |
| `src/renderer.rs` | agent badge, `Pick::AgentMode`, hover/click/width tests |
| `src/backend.rs` | combined context에 agent 추가, Codex key 보존, provider tests |
| `README.md` | 사용자 역할·전환 설명 |
| `npm/README.md` | npm 사용자 역할·전환 설명 |
| `CLAUDE.md` | dynamic role prompt 위치와 변경 원칙 안내 |

원칙적으로 수정하지 않는 파일:

```text
src/claude.rs
src/open_code.rs
npm/bridge/claude-agent-sdk-bridge.mjs
npm/package.json
```

기존 transport로 충족할 수 없다는 증거가 있을 때만 변경 범위를 재검토한다.

---

# 24. 상세 구현 의사코드

## 24.1 Agent context 계산

```rust
fn next_agent_context(&self) -> Option<AgentTurnContext> {
    match self.agent_mode {
        AgentMode::Standard if self.standard_reset_required => {
            Some(AgentTurnContext::StandardReset)
        }
        AgentMode::Standard => None,
        mode => Some(AgentTurnContext::Specialized(mode)),
    }
}
```

## 24.2 Dispatch 성공 기록

```rust
fn note_agent_dispatch_succeeded(&mut self, context: Option<AgentTurnContext>) {
    match context {
        Some(AgentTurnContext::Specialized(mode)) => {
            self.last_dispatched_agent_mode = mode;
            self.standard_reset_required = true;
        }
        Some(AgentTurnContext::StandardReset) => {
            self.last_dispatched_agent_mode = AgentMode::Standard;
            self.standard_reset_required = false;
        }
        None => {}
    }
}
```

## 24.3 역할 전환

```rust
fn set_agent_mode(&mut self, mode: AgentMode) -> Action {
    if self.agent_change_blocked() {
        self.set_composer_notice("• 현재 작업이 끝난 뒤 Agent를 변경할 수 있습니다.");
        return Action::Tick(true);
    }
    if self.agent_mode == mode {
        return Action::None;
    }
    self.agent_mode = mode;
    self.set_composer_notice(format!("• Agent: {}", mode.label()));
    Action::Tick(true)
}
```

`standard_reset_required`는 선택 시점에 false로 만들지 않는다. 다음 Standard turn이 실제 전송되어야 해제된다.

## 24.4 Context 생성

```rust
let agent_context = state.next_agent_context();
let mut params = json!({
    // existing fields
    "additionalContext": turn_additional_context(
        state.vibe_mode(),
        agent_context.as_ref().map(AgentTurnContext::render),
    )
});
```

## 24.5 Request 결과

provider request가 성공하면 해당 turn에 사용한 `agent_context` snapshot으로 state를 갱신한다.

요청 중 사용자가 mode를 바꿀 수 없도록 dispatch 시작부터 busy/transition 상태가 설정되어 있어야 한다.

---

# 25. State lifecycle matrix

| 상황 | 역할 변경 | 다음 turn context |
|---|---:|---|
| 새 실행, 첫 prompt | 가능 | Standard면 없음 |
| resume hydrate 중 | 불가 | 해당 없음 |
| resume 완료, 첫 Standard prompt | 가능 | Standard reset |
| idle, draft 작성 중 | 가능 | 선택 role |
| busy | 불가 | 현재 role 유지 |
| steer | 불가 | 현재 role 반복 |
| queued prompt 존재 | 불가 | queue 생성 당시 role 유지 |
| compacting | 불가 | 현재 role 유지 |
| provider switch pending | 불가 | 현재 role 유지 |
| background subagent live | 불가 | originating role 유지 |
| specialized → specialized | 가능 | 새 specialized block |
| specialized → Standard | 가능 | Standard reset 1회 |
| Standard reset 성공 후 Standard | 가능 | agent context 없음 |
| request 실패 | 잠금 해제 후 가능 | reset/specialized dispatch 상태 미갱신 |

---

# 26. 테스트 계획

## 26.1 `src/agent.rs`

1. default = Standard
2. cycle order
3. parse case-insensitive
4. invalid parse
5. specialized prompt 존재
6. clean Standard = None
7. Standard reset 존재
8. wrapper mode 정확
9. wrapper version 정확
10. forbidden Hoje runtime strings 없음
11. prompt max length
12. 사용자 입력을 받는 formatting API 없음

## 26.2 `AppState`

1. new session Standard/no reset
2. resume Standard/reset required
3. idle Tab cycle
4. 네 번 후 Standard
5. draft text/cursor 보존
6. image attachments 보존
7. busy Tab queue 유지
8. busy `/agent` 차단
9. compacting 차단
10. provider switch pending 차단
11. queued prompts 차단
12. foreground subagent 차단
13. background subagent 차단
14. slash completion Tab 우선
15. question Tab 우선
16. picker Tab 우선
17. `/btw` focus Tab 우선
18. Shift+Tab 기존 동작
19. `/agent` picker
20. numeric select
21. Esc no-change
22. Enter commit
23. direct `/agent planner`
24. invalid usage
25. model/effort/Vibe unchanged
26. provider switch role 유지
27. same role no notice

## 26.3 Reset state

1. clean Standard context 없음
2. Planner dispatch 성공 → reset required
3. Planner dispatch 실패 → state 미변경
4. Standard 선택만으로 reset 해제 안 됨
5. Standard reset 성공 → reset false
6. Standard reset 실패 → reset true
7. Planner → Advisor → Standard reset
8. resume first Standard reset
9. provider switch 후 Standard reset
10. compaction 후 reset 유지

## 26.4 Context builder

1. Standard clean JSON baseline equality
2. Planner key
3. Advisor key
4. Finisher key
5. Standard reset key
6. existing Vibe key 유지
7. existing rules 유지
8. Claude reminder 유지
9. XML attribute enum-generated
10. role prompt에 user text 없음

## 26.5 Backend — Codex

1. standing rules 제거
2. Claude-only key 제거
3. agent key 유지
4. Standard agent key 없음
5. steer에도 agent key 유지

## 26.6 Backend — Claude

1. combined order = handoff → agent → mode → reminder
2. Planner 포함
3. Standard reset 포함
4. clean Standard 미포함
5. system prompt unchanged
6. bridge history에서 internal prefix 제거
7. automatic task continuation 중 role 변경 잠금

## 26.7 Backend — OpenCode

1. `combined_turn_instructions()`가 OpenCode runtime에서 agent context를 통과시킴
2. 같은 호출에서 공통 rules와 Vibe 안내는 계속 제외됨
3. agent context가 start_prompt_content로 전달
4. internal rules block 안에 포함
5. history replay에서 숨김
6. native session mode 변경 없음
7. Standard reset도 OpenCode에 전달됨

## 26.8 Renderer

1. Standard badge
2. Planner badge
3. Advisor badge
4. Finisher badge
5. branch 앞/뒤 순서
6. Pick::AgentMode
7. hover
8. cost 유무 click 안정
9. 120 width
10. 100 width
11. 80 width
12. 56 width
13. minimum width
14. fullscreen
15. inline

## 26.9 역할 수동 시나리오

### Standard

- 기존 일반 구현과 유사
- 역할 설명을 매번 출력하지 않음

### Planner clear

- 질문 없이 조사·계획
- edit 없음

### Planner ambiguous

- repo 조사 먼저
- material question 하나
- final self-review

### Advisor sound proposal

- 억지 반론 없음
- 승인 근거

### Advisor risky proposal

- must/recommended/optional 분리
- 대안과 조건부 추천
- edit 없음

### Finisher light

- 과도한 subagent 없음
- targeted verification
- final rerun

### Finisher standard

- 적절한 분해
- review/QA
- blocker 수정

### Finisher strict

- adversarial/compatibility/rollback
- 증거 기반 완료

## 26.10 Cross-role 시나리오

1. Planner turn → Advisor turn: Advisor block이 이전 Planner를 supersede
2. Advisor turn → Finisher turn: Finisher가 실행
3. Finisher turn → Standard: reset 후 일반 동작
4. Planner 선택 후 prompt 없이 Standard 복귀: reset 불필요
5. resume specialized history → Standard first prompt: reset

---

# 27. 검증 명령

구현 시 최소 실행:

```text
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
node npm/bridge/claude-agent-sdk-bridge.mjs --self-test
node scripts/check-codex-compatibility.mjs
```

추가 smoke test:

- Windows x64 fullscreen
- Windows x64 inline
- Claude 새 세션/resume
- Codex 새 세션/resume
- OpenCode 새 세션/resume
- provider handoff
- `/btw`
- background subagent
- compaction

---

# 28. 구현 단계

## Phase 1 — Agent core

- `src/agent.rs`
- enum/API
- prompt files
- reset block
- static tests

완료 조건:

- compile-time prompt 포함
- cycle/parse/reset tests
- Hoje runtime 문자열 없음

## Phase 2 — AppState와 입력

- state fields
- change lock
- `/agent`
- picker
- idle Tab
- resume reset flag

완료 조건:

- queue, completion, split, Shift+Tab 회귀 없음

## Phase 3 — UI

- ComposerMode field
- badge
- Pick
- hover
- width tests

완료 조건:

- 좁은 폭 안정
- click mapping 정확

## Phase 4 — Context transport

- turn context 계산
- dispatch 성공 기록
- backend combined context
- Codex key preservation

완료 조건:

- 세 provider role context 수신
- clean Standard baseline 유지
- Standard reset 동작

## Phase 5 — Role prompt 품질

- Planner scenarios
- Advisor scenarios
- Finisher intensity/completion gate
- prompt 크기/중복 정리

## Phase 6 — 문서와 release 준비

- README
- npm README
- Tip/help
- `CLAUDE.md`
- version bump/release는 별도 승인

---

# 29. 위험과 완화

## 29.1 Standard 회귀

위험: 새 시스템 때문에 기본 payload가 변함.

완화:

- clean Standard no agent context
- baseline equality test
- reset은 필요할 때만

## 29.2 이전 역할 잔존

위험: Planner/Finisher가 Standard 복귀 후에도 계속 영향.

완화:

- superseding block contract
- one-time Standard reset
- resume reset

## 29.3 Background 자동 턴 불일치

위험: UI는 Planner인데 이전 Finisher background completion이 자동 응답.

완화:

- live subagent 동안 role change 잠금
- specialized block 지속 contract

## 29.4 Planner/Advisor mutation

위험: prompt-only 경계 위반.

완화:

- 강한 no-mutation prompt
- manual tests
- UI 설명에서 soft boundary 명시
- hard guard는 후속 설계

## 29.5 Advisor 과잉 반론

완화:

- do not manufacture objections
- sound proposal test
- severity 분리

## 29.6 Finisher 과잉 orchestration

완화:

- lowest safe intensity
- Light 기준
- subagent value gate

## 29.7 Tab 충돌

완화:

- 명시적 우선순위
- busy queue regression
- `/btw`, completion, picker tests

## 29.8 Prompt history 노출

완화:

- Claude length-delimited prefix strip
- OpenCode internal block filter
- Codex application context
- resume history smoke test

## 29.9 좁은 UI

완화:

- 짧은 label
- agent high priority
- width/click tests

## 29.10 Prompt 비용

완화:

- concise prompts
- Standard no-op
- duplicated common rules 금지
- token 상한 test

---

# 30. 호환성·Migration·Rollback

## 30.1 Migration 없음

- config schema 변경 없음
- route store 변경 없음
- transcript schema 변경 없음
- Claude bridge protocol 변경 없음
- OpenCode ACP protocol 변경 없음
- Codex thread metadata 변경 없음

## 30.2 Resume

- role 선택은 Standard로 초기화
- 첫 Standard prompt에 reset
- 과거 conversation은 그대로 유지

## 30.3 Rollback

기능 제거 순서:

1. `/agent`와 Tab/badge 제거
2. AppState fields 제거
3. context key 제거
4. backend combined context 원복
5. prompt files와 `agent.rs` 제거

저장 파일 migration이 없으므로 rollback 후 잔여 persistent state가 없다.

---

# 31. 완료 승인 기준

## 31.1 기능

- 네 역할 선택
- Standard 기본값
- idle Tab cycle
- `/agent`
- badge click
- provider switch 후 역할 유지

## 31.2 역할

- Standard 기본 하네스
- Planner repo-first clarification/plan/self-review, 구현 없음
- Advisor evidence-based recommendation/pushback, 구현 없음
- Finisher implementation/verification/review/QA/completion evidence

## 31.3 생명주기

- busy/compaction/provider switch/queue/subagent 중 변경 잠금
- steer role 안정
- Standard reset
- resume reset
- failed request state 보존

## 31.4 회귀

- busy Tab queue
- `/btw` Tab
- slash completion
- question/picker Tab
- Shift+Tab
- Vibe/Response/Fast
- history restore
- narrow UI

## 31.5 품질

- fmt/test/clippy
- Claude bridge self-test
- Codex compatibility check
- provider smoke tests

---

# 32. 재검수 방법

1차 초안을 작성한 뒤 다음 관점으로 다시 검토했다.

## 32.1 저장소 구조 대조

- `src/main.rs` 턴 생성·Tab split 처리
- `src/state.rs` busy queue·picker·completion·AppState
- `src/backend.rs` provider context 변환
- `src/renderer.rs` badge/click/width
- `src/open_code.rs` internal rules history filtering
- Claude bridge의 prefix/historical stripping

## 32.2 Hoje-Code 대조

- Planner 경계
- Architect evidence rule
- Critic omitted-surface rule
- Executor bounded implementation
- Executor-QA real-surface/adversarial validation
- Ask의 one-question/repo-first 원칙
- Plan의 planning/execution boundary
- Goals의 light/standard/strict와 completion gate

## 32.3 실패 시나리오 검토

- 이전 역할이 history에 남음
- resume 세션의 stale role
- background task 자동 턴
- busy Tab 충돌
- `/btw` Tab 충돌
- queued prompt 역할 모호성
- picker preview transient state
- request failure 후 reset state 손실
- narrow UI click drift

---

# 33. 재검수에서 발견한 문제와 수정 결과

| 발견 항목 | 1차 초안 문제 | 2차 반영 |
|---|---|---|
| Standard 복귀 | Standard에 context를 전혀 안 보내면 이전 role이 남음 | one-time Standard reset 추가 |
| Resume | 과거 specialized role을 알 수 없음 | resumed session 첫 Standard turn reset |
| Background task | role 변경 후 자동 후속 턴과 UI 불일치 | live subagent 동안 변경 잠금 |
| Queued prompt | queue가 문자열만 저장해 role snapshot 없음 | queue가 비지 않으면 변경 잠금 |
| Compaction | busy가 아닌 compaction 중 역할 변경 가능성 | compaction 중 변경 잠금 |
| Provider switch | handoff 대기 prompt의 역할 모호성 | provider switch pending 중 변경 잠금 |
| Picker preview | arrow 이동마다 실제 mode 변경·notice 가능 | Enter commit-only picker로 수정 |
| Dispatch 실패 | request 생성 시 reset을 해제하면 상태 손실 | 성공 후에만 dispatch state 갱신 |
| Claude history | 개별 agent tag strip으로 오해 가능 | 기존 length-delimited 전체 prefix strip을 사용한다고 명확화 |
| Context 순서 | 현재 역할이 handoff/history에 묻힐 수 있음 | handoff 뒤에 current role 배치 |
| Context 순서 실제값 | 초안이 handoff를 첫 항목으로 기술했으나 구현은 Vibe 안내가 먼저임 | 실제 순서를 기준으로 재작성하고 기존 항목 재배치 금지 |
| OpenCode 전달 | OpenCode는 combined 필터에서 handoff만 통과해 역할이 전달되지 않음 | agent key를 세 provider 모두 통과시키는 예외를 명시 |
| Planner read-only | 보안 경계처럼 오해 가능 | prompt-level 계약임을 반복 명시 |
| Finisher background | 자동 continuation에 새 context 없음 | role block 지속 선언 + 변경 잠금 |
| License | Hoje prompt 복제 정책 누락 | 재작성 원칙과 attribution 기준 추가 |
| UI 초기 상태 | thread 생성 전 agent 선택 설명 부족 | welcome/first prompt 전 badge 지원 추가 |
| Steer | role snapshot 설명 부족 | busy role 잠금과 turn/steer context 반복 추가 |

---

# 34. 최종 구현 체크리스트

## 설계

- [ ] `AgentMode` source of truth가 한 곳인가
- [ ] Standard clean/no-op과 reset이 분리되었는가
- [ ] 이전 block을 supersede한다고 prompt에 명시했는가
- [ ] provider-native agent와 혼합하지 않았는가

## 상태

- [ ] new/resume 초기화가 다른가
- [ ] dispatch 성공 후 상태 갱신인가
- [ ] queue/subagent/compaction/provider switch 잠금인가
- [ ] `/btw` AppState 독립성이 유지되는가

## UX

- [ ] idle Tab만 cycle하는가
- [ ] busy Tab queue가 유지되는가
- [ ] picker는 commit-only인가
- [ ] badge click과 `/agent`가 같은 setter를 쓰는가

## Prompt

- [ ] Hoje runtime 용어가 제거되었는가
- [ ] common Devez rules를 중복하지 않는가
- [ ] provider 전용 tool을 강제하지 않는가
- [ ] Planner/Advisor soft boundary를 과장하지 않는가
- [ ] Finisher가 작은 작업을 과도하게 분해하지 않는가

## Transport

- [ ] Codex agent key가 제거되지 않는가
- [ ] Claude bridge 변경 없이 전달되는가
- [ ] OpenCode history에 internal block이 노출되지 않는가
- [ ] Standard reset이 provider switch/resume에도 전달되는가

## Test

- [ ] state tests
- [ ] renderer tests
- [ ] backend tests
- [ ] prompt static tests
- [ ] three-provider smoke tests
- [ ] cross-role reset tests

---

# 35. 후속 확장 후보

수동 네 역할이 안정화된 뒤에만 검토한다.

1. provider 공통 hard read-only guard
2. queued prompt별 agent snapshot
3. session별 role persistence
4. custom agent prompt
5. Automatic router
6. Research/Insane Search tool
7. Finisher intensity UI
8. agent별 model recommendation
9. explicit role pipeline

---

# 36. 최종 결정표

| 항목 | 최종 결정 |
|---|---|
| 역할 | Standard, Planner, Advisor, Finisher |
| 기본값 | Standard |
| 자동 라우팅 | 없음 |
| Research | 없음 |
| 전환 | idle Tab, `/agent`, badge |
| busy Tab | 기존 queue 유지 |
| `/btw` Tab | pane focus 유지 |
| 역할 저장 | AppState 수명만 |
| Resume | Standard + 첫 turn reset |
| Standard prompt | clean 상태 없음, 필요 시 reset 1회 |
| Specialized prompt | 매 user-submitted turn 반복 |
| Prompt 저장 | markdown + `include_str!()` |
| Planner | Ask + Plan 통합 |
| Advisor | Architect + Critic 기반 |
| Finisher | Goals 핵심, runtime 제외 |
| Hard read-only | 첫 버전 제외 |
| OpenCode native mode | 사용 안 함 |
| Hoje 설치 | 불필요 |
| Bridge protocol 변경 | 원칙적으로 없음 |
| Persistent schema 변경 | 없음 |

---

# 부록 A. 역할 선택 예시

```text
"이 버튼 오류 고쳐줘"
→ Standard

"이 기능을 어떤 구조로 넣을지 계획해줘"
→ Planner

"이 상태를 컬럼 하나로 처리하려는데 괜찮아?"
→ Advisor

"이 계획대로 구현하고 테스트와 리뷰까지 끝내"
→ Finisher
```

역할은 자동 선택되지 않는다. 위 예시는 사용자 가이드의 의미 구분이다.

---

# 부록 B. Hoje-Code → DevezVibe 변환표

| Hoje 개념 | DevezVibe 적용 | 제외 요소 |
|---|---|---|
| Deep interview | Planner의 material ambiguity 처리 | threshold/state/artifact |
| Planner | Planner repo-first 계획 | receipt writer |
| Architect | Planner self-review + Advisor 평가 | persisted lane |
| Critic | Planner gap review + Advisor 반론 | review pass state |
| Executor | Finisher bounded implementation | goal ledger |
| Executor-QA | Finisher independent QA | structured receipt |
| Light/Standard/Strict | Finisher 실행 강도 | CLI flag/state |
| Checkpoint | 완료 증거 보고 | ledger/checkpoint command |

---

# 부록 C. 구현자가 피해야 할 단축 구현

1. `DEVEZ_INSTRUCTIONS` 전체를 역할별로 복제하지 않는다.
2. Standard에 항상 긴 reset prompt를 보내지 않는다.
3. role 변경 시 새 provider session을 만들지 않는다.
4. OpenCode `session/set_mode`만 사용해 provider별 동작을 갈라놓지 않는다.
5. busy Tab을 agent cycle로 덮어쓰지 않는다.
6. Planner를 Claude plan permission mode와 동일시하지 않는다.
7. prompt 파일을 runtime npm asset으로 따로 읽지 않는다.
8. Hoje SKILL.md 전체를 복사하지 않는다.
9. Finisher에 무조건 subagent를 쓰라고 지시하지 않는다.
10. Planner/Advisor를 보안 sandbox라고 문서화하지 않는다.
