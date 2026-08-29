# DevezVibe 4종 에이전트 시스템 상세 구현 계획서

> 문서 상태: 1차 초안  
> 대상 브랜치: `main`  
> 대상 기능: `Standard`, `Planner`, `Advisor`, `Finisher`  
> 제외 기능: Automatic 라우팅, 별도 Research 에이전트, Insane Search, Hoje 런타임/상태 파일  
> 이 문서는 구현 계획만 정의하며 제품 소스 코드는 수정하지 않는다.

---

## 1. 문서 목적

이 문서는 DevezVibe에 사용자가 즉시 전환할 수 있는 네 가지 에이전트 모드를 내장하기 위한 구현 계획을 정의한다.

```text
Standard → Planner → Advisor → Finisher → Standard
```

핵심 목표는 다음과 같다.

1. 현재 Claude Agent SDK, Codex app-server, OpenCode ACP의 기본 하네스를 유지한다.
2. 별도 플러그인이나 스킬 설치 없이 DevezVibe 바이너리 내부에 역할 지침을 포함한다.
3. 모델, provider, effort, Vibe 설정과 독립된 공통 에이전트 계층을 만든다.
4. `Standard`는 현재 동작과 완전히 동일한 기본값으로 둔다.
5. `Planner`는 Hoje Ask와 Hoje Plan의 좋은 동작을 합친 요구 명확화·설계 전용 역할로 만든다.
6. `Advisor`는 Hoje Architect/Critic의 검토 철학을 바탕으로 추천·반론·대안 비교를 담당한다.
7. `Finisher`는 Hoje Goals의 완결 실행 철학을 가져오되 `.hoje`, ledger, receipt, CLI 의존성 없이 동작한다.
8. 모든 provider에서 같은 역할명, 같은 UI, 같은 전환 규칙, 최대한 유사한 행동을 제공한다.

이 계획의 우선순위는 “기능을 많이 넣는 것”이 아니라 “현재 기본 하네스를 훼손하지 않으면서 역할 차이를 명확하게 만드는 것”이다.

---

## 2. 최종 사용자 경험

### 2.1 기본 상태

DevezVibe를 시작하면 에이전트는 항상 `Standard`로 시작한다.

```text
Standard
```

`Standard`에서는 기존 DevezVibe와 동일하게 Claude Code/Codex/OpenCode의 기본 하네스가 일반 질문, 조사, 구현, 테스트를 자유롭게 수행한다.

### 2.2 수동 전환

사용자는 idle 상태에서 `Tab`을 눌러 다음 순서로 에이전트를 전환한다.

```text
Standard
  ↓ Tab
Planner
  ↓ Tab
Advisor
  ↓ Tab
Finisher
  ↓ Tab
Standard
```

추가로 `/agent` 명령과 컴포저의 에이전트 배지를 제공한다.

```text
/agent
/agent standard
/agent planner
/agent advisor
/agent finisher
```

### 2.3 화면 표시

컴포저 상단 배지에 현재 역할을 짧게 표시한다.

```text
Standard
Planner
Advisor
Finisher
```

`Agent: Planner`처럼 접두어를 붙이지 않고 역할명만 표시해 좁은 터미널의 폭을 절약한다.

배지는 클릭 가능하며 클릭하면 `/agent`와 동일한 선택창을 연다.

### 2.4 전환 범위

에이전트 선택은 다음 항목을 변경하지 않는다.

- 현재 provider
- 모델
- reasoning effort
- Fast/service tier
- Vibe 모드
- Response 표시 모드
- Shell/Diff 표시 설정
- Claude 권한 모드
- 사용자·프로젝트의 `AGENTS.md`, `CLAUDE.md`, skills, MCP 설정
- 현재 대화 기록

에이전트 전환은 오직 다음 턴에 추가되는 역할 지침만 바꾼다.

---

## 3. 범위와 비범위

## 3.1 이번 구현 범위

- `Standard`, `Planner`, `Advisor`, `Finisher` 네 역할
- 기본값 `Standard`
- idle 상태의 bare `Tab` 순환
- `/agent` picker
- `/agent <name>` 직접 선택
- 클릭 가능한 컴포저 배지
- provider 공통 턴 지침 주입
- 역할별 내장 prompt
- 역할별 단위 테스트
- provider별 전달 회귀 테스트
- UI 폭·클릭 위치·기존 키 충돌 회귀 테스트
- README와 도움말 문서화

## 3.2 명시적으로 제외할 기능

이번 구현에는 다음을 넣지 않는다.

- `Automatic` 요청 분류·라우팅
- 여러 역할을 자동으로 이어 붙이는 pipeline
- 별도 `Research` primary agent
- Insane Search 및 직접 웹 fetch 엔진
- Hoje marketplace/plugin 자동 설치
- `.hoje` 디렉터리나 상태 파일 생성
- Hoje CLI, `ralplan`, `ultragoal` 실행
- receipt, `sha256`, `stage_n`, `ledger.jsonl`, `goals.json`
- 모델별 자동 교체
- 에이전트별 별도 모델 지정
- 에이전트별 provider 자동 변경
- 에이전트별 별도 비용 정책
- OpenCode native primary agent와의 직접 동기화
- Planner/Advisor에 대한 provider 공통 hard read-only sandbox

마지막 항목은 중요하다. 첫 버전의 Planner/Advisor “수정 금지”는 강한 역할 지침이지만 보안 경계는 아니다. Claude, Codex, OpenCode 모두에서 완전히 동일한 hard tool restriction을 걸려면 provider별 권한·도구 차단 계층을 별도로 설계해야 한다. 첫 버전은 역할 품질과 전환 일관성을 먼저 완성한다.

---

## 4. Hoje-Code에서 참고할 요소

DevezVibe는 Hoje-Code를 런타임으로 포함하지 않는다. 대신 다음 역할 철학만 재작성하여 내장한다.

## 4.1 참고 파일

`MrHoje/devez-marketplace` 기준:

- `plugins/hoje-code/agents/planner.md`
- `plugins/hoje-code/agents/architect.md`
- `plugins/hoje-code/agents/critic.md`
- `plugins/hoje-code/agents/executor.md`
- `plugins/hoje-code/agents/executor-qa.md`
- `plugins/hoje-code/skills/hoje-ask/SKILL.md`
- `plugins/hoje-code/skills/hoje-plan/SKILL.md`
- `plugins/hoje-code/skills/hoje-goals/SKILL.md`

## 4.2 가져올 원칙

### Planner 계열

- 저장소를 먼저 조사한다.
- 사실과 가정을 분리한다.
- 사용자 요구에서 material한 모호성만 질문한다.
- 저장소에서 알 수 있는 사실을 사용자에게 다시 묻지 않는다.
- 계획에 변경 경로, 계약, 위험, 검증 명령을 포함한다.
- 구현을 시작하지 않는다.
- Architect 관점과 Critic 관점으로 계획을 자체 검토한다.

### Architect/Advisor 계열

- 요청된 계약과 실제 저장소 상태를 함께 본다.
- 아키텍처, 동작, 호환성, 보안 경계, 검증 증거를 평가한다.
- 근거 없이 승인하거나 반대하지 않는다.
- 실제 blocker와 선택적 개선을 분리한다.

### Critic 계열

- 누락된 surface를 찾는다.
- 순서 오류와 숨은 의존성을 찾는다.
- 약한 acceptance criteria를 찾는다.
- 테스트가 통과해도 실제 동작이 깨질 수 있는 경우를 찾는다.
- 문제를 지적할 때 정확한 보완 방법을 함께 제시한다.

### Executor/Finisher 계열

- 한 번에 bounded한 목표를 처리한다.
- 관련 없는 사용자 변경을 보존한다.
- 가장 단순하고 호환 가능한 구현을 우선한다.
- targeted verification을 실행한다.
- 구현자와 독립된 review/QA 관점을 둔다.
- 증거 없이 완료라고 하지 않는다.
- 해결 가능한 blocker를 이유로 쉽게 포기하지 않는다.

## 4.3 가져오지 않을 요소

- Hoje 이름과 명령어를 사용자에게 노출하는 규칙
- `.hoje` 상태 계약
- workflow artifact writer
- Planner/Architect/Critic receipt 전달
- review pass 번호와 persisted subagent ID
- conflict disposition JSON schema
- 최대 5회 RALPLAN 반복 상태 머신
- goals ledger와 checkpoint 명령
- nudge budget
- Hoje 전용 hook·plugin namespace
- Claude 전용 `TaskCreate`를 canonical state로 간주하는 규칙

## 4.4 적용 방식

원문 SKILL.md를 통째로 system prompt에 넣지 않는다. 원문의 목적과 검증 규칙만 추출하고 DevezVibe의 구조에 맞는 짧고 provider-neutral한 prompt로 다시 작성한다.

목표는 “Hoje-Code를 번들링”하는 것이 아니라 다음과 같다.

```text
Hoje 역할 철학
      ↓ 재작성
DevezVibe built-in agent prompt
      ↓
Claude / Codex / OpenCode
```

---

## 5. 현재 DevezVibe 구조 분석

## 5.1 현재 턴 전달 흐름

현재 일반적인 턴 흐름은 다음과 같다.

```text
AppState
  ↓ 사용자 입력
src/main.rs
  ↓ turn/start params 생성
additionalContext
  ↓
src/backend.rs
  ├─ Claude → session/prompt
  ├─ OpenCode → start_prompt_content
  └─ Codex → turn/start
```

`src/main.rs`는 `turn_additional_context()`에서 DevezVibe 공통 지침과 Vibe 관련 턴 지침을 만든다.

`src/backend.rs`는 `combined_turn_instructions()`에서 Claude/OpenCode에 전달할 문자열형 턴 컨텍스트를 조합한다. Codex는 `additionalContext` 객체를 직접 받으며 `prepare_codex_turn_context()`에서 이미 thread-level developer instructions에 들어간 중복 공통 지침만 제거한다.

이 구조는 동적 에이전트 지침을 넣기에 적합하다.

## 5.2 현재 provider별 지침 위치

### Codex

- thread start/resume 시 `developerInstructions`에 DevezVibe 공통 규칙을 전달한다.
- 매 턴 `additionalContext`를 전달한다.
- 에이전트 지침은 매 턴 동적이므로 `additionalContext`에 남긴다.

### Claude

- session 생성 시 Claude Code preset system prompt에 DevezVibe 공통 규칙을 append한다.
- 에이전트는 세션 중 바뀌므로 system prompt를 재생성하지 않는다.
- 현재 Vibe reminder와 provider handoff가 전달되는 턴 컨텍스트 경로에 에이전트 지침을 추가한다.
- bridge는 해당 prefix를 history 표시에서 제거하므로 사용자 transcript를 오염시키지 않는다.

### OpenCode

- `start_prompt_content()`가 전달받은 instruction을 내부 `<devez-vibe-rules>` 블록으로 prompt 앞에 넣는다.
- session load 시 해당 내부 블록을 history에서 숨긴다.
- 에이전트 지침도 같은 턴 instruction 경로를 사용한다.
- 첫 버전에서는 `session/set_mode`를 사용하지 않는다. native agent와 DevezVibe agent의 의미가 엇갈리는 것을 방지하기 위해 DevezVibe가 provider-independent 역할을 소유한다.

## 5.3 현재 상태와 UI 위치

`src/state.rs`의 `AppState`가 다음을 소유한다.

- editor와 queued prompts
- busy/turn 상태
- selected model/effort
- Vibe/Response/Shell/Diff 표시 상태
- pending picker/overlay
- provider/model switch 상태
- composer에 표시할 `ComposerMode`

에이전트 선택도 `AppState`가 소유하는 것이 맞다.

`src/renderer.rs`의 `ComposerMode`와 `Pick`은 컴포저 배지와 클릭 동작을 연결한다. 따라서 에이전트 배지도 같은 체계에 넣어야 한다.

---

## 6. 전체 설계 원칙

## 6.1 DevezVibe가 역할 상태를 소유한다

provider가 현재 역할을 소유하거나 추론하게 만들지 않는다.

```text
AppState.agent_mode
      ↓
turn context builder
      ↓
provider adapter
```

UI는 provider 응답을 보고 역할을 추측하지 않고 `AppState.agent_mode`를 그대로 표시한다.

## 6.2 역할과 모델을 분리한다

다음 조합은 모두 가능해야 한다.

```text
Planner + Claude Sonnet
Planner + Claude Opus
Planner + GPT
Planner + OpenCode model
```

역할 전환은 모델을 바꾸지 않는다.

## 6.3 Standard는 무주입이다

`Standard`의 가장 중요한 계약은 기존 동작 보존이다.

`Standard`일 때는 `devez-vibe-agent` 추가 context 자체를 만들지 않는다. 빈 문자열을 보내는 방식도 피한다. provider가 기존과 동일한 payload를 받도록 한다.

## 6.4 비-Standard 역할은 턴 단위로 주입한다

`Planner`, `Advisor`, `Finisher`는 다음 턴마다 역할 지침을 받는다.

세션 시작 때 한 번만 넣으면 중간 전환이 불가능하고, provider를 바꾸면 역할이 누락될 수 있다. 따라서 역할은 항상 turn-level context이다.

## 6.5 Prompt는 공통 규칙과 분리한다

```text
기존 DEVEZ_INSTRUCTIONS
+ 기존 Vibe turn notice
+ 선택된 Agent prompt
```

역할 prompt에 한국어 출력 규칙, 응답 길이, tool 표시 규칙을 중복해서 넣지 않는다. 출력 형식은 기존 DevezVibe 공통 지침이 계속 담당한다.

## 6.6 첫 버전은 자동 pipeline이 아니다

`Planner → Finisher`는 사용자가 직접 전환한다.

Planner가 계획을 끝냈다고 자동으로 Finisher를 호출하거나 모드를 바꾸지 않는다. Finisher도 Planner를 자동 실행하지 않는다. Finisher는 현재 대화에 승인된 계획이 있으면 활용하고, 없으면 현재 요청을 실행 brief로 사용한다.

---

## 7. 신규 모듈 설계

## 7.1 `src/agent.rs`

신규 모듈이 다음 책임을 가진다.

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

필수 API:

```text
AgentMode::CHOICES
AgentMode::label()
AgentMode::id()
AgentMode::parse()
AgentMode::next()
AgentMode::picker_detail()
AgentMode::turn_instruction()
```

### `label()`

- `Standard`
- `Planner`
- `Advisor`
- `Finisher`

### `id()`

- `standard`
- `planner`
- `advisor`
- `finisher`

### `parse()`

대소문자를 무시한다.

허용값:

```text
standard
planner
advisor
finisher
```

불필요한 alias는 첫 버전에 넣지 않는다.

### `next()`

순서를 코드 한 곳에서만 정의한다.

```text
Standard → Planner → Advisor → Finisher → Standard
```

Tab, picker, 테스트가 모두 이 정의를 사용한다.

### `turn_instruction()`

- Standard: `None`
- Planner: Planner prompt
- Advisor: Advisor prompt
- Finisher: Finisher prompt

반환값을 `Option<&'static str>` 또는 `Option<Cow<'static, str>>`로 두고 Standard가 완전히 무주입임을 타입으로 표현한다.

## 7.2 Prompt 파일

추천 경로:

```text
prompts/agents/planner.md
prompts/agents/advisor.md
prompts/agents/finisher.md
```

`src/agent.rs`에서 `include_str!()`로 컴파일 타임에 포함한다.

장점:

- Rust 코드와 긴 prompt를 분리할 수 있다.
- prompt review가 쉽다.
- 최종 바이너리에 내장되므로 npm 설치 후 별도 파일이 필요 없다.
- `npm/package.json`의 `files` 목록을 수정하지 않아도 된다.
- 사용자 PC에 Hoje plugin을 설치하지 않아도 된다.

Prompt 파일은 runtime에서 읽지 않는다. 누락·경로 문제를 빌드 시점에 잡는다.

## 7.3 Prompt 공통 wrapper

역할 prompt를 전달할 때 다음과 같은 명확한 경계를 사용한다.

```xml
<devez-vibe-agent mode="planner">
...
</devez-vibe-agent>
```

wrapper 생성은 `agent.rs` 또는 `main.rs`의 context builder 한 곳에서 수행한다.

역할 prompt 내부의 사용자 입력이나 외부 콘텐츠를 이 wrapper 안에 섞지 않는다.

---

## 8. AppState 구현 계획

## 8.1 필드 추가

`AppState`에 다음 필드를 추가한다.

```rust
agent_mode: AgentMode,
```

첫 버전에는 `active_turn_agent`, `automatic_route`, pipeline 상태를 추가하지 않는다.

## 8.2 초기값

`AppState::new()`는 항상 `AgentMode::Standard`로 초기화한다.

새 실행과 resume 모두 새 프로세스에서는 Standard로 시작한다.

## 8.3 생명주기

선택한 역할은 같은 `AppState`가 살아 있는 동안 유지한다.

유지되는 경우:

- 모델 전환
- effort 전환
- Claude ↔ Codex ↔ OpenCode provider 전환
- 같은 실행 안의 session attach/resume
- Vibe/Response/Shell/Diff 변경

초기화되는 경우:

- DevezVibe 프로세스를 새로 시작

첫 버전에는 파일이나 route store에 역할을 영구 저장하지 않는다.

이유:

- 잠깐 Planner를 사용한 뒤 다음 실행이 Planner로 열리는 surprise를 방지한다.
- 기존 설정 파일 schema를 늘리지 않는다.
- 기본값 Standard라는 제품 계약을 명확히 한다.

## 8.4 메서드

```text
agent_mode()
set_agent_mode(mode)
cycle_agent_mode()
open_agent_picker()
can_change_agent()
```

`set_agent_mode()`는 실제 변경이 있을 때만 composer notice와 redraw를 발생시킨다.

예시 notice:

```text
• Agent: Planner
```

## 8.5 busy 상태 처리

에이전트 전환은 active turn 동안 금지한다.

이 결정은 현재 키 동작과 queued prompt 의미를 보호한다.

현재 busy 상태의 Tab은 composer 내용을 다음 prompt로 queue하는 데 사용된다. 이를 에이전트 전환으로 덮어쓰면 기존 핵심 UX가 깨진다.

busy 상태에서 다음 동작을 시도하면 역할을 바꾸지 않고 notice만 표시한다.

- `/agent`
- `/agent planner`
- agent badge 클릭

예시:

```text
• 응답 완료 후 Agent를 변경할 수 있습니다.
```

busy 상태의 `Tab`은 기존 queue 동작을 그대로 유지한다.

## 8.6 queued prompt 정책

첫 버전에는 queued prompt마다 별도 AgentMode를 저장하지 않는다.

이유:

- busy 중 Agent 변경을 금지하므로 queued prompt가 어느 역할을 사용할지 모호하지 않다.
- `VecDeque<String>`을 새 구조체로 바꾸지 않아도 된다.
- queue 삭제·표시·provider switch 로직의 변경 범위를 줄인다.

---

## 9. 키 입력 및 충돌 처리

## 9.1 우선순위

Tab 처리 우선순위는 다음과 같다.

```text
1. Pending overlay/question/picker 자체 Tab 처리
2. Slash command completion의 Tab 자동완성
3. /btw split view의 pane focus 전환
4. Busy 상태의 prompt queue
5. Idle 일반 composer의 Agent cycle
```

이 순서를 지켜야 기존 기능이 깨지지 않는다.

## 9.2 idle Tab

다음 조건일 때만 agent를 순환한다.

- active overlay가 없음
- slash completion이 Tab을 소비하지 않음
- `/btw` split focus 전환 상태가 아님
- busy가 아님
- modifier가 없음
- key kind가 Press 또는 Repeat

composer에 입력 중인 텍스트가 있어도 텍스트를 보존한 채 역할만 변경한다.

## 9.3 busy Tab

현재 동작 유지:

```text
Tab → queue_editor()
```

## 9.4 Shift+Tab

현재 permission 관련 처리 또는 고정 모드 no-op을 그대로 유지한다. Agent 역순 전환에 사용하지 않는다.

## 9.5 `/btw`

split view에서 bare Tab은 pane focus 전환이 우선이다.

이 상태에서 Agent를 바꾸려면:

- `/agent` 사용
- agent badge 클릭

각 pane은 자신의 `AppState.agent_mode`를 가진다.

---

## 10. `/agent` 명령과 Picker

## 10.1 SlashCommand 등록

```text
name: /agent
description: Choose the active DevezVibe agent
takes_argument: true
```

## 10.2 명령 동작

### `/agent`

picker를 연다.

### `/agent standard`

즉시 Standard로 변경한다.

### `/agent planner`

즉시 Planner로 변경한다.

### `/agent advisor`

즉시 Advisor로 변경한다.

### `/agent finisher`

즉시 Finisher로 변경한다.

### 잘못된 값

오류 block:

```text
Usage
/agent [standard|planner|advisor|finisher]
```

## 10.3 PendingInteraction

```rust
AgentModePicker {
    selected: usize,
    original: AgentMode,
}
```

## 10.4 Picker 키

- Up/Left/Ctrl+P: 이전
- Down/Right/Tab/Ctrl+N: 다음
- `1`~`4`: 해당 역할 선택
- Enter: 확정
- Esc: original 복원 후 닫기

Vibe picker와 같은 preview/restore 패턴을 사용한다.

## 10.5 Picker 설명

```text
Standard — 기존 기본 하네스로 일반 작업을 수행합니다.
Planner — 요구를 명확히 하고 구현 계획을 검증합니다. 제품 파일은 수정하지 않습니다.
Advisor — 제안한 접근의 장단점, 위험, 대안과 추천을 제공합니다.
Finisher — 구현, 검증, 리뷰를 끝까지 완료하는 데 집중합니다.
```

---

## 11. UI 및 Renderer 구현 계획

## 11.1 `ComposerMode`

다음 필드를 추가한다.

```rust
pub agent_mode: String,
```

필요하면 후속 단계에서 tone enum을 추가할 수 있으나 첫 버전은 기존 accent 계열을 재사용한다.

## 11.2 배지 순서

추천 순서:

```text
[branch] [agent] [Vibe] [Response] [Fast] ...
```

에이전트는 사용자가 현재 입력을 어떤 역할로 보낼지 결정하는 핵심 상태이므로 Vibe보다 앞에 둔다.

## 11.3 폭 우선순위

- 에이전트 배지는 높은 우선순위로 유지한다.
- 매우 좁은 폭에서는 비용·Fast·Shell·Diff 같은 낮은 우선순위 배지를 먼저 숨긴다.
- 에이전트 이름 자체는 축약하지 않는다.
- `Agent:` 접두어를 생략한다.

## 11.4 클릭 처리

`Pick`에 다음 variant를 추가한다.

```rust
AgentMode
```

badge layout에 `agent_mode_index`를 추가하고 클릭 column을 정확히 연결한다.

`main.rs`의 `pick_action()`은 다음과 같이 처리한다.

```text
Pick::AgentMode → state.open_agent_picker()
```

busy 상태에서는 picker를 열지 않고 변경 불가 notice를 보여준다.

## 11.5 UI 테스트 폭

최소 다음 폭을 검증한다.

- 120 columns: 모든 주요 배지 표시
- 80 columns: agent + Vibe + Response 유지
- 56 columns: agent가 잘리지 않고 낮은 우선순위 배지가 빠짐
- 더 좁은 임계값: rule width와 cursor 위치가 깨지지 않음

클릭 pick이 실제 agent 문자열의 column에만 매핑되는지도 테스트한다.

---

## 12. 턴 Context 구현 계획

## 12.1 `turn_additional_context()` 확장

현재 signature:

```rust
fn turn_additional_context(vibe: VibeMode) -> Value
```

변경안:

```rust
fn turn_additional_context(vibe: VibeMode, agent: AgentMode) -> Value
```

Standard에서는 기존 JSON과 동일하게 유지한다.

Planner 예시:

```json
{
  "devez-vibe-agent": {
    "value": "<devez-vibe-agent mode=\"planner\">...</devez-vibe-agent>",
    "kind": "application"
  }
}
```

## 12.2 Standard 무주입 테스트

Standard context에 다음 pointer가 없어야 한다.

```text
/additionalContext/devez-vibe-agent
```

기존 `devez-vibe-rules`, `devez-vibe-mode`, Claude reminder의 값은 변경되지 않아야 한다.

## 12.3 `combined_turn_instructions()`

Claude와 OpenCode에 에이전트 지침을 포함한다.

추천 조합 순서:

```text
provider handoff context
agent instruction
Vibe mode notice
Claude reminder
```

역할 지침을 이전 대화 기록보다 뒤에 두어 현재 턴의 행동 계약이 명확하게 유지되도록 한다.

각 부분은 빈 문자열이면 제외한다.

## 12.4 Codex

`prepare_codex_turn_context()`는 `devez-vibe-agent`를 제거하지 않는다.

삭제 대상은 기존처럼 session-level에 이미 존재하는 공통 rules와 Claude 전용 항목뿐이다.

회귀 테스트로 agent key가 Codex payload에 남는지 확인한다.

## 12.5 Claude

Claude bridge 수정 없이 기존 `handoffContext` transport를 재사용한다.

실제 의미는 provider handoff만이 아니라 per-turn internal context지만 현재 Vibe reminder도 이 경로를 사용하므로 첫 버전에서는 transport rename을 하지 않는다.

에이전트 지침은 history 복원 시 사용자 prompt로 보이지 않아야 한다.

## 12.6 OpenCode

기존 `start_prompt_content()`의 instruction prefix 경로를 사용한다.

- agent context가 `<devez-vibe-rules>` 내부 전달 블록에 포함된다.
- session load가 해당 내부 block을 사용자 history에서 숨긴다.
- OpenCode native agent mode는 바꾸지 않는다.

---

## 13. 공통 에이전트 계약

모든 비-Standard prompt는 다음 공통 원칙을 공유한다.

1. 기존 DevezVibe 시스템 지침과 프로젝트 지침을 우선 존중한다.
2. 현재 provider가 제공하는 도구만 사용한다.
3. 존재하지 않는 도구나 agent 이름을 가정하지 않는다.
4. 저장소에서 확인 가능한 사실은 먼저 조사한다.
5. 사실, 추정, 권고를 구분한다.
6. 실행했다고 주장하려면 실제 실행 증거가 있어야 한다.
7. 관련 없는 사용자 변경을 되돌리지 않는다.
8. 단순한 작업을 불필요하게 복잡한 workflow로 만들지 않는다.
9. 서브에이전트는 독립 검토나 병렬성이 실제 이득일 때만 사용한다.
10. 역할 전환을 사용자에게 강요하거나 자동으로 바꾸지 않는다.

---

# 14. Standard 상세 계획

## 14.1 목적

평상시 사용하는 범용 모드다.

## 14.2 동작

- 기존 Claude Code/Codex/OpenCode 하네스 그대로 동작
- 일반 질문
- 저장소 조사
- 코드 구현
- 버그 수정
- 테스트
- 리팩터링
- 문서 작성
- provider 기본 subagent/tool 사용

## 14.3 Prompt

추가 prompt 없음.

```text
AgentMode::Standard.turn_instruction() == None
```

## 14.4 중요한 회귀 조건

- Standard를 추가한 뒤 기존 payload가 바뀌지 않아야 한다.
- 기존 모델 응답 스타일이 달라지지 않아야 한다.
- 기존 permission, Vibe, queue, provider switch 동작이 달라지지 않아야 한다.
- `devez-vibe-agent` key가 존재하지 않아야 한다.

## 14.5 Standard와 Finisher 차이

Standard도 구현과 테스트를 잘할 수 있다. Finisher를 별도로 두는 이유는 “구현 가능 여부”가 아니라 “완료 계약의 강도”다.

```text
Standard: provider 기본 판단에 맡김
Finisher: 분해·검증·리뷰·재실행·완료 증거를 명시적으로 요구
```

---

# 15. Planner 상세 계획

## 15.1 역할 정의

Planner는 Hoje Ask와 Hoje Plan을 합친다.

```text
요구 명확화
  ↓
저장소 조사
  ↓
설계 선택
  ↓
Architect/Critic 관점 자체 검토
  ↓
구현 가능한 최종 계획
```

Planner는 제품 파일을 구현하지 않는다.

## 15.2 사용 상황

- 기능을 어떻게 설계할지 결정할 때
- 변경 범위가 넓을 때
- 요구가 일부 모호할 때
- 여러 대안의 구조적 비교가 필요할 때
- 구현 전에 위험과 검증 계획이 필요할 때
- 사용자가 구현 계획서만 원할 때

## 15.3 명확성 판단

첫 단계에서 요청을 다음처럼 분류한다.

### Clear

목표, 범위, acceptance criteria가 충분하다.

- 질문 없이 저장소 조사와 계획으로 이동한다.

### Materially ambiguous

아래 항목 중 하나가 구현 방향을 바꾼다.

- 대상 surface
- 범위
- 제품 동작 계약
- 호환성
- 데이터 손실 가능성
- acceptance criteria
- 보안/권한 경계
- 사용자만 결정할 수 있는 제품 선택

처리:

1. 저장소에서 먼저 조사한다.
2. 조사로 해결되지 않은 항목만 묻는다.
3. 한 번에 가장 영향이 큰 질문 하나만 한다.
4. 답변 후 바로 계획을 갱신한다.

### Non-material ambiguity

명명, 사소한 구현 세부, 쉽게 되돌릴 수 있는 선택은 합리적 가정을 명시하고 계획을 계속한다.

## 15.4 저장소 조사 계약

Planner는 계획 전에 다음을 확인한다.

- 관련 파일과 symbol
- 호출 경로와 데이터 흐름
- 기존 패턴
- 테스트 위치
- config/schema 영향
- provider별 영향
- 사용자 변경과 현재 diff
- 관련 문서

읽을 수 있는 사실을 사용자에게 묻지 않는다.

## 15.5 계획 출력 계약

최종 결과는 최소 다음 섹션을 포함한다.

1. 목표
2. 확인한 저장소 사실
3. 가정과 열린 결정
4. 범위와 비범위
5. 설계 선택과 대안
6. 변경 예상 파일/모듈
7. 단계별 구현 순서
8. 상태·데이터·UI 흐름
9. provider별 차이
10. 위험과 완화
11. 테스트·검증 계획
12. 완료 조건

모든 작업에 억지로 여러 대안을 만들지 않는다. 실제 대안이 하나뿐이면 다른 선택지가 왜 부적절한지 짧게 설명한다.

## 15.6 Architect 관점 자체 검토

최종안 전에 다음을 점검한다.

- 아키텍처 일관성
- 사용자 동작 계약
- 이전 버전 호환성
- provider 간 차이
- 보안·권한 경계
- 데이터·세션 생명주기
- UI 상태의 소유자
- 검증 증거가 실제 문제를 잡는지

## 15.7 Critic 관점 자체 검토

- 누락된 surface
- 숨은 의존성
- 잘못된 단계 순서
- acceptance criteria 누락
- 테스트가 통과해도 깨질 수 있는 실제 동작
- rollback 경로 누락
- 구현 단계에서 결정해야 할 사항을 계획이 숨기고 있지 않은지

material한 문제가 있으면 최종 출력 전에 계획을 수정한다.

## 15.8 수정 금지 계약

Planner prompt에 다음을 명시한다.

- 제품 source edit/write 금지
- mutation-oriented shell 금지
- commit/push/PR 금지
- implementation worker 위임 금지
- 사용자가 “구현”이라고 말해도 Planner 모드에서는 구현 계획만 작성

단, 첫 버전에는 provider 공통 hard enforcement가 없으므로 이 계약은 prompt-level behavioral rule이다.

## 15.9 종료 동작

Planner는 자동으로 Finisher를 실행하지 않는다.

최종 문장은 필요할 때 다음 행동만 안내한다.

```text
이 계획을 실행하려면 Finisher 또는 Standard로 전환할 수 있습니다.
```

불필요하게 매 답변마다 전환을 광고하지 않는다.

---

# 16. Advisor 상세 계획

## 16.1 역할 정의

Advisor는 수동적인 동의자가 아니라 기술적 판단 보조자다.

```text
사용자 제안
  ↓
저장소·조건 확인
  ↓
장점/위험/대안 평가
  ↓
필요하면 반론
  ↓
추천과 선택 조건 제시
```

## 16.2 Planner와 차이

```text
Planner: 어떻게 구현할 것인가?
Advisor: 그 방법을 선택하는 것이 적절한가?
```

Advisor는 완전한 작업 순서를 작성하는 것이 목적이 아니다. 선택을 평가하고 의사결정을 돕는 것이 목적이다.

## 16.3 평가 항목

- 요구와 접근법의 일치
- 현재 저장소 구조와의 일치
- 단순성
- 유지보수성
- 확장성
- 호환성
- migration 비용
- 운영·관측 가능성
- 성능
- 보안·권한
- 테스트 가능성
- 되돌리기 용이성
- 팀이 감당할 복잡성

## 16.4 반론 규칙

Advisor는 반론을 만들기 위해 억지로 반대하지 않는다.

반론 조건:

- 실제 correctness 위험
- 명확한 유지보수 비용
- 불필요한 복잡성
- 기존 계약 파손
- 더 단순하고 동등한 대안
- 데이터/보안/호환성 위험

원안이 적절하면 명확히 승인하고 그 이유를 설명한다.

## 16.5 출력 구조

권장 기본 구조:

1. 판단
2. 근거
3. 필수 우려
4. 추천 개선
5. 선택적 개선
6. 대안 비교
7. 최종 추천
8. 결정이 달라지는 조건

문제가 없을 때 빈 “필수 우려”를 억지로 채우지 않는다.

## 16.6 심각도 분리

```text
Must fix
Recommendation
Optional
```

스타일·취향 수준의 의견을 blocker처럼 쓰지 않는다.

## 16.7 조사 원칙

- 저장소 구조가 판단에 중요하면 먼저 읽는다.
- 최신 외부 사실이 필요하면 현재 provider 기본 검색 기능을 사용할 수 있다.
- 별도 Research/Insane Search 엔진은 이번 구현에 포함하지 않는다.
- 확인하지 못한 것은 추정으로 표시한다.

## 16.8 수정 금지

Advisor는 제품 파일을 직접 구현하지 않는다.

사용자가 구현 요청을 함께 넣더라도 먼저 판단과 추천을 제공하고 역할 경계를 유지한다. 실제 구현은 Standard 또는 Finisher의 책임이다.

이 역시 첫 버전에는 prompt-level 경계다.

---

# 17. Finisher 상세 계획

## 17.1 역할 정의

Finisher는 Hoje Goals의 핵심인 목표 완결 책임을 가져온다.

```text
요청 또는 승인된 계획
  ↓
작업 강도 판단
  ↓
목표 분해
  ↓
구현
  ↓
검증
  ↓
리뷰/QA
  ↓
문제 수정 및 전체 재검증
  ↓
완료 증거
```

## 17.2 입력 우선순위

1. 현재 대화에서 사용자가 승인한 구체적 계획
2. 사용자가 제공한 계획/PRD/brief
3. 현재 사용자 요청

Planner 산출물이 없다고 실행을 거부하지 않는다.

## 17.3 실행 강도

Finisher prompt 내부에서 가장 낮은 안전 강도를 선택한다.

### Light

기준:

- 로컬 저위험 변경
- 대략 2개 이하 파일
- 대략 200 net lines 미만
- cross-layer가 아님

동작:

- 주 에이전트가 직접 구현
- targeted verification
- 자체 review
- 최종 rerun

### Standard

기준:

- 3개 이상 파일
- 200 lines 안팎 이상
- UI/backend/provider 등 cross-layer
- 독립 slice가 있음

동작:

- 명시적 작업 계획
- 필요 시 구현 slice 위임
- 독립 architecture review 또는 QA 관점
- regression 검증
- 최종 전체 rerun

### Strict

기준:

- auth/security
- 결제
- 파괴적 데이터 처리
- migration
- concurrency
- public API 호환성
- production infrastructure
- 사용자가 최대 검증을 명시

동작:

- 비사소한 구현 slice 분리
- 독립 review와 QA/red-team
- adversarial case 확대
- rollback/compatibility 확인
- 완료 전 전체 rerun

## 17.4 작업 분해

모든 작업을 무조건 여러 goal로 나누지 않는다.

분해 기준:

- 독립적으로 구현·검증 가능한 slice
- 서로 다른 layer
- 독립 병렬성이 있는 작업
- review boundary가 다른 작업

같은 acceptance surface를 공유하는 validation-coupled 작업은 한 목표로 유지한다.

## 17.5 구현 규칙

- 수정 전에 관련 코드를 읽는다.
- 가장 단순한 호환 구현을 사용한다.
- 관련 없는 사용자 변경을 보존한다.
- 현재 목표 바깥의 리팩터링을 피한다.
- 실패한 테스트를 무시하지 않는다.
- 변경 범위가 커지면 작업 강도를 승격한다.

## 17.6 서브에이전트 사용

현재 provider가 지원하고 실제 이득이 있을 때만 사용한다.

- Claude: native Agent/Task 활용 가능
- OpenCode: native task/subagent 활용 가능
- Codex: 사용 가능한 현재 하네스 기능에 맞춤

정확한 subagent 이름이나 tool 존재를 prompt에서 강제하지 않는다.

provider가 독립 lane을 지원하지 않으면 같은 에이전트가 순차적으로 implementation → review → QA 관점을 수행한다.

다른 provider CLI를 shell로 강제 호출하지 않는다.

## 17.7 검증 계약

완료 전 최소 다음을 수행한다.

1. 변경 slice별 targeted verification
2. 관련 regression test
3. 사용자-facing surface 확인
4. 실제 artifact 존재 확인
5. 최종 전체 rerun
6. diff 자체 review

Strict에서는 추가:

- adversarial case
- rollback 가능성
- 호환성 경계
- 실패 시나리오
- 보안 경계

## 17.8 Blocker 처리

### Resolvable

- 빌드 오류
- 테스트 실패
- 누락 구현
- 조사 가능한 모호성
- 설치 가능한 dependency

행동:

- 조사
- 수정
- 재검증
- 필요 시 subtask 추가

쉽게 멈추고 사용자에게 되묻지 않는다.

### Human-blocked

- credential/secret
- 외부 승인
- 물리적·수동 작업
- 접근 권한 없음
- 제품 책임자가 선택해야 하는 비가역 결정

행동:

- blocker를 구체적으로 설명
- 이미 완료한 작업과 남은 작업 구분
- 최소한의 사용자 입력만 요청

## 17.9 완료 gate

다음 조건을 모두 만족해야 완료라고 한다.

- 요청 범위 구현
- acceptance criteria 확인
- 필요한 테스트 실행
- review blocker 없음
- QA blocker 없음
- 최종 rerun 결과 확인
- 남은 위험을 명시

검증을 실행할 수 없었다면 완료로 위장하지 않는다.

## 17.10 최종 보고

- 구현 결과
- 변경한 영역
- 실행한 검증과 결과
- 수정 과정에서 발견한 문제
- 남은 위험 또는 미검증 항목
- human blocker가 있으면 정확한 다음 행동

Hoje receipt 형식은 사용하지 않는다.

---

## 18. Prompt 작성 기준

## 18.1 언어

역할 prompt는 provider 간 일관성과 Hoje 원칙 재작성 편의성을 위해 간결한 영어 instruction으로 작성하는 것을 권장한다.

사용자 출력 언어는 기존 DevezVibe 공통 한국어 지침이 담당한다.

## 18.2 중복 금지

역할 prompt에 다음을 반복하지 않는다.

- 한국어 출력 규칙
- 답변 길이 제한
- Vibe 설명
- tool UI 설명
- provider 이름별 세부 구현
- Claude SDK 인증 설명

## 18.3 Prompt budget

각 역할 prompt에 상한을 둔다.

권장:

```text
Planner: 2,000~4,000 tokens 이하
Advisor: 1,500~3,000 tokens 이하
Finisher: 2,500~5,000 tokens 이하
```

Hoje SKILL.md 전체를 복사하면 이 범위를 크게 초과하고 매 턴 비용과 집중력이 나빠진다.

## 18.4 Prompt 정적 검사

단위 테스트에서 다음 문자열이 built-in prompt에 들어가지 않는지 확인한다.

```text
.hoje
ralplan
ultragoal
hoje-code:
ledger.jsonl
goals.json
HOJE_SESSION_ID
```

직접 Hoje runtime을 호출하는 잘못된 지침이 섞이는 것을 방지한다.

## 18.5 Standard 검사

Standard prompt가 빈 문자열이 아니라 `None`인지 확인한다.

---

## 19. 파일별 예상 변경 범위

| 파일 | 구현 내용 |
|---|---|
| `src/agent.rs` | 신규 AgentMode, label/parse/cycle, prompt include, wrapper, 단위 테스트 |
| `prompts/agents/planner.md` | Planner 내장 prompt |
| `prompts/agents/advisor.md` | Advisor 내장 prompt |
| `prompts/agents/finisher.md` | Finisher 내장 prompt |
| `src/main.rs` | `mod agent`, 턴 context에 agent 전달, idle Tab 조건, agent badge click action, Tip 갱신 |
| `src/state.rs` | `agent_mode` 상태, picker, `/agent`, cycle, busy 차단, ComposerMode 값, 테스트 |
| `src/renderer.rs` | Agent badge, Pick::AgentMode, width/pick 테스트 |
| `src/backend.rs` | `combined_turn_instructions()`에 agent 추가, Codex key 보존, provider 회귀 테스트 |
| `README.md` | Agent 사용법과 역할 설명 |
| `npm/README.md` | npm 사용자용 동일 설명 |
| `CLAUDE.md` | dynamic agent prompt 위치를 `src/agent.rs`/`prompts/agents`로 안내 |

원칙적으로 수정하지 않을 파일:

- `src/claude.rs`
- `src/open_code.rs`
- `npm/bridge/claude-agent-sdk-bridge.mjs`
- `npm/package.json`

구현 중 기존 transport로 요구사항을 충족할 수 없다는 사실이 확인될 때만 범위를 재검토한다.

---

## 20. 상세 데이터 흐름

## 20.1 Standard

```text
User input
  ↓
AppState.agent_mode = Standard
  ↓
turn_additional_context(vibe, Standard)
  ↓ no agent key
backend
  ↓
provider 기존 payload
```

## 20.2 Planner/Advisor/Finisher

```text
User input
  ↓
AppState.agent_mode
  ↓
AgentMode::turn_instruction()
  ↓
<devez-vibe-agent mode="...">...</devez-vibe-agent>
  ↓
additionalContext["devez-vibe-agent"]
  ↓
backend provider adapter
  ├─ Codex additionalContext
  ├─ Claude turn context prefix
  └─ OpenCode instruction prefix
```

## 20.3 Provider 전환

```text
Planner + Claude
  ↓ model/provider switch
Planner + Codex
```

`AppState.agent_mode`는 그대로이며 새 provider의 다음 턴에 같은 역할 prompt가 전달된다.

---

## 21. 테스트 계획

## 21.1 `agent.rs` 단위 테스트

1. Default가 Standard
2. cycle 순서
3. parse 대소문자
4. invalid parse 거부
5. Standard instruction이 None
6. 세 prompt가 비어 있지 않음
7. Hoje runtime 금지 문자열 없음
8. prompt 최대 길이 상한
9. wrapper mode 속성 정확

## 21.2 State 테스트

1. `AppState::new()` Standard
2. idle Tab: Standard → Planner
3. 4회 순환 후 Standard
4. composer text 보존
5. busy Tab은 queue 동작 유지
6. busy `/agent`는 차단
7. busy badge click은 차단
8. slash completion Tab 우선
9. question overlay Tab 우선
10. Vibe picker Tab 우선
11. `/btw` split Tab 우선
12. Shift+Tab 기존 동작 유지
13. `/agent` picker open
14. arrows/Tab/numeric 선택
15. Esc original 복원
16. Enter 확정
17. `/agent planner` 직접 설정
18. 잘못된 인자 Usage
19. model/effort/Vibe 값 불변
20. provider switch 후 role 유지

## 21.3 Main/context 테스트

1. Standard context에 agent key 없음
2. Planner context에 planner wrapper
3. Advisor context에 advisor wrapper
4. Finisher context에 finisher wrapper
5. 기존 Vibe context 유지
6. existing Devez rules 유지
7. context의 사용자 표시 누출 없음

## 21.4 Backend 테스트

### Codex

- `prepare_codex_turn_context()` 후 agent key 유지
- standing rules만 제거
- Standard에는 agent key 없음

### Claude

- combined context에 agent 포함
- mode/handoff/reminder와 deterministic order
- session start system prompt는 기존과 동일

### OpenCode

- combined context가 `start_prompt_content()`에 전달됨
- 내부 rules block이 history에서 숨겨짐

## 21.5 Renderer 테스트

1. agent badge 표시
2. 각 role label
3. Pick::AgentMode column
4. 120/80/56 폭
5. branch와 agent 순서
6. cost 표시 유무로 click 위치가 이동하지 않음
7. hover highlight
8. inline/fullscreen 공통

## 21.6 Prompt 행동 수동 테스트

### Standard

- 기존과 유사한 일반 구현
- 불필요한 역할 설명 없음

### Planner clear request

- 저장소 조사
- 질문 없이 계획
- 파일 수정 없음

### Planner ambiguous request

- 저장소 사실 먼저 조사
- 핵심 질문 하나
- 최종 계획 자체 검토

### Advisor sound proposal

- 억지 반론 없이 승인
- 장점과 조건 설명

### Advisor risky proposal

- material risk를 severity별 분리
- 대안과 추천
- 구현하지 않음

### Finisher light

- 직접 구현
- targeted test
- 전체 rerun

### Finisher standard/strict

- 작업 분해
- 가능한 경우 독립 review/QA
- blocker 수정 후 재검증
- 증거 기반 완료 보고

## 21.7 검증 명령

구현 시 최소:

```text
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
node npm/bridge/claude-agent-sdk-bridge.mjs --self-test
node scripts/check-codex-compatibility.mjs
```

실제 사용 smoke test는 Windows x64 패키징 바이너리에서 수행한다.

---

## 22. 구현 단계

## Phase 1 — Agent core

- `src/agent.rs`
- AgentMode enum/API
- prompt 파일 3개
- static tests

완료 조건:

- Standard None
- 세 역할 prompt compile-time 포함
- cycle/parse 테스트 통과

## Phase 2 — State와 명령

- AppState field/default
- `/agent`
- picker
- busy 차단
- idle Tab cycle

완료 조건:

- 기존 queue/split/completion/Shift+Tab 회귀 없음

## Phase 3 — UI

- ComposerMode agent field
- badge
- click Pick
- width tests

완료 조건:

- narrow layout 안정
- 클릭 위치 정확

## Phase 4 — Provider context

- main additionalContext
- backend combined instructions
- Codex key preservation

완료 조건:

- Claude/Codex/OpenCode 모두 역할 지침 수신
- Standard payload baseline 유지

## Phase 5 — Prompt 품질 조정

- Planner behavior test
- Advisor behavior test
- Finisher intensity/completion gate test
- prompt 중복 제거

## Phase 6 — 문서와 release 준비

- README
- npm README
- 도움말/Tip
- version bump와 release는 별도 승인 후

---

## 23. 위험과 완화

## 23.1 Standard 회귀

위험: Agent system 도입만으로 기본 payload가 달라짐.

완화:

- Standard `None`
- agent JSON key 미생성
- baseline unit test

## 23.2 Planner가 수정 수행

위험: prompt-level read-only 경계를 모델이 어길 수 있음.

완화:

- 명확한 no-mutation prompt
- 수동 provider별 테스트
- hard guard는 후속 별도 설계
- UI에서 Planner 설명에 “수정하지 않음” 표시

## 23.3 Advisor의 과도한 반론

위험: 모든 접근에 불필요하게 반대.

완화:

- “do not manufacture objections” 명시
- sound proposal 승인 테스트
- must/recommendation/optional 분리

## 23.4 Finisher 과도한 비용

위험: 작은 작업에도 subagent/review를 남발.

완화:

- lowest safe intensity
- Light 기준
- subagent only when independent value exists

## 23.5 역할 중복

위험: Standard와 Finisher, Planner와 Advisor의 차이가 흐려짐.

완화:

- 역할별 명시적 목적
- Planner는 실행 금지
- Advisor는 선택 평가
- Finisher는 완료 gate

## 23.6 Tab 충돌

위험: queue, split focus, completion, picker가 깨짐.

완화:

- 명시적 우선순위
- 기존 동작별 회귀 테스트
- idle fallback에서만 cycle

## 23.7 Prompt가 history에 노출

위험: 사용자 대화에 내부 instruction 표시.

완화:

- 기존 Claude stripHandoff 경로
- OpenCode internal rules filtering
- Codex application context
- resume/history smoke test

## 23.8 좁은 UI 깨짐

완화:

- 접두어 없는 짧은 label
- 낮은 우선순위 badge부터 숨김
- width별 renderer 테스트

---

## 24. 호환성과 Migration

기존 config/schema migration은 없다.

- 기존 사용자는 Standard로 시작
- 기존 세션 resume 가능
- route store 형식 변경 없음
- Claude bridge 프로토콜 변경 없음
- OpenCode ACP protocol 변경 없음
- Codex thread data 변경 없음
- 기존 Vibe settings 파일 변경 없음

기능 제거 시 `agent_mode`와 관련 UI/context 코드만 제거하면 기존 구조로 돌아갈 수 있다.

---

## 25. 완료 승인 기준

### 기능

- 네 역할 선택 가능
- Standard 기본값
- idle Tab 순환
- `/agent`와 badge picker
- provider 전환 후 역할 유지

### 행동

- Standard 기존 동작 보존
- Planner 조사·명확화·계획·자체 검토, 구현 없음
- Advisor 근거 기반 추천·반론, 구현 없음
- Finisher 구현·검증·리뷰·완료 증거

### 안정성

- busy Tab queue 유지
- `/btw` Tab 유지
- slash completion 유지
- Shift+Tab 유지
- history에 prompt 미노출
- 좁은 터미널 안정

### 품질

- Rust tests 통과
- clippy/fmt 통과
- Claude bridge self-test 통과
- 세 provider smoke test 완료

---

## 26. 후속 확장 후보

첫 버전 안정화 후에만 검토한다.

1. Agent별 hard tool policy
2. session별 agent persistence
3. 사용자 지정 agent prompt
4. Automatic router
5. Research/Insane Search tool
6. Finisher execution intensity UI
7. Agent별 모델 추천
8. Agent pipeline

Automatic은 네 수동 역할의 실제 사용 데이터를 확인한 뒤 추가하는 것이 안전하다.

---

## 27. 구현 결정 요약

| 항목 | 결정 |
|---|---|
| 기본 역할 | Standard |
| 역할 수 | 4 |
| 전환 | 수동 |
| Tab | idle fallback에서만 cycle |
| Busy Tab | 기존 queue 유지 |
| Split Tab | 기존 pane 전환 유지 |
| persistence | 프로세스 수명만 |
| Standard prompt | 없음 |
| Prompt 저장 | markdown + `include_str!` |
| Planner | Ask + Plan 통합 |
| Advisor | Architect + Critic 기반 신규 역할 |
| Finisher | Goals 핵심 철학, runtime 제거 |
| Provider native agent | 사용하지 않음 |
| Hard read-only | 후속 범위 |
| Hoje plugin 설치 | 불필요 |
| Research | 제외 |
| Automatic | 제외 |

---

## 28. 1차 초안 자체 점검 항목

다음 재검수에서 반드시 다시 확인한다.

- 현재 main의 Tab 우선순위를 정확히 반영했는가
- busy queued prompt가 Agent 변경으로 오염되지 않는가
- `/btw` 두 AppState의 역할 범위가 명확한가
- Standard가 정말 payload 무변경인가
- Claude/OpenCode history에서 역할 지침이 숨겨지는가
- Planner/Advisor read-only가 보안 경계가 아님을 명확히 썼는가
- Finisher가 작은 작업에 과도한 orchestration을 만들지 않는가
- Hoje runtime 용어가 최종 prompt에 남지 않도록 검사하는가
- 구현 파일 범위에서 불필요한 bridge 변경을 피했는가
- UI 좁은 폭과 click mapping 테스트가 포함됐는가
- 역할별 acceptance criteria가 서로 중복되지 않는가
