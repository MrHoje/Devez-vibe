# Devez Vibe 4-Agent 시스템 상세 구현 계획서

> 상태: **1차 초안 재검수 완료 / 구현 전 설계 기준안**  
> 작성일: 2026-08-30  
> 대상 저장소: `MrHoje/Devez-vibe`  
> 참고 구현: `MrHoje/devez-marketplace/plugins/hoje-code`  
> 대상 Agent: `Standard`, `Planner`, `Advisor`, `Finisher`  
> 제외 범위: Automatic routing, Research Agent, Hoje Research, Insane Search, `.hoje` 런타임 이식  
> 이 커밋의 범위: **문서만 추가하며 제품 코드는 수정하지 않는다.**

---

## 0. 문서 목적과 최종 결론

이 문서는 Devez Vibe에 네 가지 수동 선택형 Agent를 추가하기 위한 구현 기준을 정의한다.

```text
Standard  → 현재 Devez Vibe의 범용 기본 동작
Planner   → 요구 명확화 + 저장소 조사 + 설계 + 계획 검증
Advisor   → 사용자 제안 평가 + 추천 + 반론 + 대안·트레이드오프
Finisher  → 목표 분해 + 구현 + 검증 + 독립 검토 + 완료 판정
```

최종 설계 결론은 다음과 같다.

1. Agent는 provider가 아니라 **Devez Vibe의 공통 UI·상태 계층이 소유**한다.
2. Agent 선택은 `Standard`가 기본값인 **수동 전환 방식**으로 구현한다.
3. `Standard`는 별도 역할 지침을 넣지 않아 현재 기본 하네스 동작을 보존한다.
4. `Planner`, `Advisor`, `Finisher`만 매 턴 동적 application context로 역할 지침을 전달한다.
5. Claude, Codex, OpenCode 모두 같은 Agent 의미를 사용하며 OpenCode의 native agent mode에는 의존하지 않는다.
6. Agent 선택은 턴 시작 시 스냅샷으로 고정하고, 실행 중에는 바꾸지 않는다.
7. Planner와 Advisor는 제품 소스를 수정하지 않는 역할로 정의한다. 다만 이 제약은 v1에서 prompt contract이며 보안 sandbox는 아니다.
8. Finisher는 Hoje Goals의 핵심 품질 원칙을 사용하지만 `.hoje`, ledger, receipt, 별도 CLI 상태 머신은 가져오지 않는다.
9. Hoje Ask와 Hoje Plan은 별도 Agent로 나누지 않고 Planner에 합친다.
10. Automatic과 Research는 v1 범위에서 명시적으로 제외한다.

---

## 1. 목표

### 1.1 사용자 목표

사용자는 모델이나 provider를 바꾸지 않고도 대화의 작업 성향을 즉시 바꿀 수 있어야 한다.

```text
일반적인 질문·수정             → Standard
요구 정리·구현 계획            → Planner
설계 선택 검토·추천·반론       → Advisor
큰 작업을 구현·검증까지 완결   → Finisher
```

Agent 선택 후에도 다음 상태는 독립적으로 유지되어야 한다.

- 선택된 model
- reasoning effort
- provider
- Claude permission mode
- Vibe mode
- Response 길이와 표시 방식
- Shell/Diff 표시 방식
- 세션 history
- 현재 작업 폴더

### 1.2 제품 목표

- 최신 Claude Code/Codex/OpenCode 기본 하네스의 장점을 유지한다.
- 역할이 실제 행동을 바꾸는 경우에만 Agent를 구분한다.
- provider별 별도 Agent 구현을 만들지 않는다.
- 역할 전환 때문에 세션을 재시작하거나 대화 history를 복제하지 않는다.
- Agent 선택이 기존 입력·렌더링·세션 전환 동작을 깨뜨리지 않게 한다.
- 기본값 `Standard`에서 기존 사용자 경험이 실질적으로 달라지지 않게 한다.

### 1.3 품질 목표

- 역할별 목적, 금지 행동, 완료 조건이 겹치지 않아야 한다.
- 실제 provider가 제공하지 않는 도구나 subagent를 사용했다고 가장하지 않아야 한다.
- Planner와 Advisor가 불필요하게 질문하거나 반론하지 않아야 한다.
- Finisher가 검증 없이 완료를 선언하지 않아야 한다.
- Agent 지침이 사용자 history에 일반 사용자 메시지처럼 노출되지 않아야 한다.
- Agent 전환 상태와 실제 턴에 적용된 역할이 화면에서 불일치하지 않아야 한다.

---

## 2. 범위

### 2.1 v1 포함 범위

- `Standard`, `Planner`, `Advisor`, `Finisher` 네 Agent
- 기본값 `Standard`
- `/agent` 선택 picker
- `/agent standard|planner|advisor|finisher` 직접 선택
- idle 상태에서 bare `Tab` 순환
- composer 상단 Agent badge
- badge click으로 Agent picker 열기
- Claude/Codex/OpenCode 공통 역할 지침 주입
- 새 턴마다 선택 Agent를 스냅샷
- `/new`, `/resume`, provider 전환, `/btw`와의 상태 규칙
- 역할별 prompt contract
- provider 능력 차이에 대한 degradation 규칙
- unit/integration/regression test
- README와 npm README 사용법 반영

### 2.2 v1 제외 범위

다음 항목은 구현하지 않는다.

- Automatic Agent/router
- 요청 분류용 추가 LLM 호출
- Research Agent
- Hoje Research
- Insane Search
- 별도 web search provider
- `.hoje` 상태 디렉터리
- `hoje` CLI
- goals ledger와 hash chain
- receipt, `stage_n`, SHA-256 workflow 증명
- Planner 결과의 자동 Finisher handoff
- Agent별 model 자동 변경
- Agent별 effort 자동 변경
- Agent별 permission mode 자동 변경
- OpenCode `session/set_mode` 기반 native agent 전환
- Agent mode의 사용자 전역 영구 저장
- prompt를 원격에서 다운로드하는 기능
- 사용자 정의 Agent 편집 UI

### 2.3 향후 확장 가능하지만 v1에 넣지 않는 항목

- Automatic router
- Agent별 기본 model/effort profile
- Agent mode persistence 옵션
- Planner 계획을 Finisher가 구조화해 직접 읽는 artifact
- hard read-only tool guard
- 별도 Researcher subagent
- `/agent finisher --strict` 같은 명시적 intensity flag
- Agent prompt marketplace

---

## 3. 용어

| 용어 | 의미 |
|---|---|
| Agent mode | 사용자가 선택한 `Standard`, `Planner`, `Advisor`, `Finisher` 역할 |
| selected agent | UI에서 현재 선택된 역할 |
| active turn agent | 실행 중인 턴이 시작될 때 확정된 역할 |
| common instructions | 현재 `DEVEZ_INSTRUCTIONS`, `CLAUDE_DEVEZ_INSTRUCTIONS`처럼 모든 역할에 적용되는 Devez Vibe 지침 |
| role instructions | Planner, Advisor, Finisher에만 추가되는 동적 역할 지침 |
| provider adapter | Claude/Codex/OpenCode로 요청을 전달하는 기존 backend 계층 |
| capability degradation | provider가 subagent·task·질문 도구 등을 지원하지 않을 때 더 단순한 수행 방식으로 낮추는 것 |
| product source | 사용자의 실제 저장소 코드·설정·문서 중 작업 대상 파일 |
| diagnostic command | 파일을 의도적으로 수정하지 않고 사실 확인을 위해 실행하는 명령 |
| mutation | 제품 소스, 프로젝트 설정, 의존성, Git 상태 등을 변경하는 작업 |

---

## 4. 현재 Devez Vibe 구조에서 확인된 사실

### 4.1 공통 UI 상태

Devez Vibe는 `AppState`에서 composer, model, effort, Vibe, 표시 모드, history, picker와 실행 상태를 관리한다. Agent도 같은 계층이 소유해야 한다.

```text
AppState
├─ composer/editor
├─ selected model / effort
├─ Vibe / Response / Shell / Diff
├─ overlay picker
├─ turn state / busy state
├─ plan / subagent view
└─ 새 Agent mode 상태
```

provider가 Agent 상태를 소유하면 Claude → Codex → OpenCode 전환 시 의미가 달라질 수 있으므로 사용하지 않는다.

### 4.2 턴 요청 조립 위치

일반 턴과 `/btw` 턴은 `src/main.rs`에서 `additionalContext`를 만들고 backend에 전달한다.

현재 공통 context에는 다음 종류가 있다.

```text
devez-vibe-rules
claude-devez-vibe-rules
claude-devez-vibe-reminder
devez-vibe-mode
provider-handoff
```

Agent 구현은 여기에 조건부 `devez-vibe-agent` context를 추가하는 방식이 적합하다.

### 4.3 provider 분기 위치

`src/backend.rs`는 한 턴을 다음 경로로 분배한다.

```text
Claude   → session/prompt 또는 session/steer
Codex    → turn/start 또는 turn/steer
OpenCode → start_prompt_content
```

따라서 Agent 기능은 provider bridge를 각각 개조하기보다 backend가 전달할 공통 context를 확장하는 방식이 단순하다.

### 4.4 Claude 지침 계층

Claude는 세션 생성 시 `CLAUDE_DEVEZ_INSTRUCTIONS`를 Claude Code preset system prompt 뒤에 append한다. 턴별 context는 bridge의 handoff wrapper를 통해 사용자 입력 앞에 붙는다.

이 계층 차이 때문에 다음 사실이 중요하다.

> 턴별 Planner 지침은 Claude system prompt의 공통 200자 제한을 단독으로 무효화할 수 없다.

따라서 공통 system prompt 자체에 “Devez Vibe가 제공한 역할 contract가 상세 산출물을 요구하는 경우”에 대한 좁은 예외를 추가해야 한다.

### 4.5 Codex 지침 계층

Codex는 thread 시작·resume 때 `developerInstructions`를 받고, 각 턴에서 `additionalContext`를 받을 수 있다. 기존 `prepare_codex_turn_context`는 standing rule 복사본과 Claude 전용 항목을 제거한다.

`devez-vibe-agent`는 동적 턴 지침이므로 제거하지 않아야 한다.

### 4.6 OpenCode 지침 계층

OpenCode는 Devez Vibe가 전달한 turn context를 prompt content에 포함할 수 있다. v1에서는 OpenCode native agent mode를 사용하지 않고 Devez Vibe 역할 contract를 그대로 전달한다.

### 4.7 `/btw`의 Tab 선점

현재 `/btw` split view가 열려 있을 때 bare `Tab`은 main pane과 Btw pane의 focus를 전환한다.

따라서 Agent cycle은 다음 우선순위를 침범하면 안 된다.

```text
1. 질문·picker·completion 입력
2. /btw split focus 전환
3. 기존 composer 입력 의미
4. 조건을 모두 만족할 때만 Agent cycle
```

### 4.8 기존 공통 지침의 강한 분량 제한

현재 공통 지침은 일반 최종 응답을 약 200자로 제한한다. 이는 Standard의 평상시 응답에는 적합하지만 Planner의 상세 계획과 Advisor의 대안 비교에는 부족할 수 있다.

v1은 공통 규칙을 제거하지 않고 다음 조건부 예외를 추가한다.

```text
Planner → 계획 artifact는 필요한 상세도를 허용
Advisor → 비교·추천 근거가 필요한 경우 상세도를 허용
Finisher → 완료 증거·blocker·사용자 선택이 필요한 경우에만 상세도를 허용
Standard → 기존 제한 유지
```

---

## 5. Hoje-code에서 가져올 것과 가져오지 않을 것

### 5.1 참고 대상

| Devez Vibe Agent | Hoje-code 참고 요소 |
|---|---|
| Standard | 별도 Hoje 역할 없음. 현재 provider 기본 harness 유지 |
| Planner | Hoje Ask + Hoje Plan + planner + architect + critic |
| Advisor | architect + critic의 검토 기준을 자문용으로 재구성 |
| Finisher | Hoje Goals + executor + executor-qa + architect |

### 5.2 Planner에 가져올 원칙

Hoje planner에서 가져온다.

- 저장소를 먼저 조사한다.
- 계획 범위를 제한한다.
- 영향 경로, 계약, 위험, 검증 방법을 포함한다.
- 사실과 가정을 구분한다.
- 실행하지 않은 확인을 실행했다고 말하지 않는다.
- 제품 파일을 수정하지 않는다.

Hoje Ask에서 가져온다.

- 명확한 요청에는 인터뷰를 강요하지 않는다.
- 저장소에서 알 수 있는 것은 사용자에게 묻지 않는다.
- 중요한 모호성만 질문한다.
- 한 번에 가장 가치가 높은 질문 하나를 우선한다.
- 숨은 가정, 범위, acceptance criteria, non-goal을 확인한다.
- 요구가 충분히 명확해진 뒤 계획을 만든다.

Hoje Architect에서 가져온다.

- 아키텍처 경계
- 제품 동작 계약
- 호환성
- 보안 경계
- 검증 증거

Hoje Critic에서 가져온다.

- 누락된 surface
- 잘못된 순서
- 숨은 의존성
- 약한 acceptance criteria
- 테스트가 통과해도 실제 동작이 깨질 수 있는 경우

### 5.3 Advisor에 가져올 원칙

Hoje Architect와 Critic의 “독립 검토” 원칙을 가져오되 계획 합의 workflow는 가져오지 않는다.

Advisor는 다음 질문에 답하는 역할이다.

```text
사용자가 제안한 방법이 실제로 적절한가?
더 단순하거나 안전한 대안이 있는가?
지금의 장점과 미래 비용은 무엇인가?
반드시 바꿔야 하는 문제와 선택 개선은 무엇인가?
```

### 5.4 Finisher에 가져올 원칙

Hoje Goals의 다음 개념을 가져온다.

- light / standard / strict 실행 강도
- bounded goal
- 관련 없는 사용자 변경 보존
- 구현 전 조사
- targeted verification
- 실제 사용자 surface 검증
- 독립 QA 또는 가능한 가장 가까운 대체 검토
- regression/adversarial check
- blocker를 조언으로 약화하지 않기
- 증거 없이 완료 선언하지 않기

### 5.5 가져오지 않을 Hoje 요소

다음은 plugin runtime과 durability를 위한 요소이므로 v1에 이식하지 않는다.

- `.hoje/_session-*`
- ambiguity score 저장
- threshold 설정 해석
- staged draft와 two-phase state write
- RALPLAN writer
- `stage_n`
- receipt path와 SHA-256
- conflict disposition JSON
- goals ledger
- hash chain
- checkpoint CLI
- pause critic gate
- 자동 handoff
- plugin namespace
- 별도 Node runtime
- research mission/experiment/verdict ledger

### 5.6 참고 방식

Hoje prompt를 그대로 복사하는 것이 아니라 역할 경계와 검증 원칙을 Devez Vibe의 공통 provider 구조에 맞게 다시 작성한다.

원문을 상당 부분 그대로 복사하게 되면 해당 라이선스와 attribution을 확인해 고지를 유지한다. 가능하면 행동 계약만 재서술한다.

---

## 6. 핵심 설계 원칙

### 6.1 Standard-first

기능 추가 후에도 첫 실행은 `Standard`다. 사용자가 Agent 기능을 모르더라도 기존 Devez Vibe처럼 사용할 수 있어야 한다.

### 6.2 역할과 model 분리

```text
Agent  = 행동 계약
Model  = 추론·생성 엔진
Effort = 추론 강도
```

다음 조합을 모두 허용한다.

```text
Planner + Sonnet
Planner + GPT-5.6 Sol
Advisor + Opus
Finisher + Codex
```

Agent 선택이 model이나 effort를 임의로 바꾸면 안 된다.

### 6.3 provider-neutral semantics

같은 Agent 이름은 provider와 관계없이 같은 의미를 가져야 한다.

```text
Claude Planner   == Codex Planner   == OpenCode Planner
Claude Advisor   == Codex Advisor   == OpenCode Advisor
Claude Finisher  == Codex Finisher  == OpenCode Finisher
```

도구 표현은 다를 수 있지만 역할 계약은 같아야 한다.

### 6.4 per-turn snapshot

Agent는 턴 시작 순간 확정한다.

```text
selected_agent = Planner
사용자 submit
active_turn_agent = Planner
turn/start
```

턴 실행 중 `selected_agent`를 바꾸지 않는다. `turn/steer`는 같은 active turn agent를 유지한다.

### 6.5 명시적 수동 전환

v1은 사용자의 선택만 따른다.

- Agent가 스스로 다른 primary Agent로 전환하지 않는다.
- Planner가 자동으로 Finisher를 시작하지 않는다.
- Advisor가 자동으로 Planner를 시작하지 않는다.
- Finisher가 자동으로 mode를 Standard로 되돌리지 않는다.

### 6.6 최소 추가 추상화

새 모듈은 Agent 정의와 prompt 조립에 집중한다. 별도 workflow engine이나 DAG runtime은 만들지 않는다.

### 6.7 truthful degradation

provider가 독립 subagent를 지원하지 않으면 같은 모델의 self-review로 낮춘다. 이 경우 “독립 검토”라고 보고하지 않는다.

### 6.8 role instruction은 보안 경계가 아님

Planner/Advisor의 no-edit 규칙은 v1에서 행동 지침이다. 악의적 prompt나 provider 오류까지 막는 hard sandbox가 아니다.

hard read-only가 필요하면 후속 버전에서 tool approval/dispatch 계층에 mutation guard를 추가한다.

---

## 7. 시스템 아키텍처

### 7.1 목표 구조

```text
┌──────────────────────────────────────────────┐
│ Devez Vibe UI / AppState                     │
│                                              │
│ selected_agent: Standard|Planner|Advisor|... │
│ active_turn_agent: Option<AgentMode>         │
└──────────────────────┬───────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────┐
│ Agent contract layer                         │
│                                              │
│ Standard → no role context                   │
│ Planner  → planner contract                  │
│ Advisor  → advisor contract                  │
│ Finisher → finisher contract                 │
└──────────────────────┬───────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────┐
│ turn_additional_context                      │
│                                              │
│ common rules + vibe + reminder + role        │
└──────────────────────┬───────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────┐
│ Backend provider routing                     │
├────────────────┬────────────────┬────────────┤
│ Claude         │ Codex          │ OpenCode   │
│ handoffContext │ additionalCtx  │ prompt ctx │
└────────────────┴────────────────┴────────────┘
```

### 7.2 새 모듈

권장 파일:

```text
src/agent.rs
prompts/agents/planner.md
prompts/agents/advisor.md
prompts/agents/finisher.md
```

`include_str!`로 compile-time embedding하면 npm 패키지에 prompt 파일을 별도로 배포할 필요가 없다.

### 7.3 `AgentMode`

권장 형태:

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

필요 메서드:

```rust
impl AgentMode {
    pub const fn label(self) -> &'static str;
    pub const fn short_label(self) -> &'static str;
    pub const fn config_value(self) -> &'static str;
    pub const fn next(self) -> Self;
    pub const fn description(self) -> &'static str;
    pub const fn output_policy(self) -> AgentOutputPolicy;
    pub const fn mutation_policy(self) -> AgentMutationPolicy;
    pub fn instructions(self) -> Option<&'static str>;
}
```

`Standard::instructions()`는 반드시 `None`을 반환한다.

### 7.4 역할 contract 구조

역할 prompt는 단순 persona 소개가 아니라 다음 항목을 명시해야 한다.

```text
identity
purpose
use-when
do-not-do
investigation policy
question policy
mutation policy
verification policy
subagent/degradation policy
output contract
completion criteria
```

### 7.5 Agent context 형식

권장 application context:

```json
{
  "devez-vibe-agent": {
    "value": "<devez-vibe-agent mode=\"planner\" ...>...</devez-vibe-agent>",
    "kind": "application"
  }
}
```

역할 contract 예시 메타데이터:

```text
mode=planner
mutation=forbidden
output=expanded-artifact
handoff=manual
```

이 메타데이터는 보안용 서명이 아니라 공통 prompt와 역할 prompt 사이의 명확한 계약 표지다.

### 7.6 공통 지침과 역할 지침 조합

최종 개념:

```text
common Devez rules
+ provider-specific common rules
+ current Vibe notice
+ provider handoff history
+ Claude reminder when applicable
+ current role contract
```

역할 contract를 같은 턴 context의 마지막에 두어 role-specific output contract가 가장 가깝게 읽히게 한다.

단 Claude system prompt가 더 높은 우선순위이므로 공통 system prompt에도 역할 예외를 정의해야 한다.

---

## 8. 공통 prompt 변경 원칙

### 8.1 Standard의 의미

초안에서는 “Standard의 prompt payload를 byte 단위로 바꾸지 않는다”는 목표를 둘 수 있지만 Claude의 system prompt 계층 때문에 Planner 상세 출력 예외를 구현하려면 공통 prompt에 좁은 조건문을 추가해야 한다.

따라서 최종 기준을 다음과 같이 수정한다.

```text
Standard = 역할 context가 없고, 실제 행동이 기존과 동일해야 한다.
```

byte-identical prompt는 acceptance criteria로 사용하지 않는다.

### 8.2 공통 분량 예외

`DEVEZ_INSTRUCTIONS`와 `CLAUDE_DEVEZ_INSTRUCTIONS`에 다음 의미를 추가한다.

```text
Devez Vibe가 application context로 제공한 역할 contract가
`output=expanded-artifact`를 명시하면, 해당 역할의 핵심 산출물에는
일반 200자 제한을 적용하지 않는다.

이 예외는 다음에만 사용한다.
- Planner의 구현 계획
- Advisor의 비교·권고 분석
- 사용자 선택에 필요한 전체 대안
- Finisher의 blocker·검증 증거가 생략되면 판단이 왜곡되는 경우

일반 진행 안내와 단순 완료 보고는 계속 짧게 쓴다.
```

### 8.3 공통 규칙과 역할 규칙의 충돌 처리

| 충돌 | 최종 규칙 |
|---|---|
| 일반 200자 제한 vs Planner 계획 | Planner 계획 artifact가 우선 |
| 짧은 답변 vs Advisor 대안 비교 | 판단에 필요한 범위만 확장 |
| Finisher 상세 과정 vs 간결한 완료 보고 | 과정은 plan/tool UI, 최종은 증거 중심으로 압축 |
| 공통 Task 규칙 vs Planner read-only | Task는 조사·계획 검토용이며 mutation은 금지 |
| 공통 질문 규칙 vs Planner clarification | 지원 시 AskUserQuestion, 미지원 시 일반 text 질문 |
| 사용자 요청 vs 역할 금지 | Planner/Advisor에서는 구현하지 않고 역할 경계를 알림 |

### 8.4 사용자 위조 가능성

사용자가 role tag를 직접 입력할 수 있으므로 role tag는 authorization boundary로 취급하지 않는다.

그 결과 바뀔 수 있는 것은 출력 길이·작업 성향뿐이어야 한다. 파일 권한 상승이나 보안 우회 조건으로 사용하면 안 된다.

---

## 9. 상태 모델

### 9.1 `AppState` 필드

권장 필드:

```rust
selected_agent: AgentMode,
active_turn_agent: Option<AgentMode>,
```

선택적으로 picker draft가 필요하면 overlay 내부 상태로만 둔다.

### 9.2 필드 의미

| 필드 | 의미 |
|---|---|
| `selected_agent` | 다음 새 턴에 적용할 역할 |
| `active_turn_agent` | 현재 실행 중인 턴에 이미 적용된 역할 |

idle에서는 다음이 성립한다.

```text
active_turn_agent = None
```

turn 시작 후:

```text
active_turn_agent = Some(selected_agent)
```

turn 종료·실패·interrupt 후:

```text
active_turn_agent = None
```

### 9.3 실행 중 전환 금지

busy 상태에서 Agent 변경 요청이 들어오면 다음과 같이 처리한다.

```text
현재 턴이 끝난 뒤 Agent를 변경할 수 있습니다.
```

v1에서는 다음 턴 예약 전환을 만들지 않는다.

이유:

- UI와 active turn 역할의 불일치 방지
- queued prompt가 어느 역할을 소유하는지 복잡해지는 문제 방지
- `turn/steer`에 새 역할이 잘못 적용되는 문제 방지
- split pane과 parent pane의 상태 혼동 방지

### 9.4 초기값과 reset

| 이벤트 | Agent 결과 |
|---|---|
| 프로그램 시작 | Standard |
| 첫 새 세션 | Standard |
| `/new` | Standard로 reset |
| `/resume` 성공 | Standard로 reset |
| `/resume` 실패 | 기존 선택 유지 |
| model 변경 | 유지 |
| effort 변경 | 유지 |
| provider 변경 | 유지 |
| Vibe 변경 | 유지 |
| 같은 세션의 다음 턴 | 유지 |

특수 Agent가 며칠 뒤 resume에서 의도치 않게 살아나는 것을 막기 위해 v1에서는 session history에 Agent를 영구 저장하지 않는다.

### 9.5 provider 전환

동일 UI conversation에서 provider만 바꾸는 경우 `selected_agent`를 유지한다.

```text
Planner + Claude
→ provider switch
Planner + Codex
```

새 provider의 다음 턴에도 동일 역할 contract를 전달한다.

### 9.6 `/btw`

Btw state 생성 시 현재 pane의 `selected_agent`를 복사한다.

```text
main.selected_agent = Advisor
forked_side_state → btw.selected_agent = Advisor
```

생성 후에는 두 `AppState`가 독립적으로 Agent를 가진다.

```text
main = Advisor
btw  = Planner
```

bare `Tab`은 split focus 전환에 계속 사용하므로 split 상태에서는 Agent cycle을 실행하지 않는다. focused pane의 역할 변경은 `/agent` 또는 Agent badge click으로 한다.

---

## 10. Agent 선택 UX

### 10.1 picker

`/agent`는 다음 picker를 연다.

| 선택 | 설명 |
|---|---|
| Standard | 기존 Devez Vibe 기본 동작과 일반 작업 |
| Planner | 요구사항을 정리하고 저장소를 조사해 구현 계획 작성 |
| Advisor | 접근법을 평가하고 추천·반론·대안 비교 |
| Finisher | 목표를 분해하고 구현·검증·검토까지 완결 |

### 10.2 직접 명령

```text
/agent standard
/agent planner
/agent advisor
/agent finisher
```

허용 가능한 짧은 alias:

```text
/agent std
/agent plan
/agent adv
/agent finish
```

모호한 부분 일치로 Agent를 바꾸지 않는다. exact alias만 허용한다.

### 10.3 bare Tab 순환

순서:

```text
Standard → Planner → Advisor → Finisher → Standard
```

Agent cycle 조건을 모두 만족해야 한다.

- key modifier 없음
- press 또는 repeat
- Btw split 없음
- overlay 없음
- 질문 입력 없음
- completion 후보 없음
- turn 실행 중 아님
- session start/resume 대기 중 아님
- compaction 중 아님
- composer 본문이 비어 있음

조건이 하나라도 맞지 않으면 기존 Tab 의미를 유지한다.

### 10.4 Shift+Tab

Shift+Tab의 기존 permission 관련 의미는 변경하지 않는다.

### 10.5 badge

composer 상단 mode 영역에 현재 Agent를 표시한다.

```text
Standard
Planner
Advisor
Finisher
```

Agent는 사용자 입력의 행동 모드이므로 context/usage 중심 status line보다 composer mode badge가 적합하다.

### 10.6 좁은 화면

권장 축약:

```text
Std
Plan
Adv
Finish
```

축약은 renderer가 공간 부족 시에만 사용한다. picker와 help에는 항상 전체 이름을 보여준다.

Agent badge의 우선순위는 다음과 같이 둔다.

```text
model / agent / 핵심 permission
> branch
> shell/diff 보조 표시
```

실제 우선순위는 현재 composer layout과 함께 렌더링 회귀 테스트로 확정한다.

### 10.7 전환 notice

Agent를 바꿀 때 transcript block을 추가하지 않고 기존 짧은 composer/status notice를 사용한다.

```text
Agent · Planner
```

이 notice는 대화 history나 provider handoff에 포함하지 않는다.

---

## 11. 턴 lifecycle

### 11.1 새 턴

```text
1. 사용자가 prompt 입력
2. submit 직전 selected_agent 확인
3. active_turn_agent로 snapshot
4. role context 조립
5. turn/start 요청
6. UI는 active_turn_agent 표시
7. 완료/실패/interrupt에서 snapshot 해제
```

### 11.2 queued prompt

현재 턴 실행 중에는 Agent 변경을 막으므로 queued prompt는 같은 `selected_agent`를 사용한다.

향후 busy 상태 전환 예약을 지원한다면 queued prompt마다 Agent snapshot을 저장해야 하지만 v1에는 넣지 않는다.

### 11.3 steer

`turn/steer`는 이미 실행 중인 턴에 추가 입력을 보내는 기능이다.

- 새 role context를 넣지 않는다.
- `active_turn_agent`를 유지한다.
- selected Agent를 바꿀 수 없다.

### 11.4 interrupt

interrupt 후:

- active turn agent 해제
- selected agent 유지
- 다음 prompt는 같은 selected agent 사용

### 11.5 request 실패

`turn/start`가 실패하면:

- active turn agent 해제
- selected agent 유지
- 실패 notice에 Agent 때문에 실패했다고 단정하지 않음
- provider 원본 오류 유지

---

## 12. provider별 전달 계획

### 12.1 공통 context 생성

기존 함수 개념을 다음과 같이 확장한다.

```rust
fn turn_additional_context(vibe: VibeMode, agent: AgentMode) -> Value
```

Standard:

```text
devez-vibe-agent key 없음
```

나머지:

```text
devez-vibe-agent key 있음
```

### 12.2 Claude

Claude 세션 system prompt:

```text
Claude Code preset
+ CLAUDE_DEVEZ_INSTRUCTIONS
```

턴 input:

```text
provider handoff wrapper
├─ Vibe notice
├─ 이전 provider history 요약
├─ Claude turn reminder
└─ Agent role contract
```

필수 변경:

- `combined_turn_instructions`에 Agent context 추가
- Agent context를 가장 뒤에 배치
- common system prompt에 role output exception 추가
- 기존 handoff wrapper가 history에서 제거되는지 테스트

Claude 세션을 Agent 전환 때 재시작하지 않는다.

### 12.3 Codex

thread 시작·resume:

```text
developerInstructions = common rules
```

각 turn:

```text
additionalContext.devez-vibe-agent = role contract
```

필수 변경:

- `prepare_codex_turn_context`가 role key를 제거하지 않게 유지
- Standard에는 key 자체를 넣지 않음
- role contract가 turn 단위로 바뀌는지 test fixture 확인

### 12.4 OpenCode

OpenCode에는 native primary agent 전환을 호출하지 않는다.

```text
Devez role contract
→ existing start_prompt_content turn context
→ 동일 prompt 의미
```

이유:

- Claude/Codex와 동작을 맞추기 쉬움
- OpenCode 설정 파일이나 설치 Agent에 의존하지 않음
- project별 Agent 이름 충돌 없음
- native mode가 model/tool permission까지 바꿔 provider 중립성을 깨는 문제 방지

### 12.5 capability 차이

role prompt는 특정 provider tool 이름을 필수로 요구하지 않는다.

```text
가능하면 native task/plan 사용
가능하면 subagent 사용
불가능하면 같은 역할의 순차 검토
도구가 없으면 명시적 checklist
```

실제 사용하지 않은 tool이나 subagent를 최종 답변에서 사용했다고 표현하지 않는다.

---

## 13. history·resume·handoff 계획

### 13.1 role context 비노출

Agent contract는 대화 내용이 아니라 Devez Vibe application instruction이다.

다음 화면에는 나타나면 안 된다.

- 사용자 메시지 bubble
- resume preview
- provider handoff transcript
- copied prompt text
- session title

### 13.2 Claude history

Claude bridge의 length-prefixed handoff wrapper에 role context를 함께 넣고 기존 `stripHandoff`가 전체 internal prefix를 제거하도록 유지한다.

회귀 테스트:

```text
role contract 포함 turn 실행
→ transcript 재조회
→ user text에는 원래 prompt만 존재
```

### 13.3 Codex history

Codex additional application context가 user message로 저장되지 않는 현재 동작을 검증한다. 저장된다면 history normalization에서 Devez application context를 숨겨야 한다.

### 13.4 OpenCode history

OpenCode prompt wrapper가 history replay에서 일반 사용자 text로 복원되지 않는지 확인한다. 문제가 있으면 exact internal tag만 제거하고 사용자 입력은 보존한다.

### 13.5 provider handoff

provider handoff snapshot에 selected Agent를 저장하지 않는다.

이유:

- Agent는 UI state에서 이미 유지됨
- history와 role state 결합 방지
- resume 시 Standard reset 규칙 유지
- 이전 specialized role이 새 provider에서 자동 부활하는 문제 방지

동일 세션 provider 전환 후 다음 턴에는 현재 UI selected Agent를 새로 주입한다.

---

# 14. Standard Agent

## 14.1 목적

현재 Devez Vibe의 일반 사용 모드다.

```text
질문
조사
간단한 코드 수정
일반적인 구현
테스트
리뷰
설명
```

을 provider 기본 하네스 판단에 맡긴다.

## 14.2 핵심 계약

- 별도 role prompt를 주입하지 않는다.
- 기존 common instructions를 사용한다.
- 기존 provider tools·skills·project instructions를 그대로 사용한다.
- subagent 사용 여부는 provider 기본 판단에 맡긴다.
- 기존 model/effort/permission 동작을 바꾸지 않는다.

## 14.3 사용 예

```text
이 오류 원인 찾아서 고쳐줘
이 함수 설명해줘
이 테스트가 왜 실패해?
이 화면에 버튼 추가해줘
```

## 14.4 금지할 추가 동작

Standard라는 이름 때문에 다음 지침을 덧붙이면 안 된다.

```text
무조건 구현하라
무조건 계획을 생략하라
질문하지 마라
항상 테스트하라
항상 subagent를 써라
```

이런 문구는 최신 기본 하네스의 판단을 오히려 제한한다.

## 14.5 구현 계획

- `AgentMode::Standard`를 default로 지정
- role instruction `None`
- picker 첫 항목
- Tab 순환 첫 항목
- new/resume reset 대상
- UI badge 표시

## 14.6 acceptance criteria

- Standard turn에는 `devez-vibe-agent` key가 없음
- Agent 기능 도입 전과 동일 prompt에서 tool 선택·응답 성향이 실질적으로 동일
- 기존 테스트 전체 통과
- model/provider/Vibe 전환 기능 변화 없음

## 14.7 테스트

- default enum이 Standard
- AppState 생성 시 Standard
- `/new` 후 Standard
- resume 성공 후 Standard
- Standard submit payload에 role context 없음
- Standard에서 existing Task/plan flow 그대로 동작
- Standard에서 existing completion/Tab semantics 보존

---

# 15. Planner Agent

## 15.1 목적

Planner는 Hoje Ask와 Hoje Plan을 하나의 primary role로 합친다.

```text
모호한 요구
→ 저장소 사실 조사
→ 필요한 질문
→ 요구·제약 확정
→ 구현 옵션 분석
→ 계획 초안
→ Architect/Critic 관점 재검토
→ 실행 가능한 최종 계획
```

Planner는 제품 소스를 수정하지 않는다.

## 15.2 사용 조건

사용자가 직접 Planner를 선택한 모든 prompt는 계획 요청으로 해석한다.

적합한 요청:

```text
이 기능 어떻게 설계할까?
인증 구조 변경 계획 세워줘
이 요구를 구현 가능한 수준으로 정리해줘
이 버그 수정 범위와 회귀 위험 분석해줘
```

명확한 질문이라면 인터뷰를 생략하고 바로 조사·계획한다.

## 15.3 요구 명확성 판단

Planner는 다음 항목을 확인한다.

- 목표
- 대상 surface
- 변경 범위
- 필수 동작
- 비기능 요구
- 호환성
- 안전 경계
- acceptance criteria
- non-goal
- 사용자 결정이 필요한 trade-off

### 15.3.1 저장소에서 알아낼 수 있는 사실

다음은 사용자에게 묻기 전에 직접 조사한다.

- 현재 구현 위치
- 기존 naming·pattern
- 호출 흐름
- 데이터 구조
- 테스트 구조
- 사용 중인 library 버전
- project convention
- 이미 존재하는 기능

### 15.3.2 질문 조건

다음 세 조건을 모두 만족할 때만 질문한다.

1. 계획이 materially 달라진다.
2. 저장소나 명시된 context에서 확인할 수 없다.
3. 안전하게 추론하기 어렵다.

가능하면 가장 중요한 질문 하나를 먼저 한다.

지원 provider에서 `AskUserQuestion`을 사용할 수 있으면 사용한다. 없으면 일반 text로 명확한 질문과 선택 결과를 적는다.

## 15.4 조사 정책

- 관련 파일과 호출 경로를 먼저 확인한다.
- 첫 검색 결과 하나로 결론내리지 않는다.
- 현재 구현, 과거 이슈, 가정을 구분한다.
- 실행한 명령과 실행하지 않은 명령을 구분한다.
- 제품 소스 변경 명령, formatter, dependency install, migration 실행은 하지 않는다.
- 필요한 경우 build/test 같은 진단 명령은 실행할 수 있으나 제품 파일을 의도적으로 바꾸지 않는다.

## 15.5 계획 작성 단계

### 단계 A — 요청 해석

- 최종 목표
- 명시 요구
- 숨은 제약
- non-goal
- 불확실성

### 단계 B — 저장소 조사

- 영향 파일
- symbol/호출 경로
- state 흐름
- provider/API 계약
- 기존 test surface

### 단계 C — 옵션 분석

의미 있는 선택지가 둘 이상일 때만 비교한다.

```text
옵션
장점
단점
호환성 영향
운영 비용
추천 여부
```

대안이 실질적으로 없으면 억지로 가짜 옵션을 만들지 않는다.

### 단계 D — 계획 초안

초안은 다음을 포함한다.

- 변경 목적
- 영향 경로
- 계약 변화
- 순서가 있는 구현 단계
- migration/compatibility
- 위험
- test/verification
- acceptance criteria

### 단계 E — Architect 관점

- 구조 경계가 맞는가
- 책임 배치가 맞는가
- 제품 계약이 보존되는가
- 보안·권한 경계를 침범하지 않는가
- provider별 차이를 놓치지 않았는가

### 단계 F — Critic 관점

- 요구 누락
- sequencing 오류
- hidden dependency
- 약한 acceptance criteria
- 테스트는 통과하지만 사용자 동작은 깨지는 경우
- 계획이 불필요하게 큰 경우

### 단계 G — 최종 계획

blocker를 반영하고 실행자가 추가 해석 없이 시작할 수 있는 계획을 만든다.

## 15.6 complexity별 검토 강도

| 강도 | 기준 | 검토 방식 |
|---|---|---|
| Light | 한 surface, 저위험, 명확한 변경 | Planner self-review |
| Standard | 다중 파일, cross-layer, 일반 기능 | Planner + Critic 관점 |
| High-risk | auth, security, migration, concurrency, public API | Planner + Architect + Critic |

실제 subagent가 가능하면 독립 lane을 사용할 수 있다. 불가능하면 순차 self-review로 낮추고 독립 검토라고 표현하지 않는다.

## 15.7 출력 계약

Planner 최종 산출물 권장 구조:

```markdown
## 목표
## 확인된 저장소 사실
## 확정 요구사항
## 가정과 미해결 결정
## 권장 설계
## 영향 범위
## 구현 단계
## 위험과 대응
## 검증 계획
## 완료 기준
```

단순한 계획은 불필요한 빈 섹션을 생략한다.

Planner 결과에는 일반 200자 제한을 적용하지 않는다. 대신 중복, 장황한 서론, 같은 단계 재진술은 피한다.

## 15.8 mutation contract

Planner는 다음을 하지 않는다.

- 제품 파일 Edit/Write
- formatter 실행
- dependency 변경
- migration 실행
- commit/push/PR
- 구현 subagent 실행
- 계획 승인 전 Finisher 자동 시작

사용자가 Planner mode에서 구현을 요청해도 계획까지만 작성한다.

권장 응답:

```text
Planner에서는 구현하지 않고 실행 가능한 계획까지 정리합니다.
실제 반영은 Standard 또는 Finisher로 전환한 뒤 진행합니다.
```

## 15.9 구현 계획

- `planner.md` compile-time embedding
- role metadata: `mutation=forbidden`, `output=expanded-artifact`
- role prompt에 clarification gate 포함
- role prompt에 repo-first 조사 포함
- role prompt에 Architect/Critic review ladder 포함
- AgentMode 선택 시 turn context에 추가
- UI badge `Planner`

## 15.10 테스트

- Planner context에 no-edit contract 존재
- clear request에서 질문 강제 문구 없음
- repo fact를 사용자에게 묻지 않는 규칙 존재
- facts/assumptions 분리 규칙 존재
- affected paths/contracts/risks/verification 포함
- detailed artifact exception 작동
- product file mutation을 요청해도 계획만 생성하는 prompt contract
- provider별 role context 전달
- history에서 role context 숨김

---

# 16. Advisor Agent

## 16.1 목적

Advisor는 수동적인 동의자가 아니라 기술 의사결정 보조 역할이다.

```text
사용자 접근법
→ 실제 구현·제품 계약 확인
→ 장점과 위험 분석
→ 필요한 반론
→ 대안 비교
→ 권장안과 조건 제시
```

Planner가 “어떻게 구현할 것인가”를 다룬다면 Advisor는 “그 선택이 적절한가”를 다룬다.

## 16.2 사용 예

```text
이걸 컬럼 하나 추가해서 처리하려는데 어때?
WPF 대신 Electron으로 옮기는 게 나을까?
이 API를 직접 호출하는 구조 괜찮아?
A와 B 중 어떤 방식이 유지보수에 좋아?
내 설계에서 놓친 문제 있어?
```

## 16.3 핵심 행동

- 사용자의 제안을 그대로 반복하지 않는다.
- 저장소와 실제 제약을 확인한다.
- 의미 있는 위험만 제시한다.
- 더 단순한 대안이 있으면 추천한다.
- 현재 요구에서 원안이 가장 적절하면 명확히 승인한다.
- 미래 확장 가능성을 핑계로 불필요한 추상화를 추천하지 않는다.
- 필수 문제와 선택 개선을 분리한다.
- 최종 결정권은 사용자에게 둔다.

## 16.4 반론 기준

다음 경우에 적극적으로 반론한다.

- correctness 문제가 예상됨
- 데이터 손실·보안·권한 위험
- 기존 제품 계약 위반
- 숨은 운영 비용이 큼
- 회귀 가능성이 높음
- 현재 요구보다 복잡도가 과도함
- 이미 있는 구조를 불필요하게 중복함
- provider/API 실제 동작과 충돌함

다음 이유만으로는 반론하지 않는다.

- 개인 취향
- 추상적인 “확장성”
- 근거 없는 best practice
- 실제 요구와 무관한 대규모 리팩터링 가능성
- 단지 다른 방법도 존재한다는 사실

## 16.5 evidence policy

강한 반론에는 다음 중 하나 이상의 근거가 필요하다.

- 현재 코드 경로
- 현재 데이터/상태 계약
- 테스트 동작
- 공식 문서/API 계약
- 명백한 논리적 반례
- 사용자가 명시한 제약

근거가 약하면 `가능성`, `추정`, `확인 필요`로 표현한다.

## 16.6 출력 계약

권장 구조:

```markdown
## 판단
권장 / 조건부 허용 / 비권장

## 핵심 근거

## 반드시 해결할 문제

## 선택 개선

## 대안과 트레이드오프

## 추천안
```

단순한 질문은 짧게 답한다. 비교 선택지가 많거나 근거가 중요한 경우 role output 예외로 필요한 상세도를 허용한다.

## 16.7 mutation contract

Advisor는 제품 소스를 수정하지 않는다.

- Read/Grep/Glob 등 조사 허용
- 비파괴 diagnostic command 허용
- Edit/Write/formatter/install/migration/commit 금지
- “좋은 방법”을 실제로 구현하지 않음

사용자가 “의견도 주고 구현해줘”라고 해도 Advisor에서는 먼저 판단만 제공하고 Standard/Finisher 전환을 안내한다.

## 16.8 Planner와 경계

| 요청 | Planner | Advisor |
|---|---|---|
| 요구가 애매함 | 명확화 후 계획 | 핵심 제안이 없으면 판단 질문 |
| 구현 단계 작성 | 주 역할 | 필요할 때만 요약 |
| 사용자 제안 평가 | 계획 입력으로 사용 | 주 역할 |
| 대안 비교 | 설계 결정에 필요할 때 | 주 역할 |
| 제품 소스 수정 | 금지 | 금지 |
| 최종 산출물 | 실행 계획 | 판단·추천 |

## 16.9 구현 계획

- `advisor.md` compile-time embedding
- role metadata: `mutation=forbidden`, `output=adaptive-expanded`
- must-fix/optional 구분
- no manufactured objection 규칙
- recommendation과 user-decision 규칙
- evidence-first 규칙
- UI badge `Advisor`

## 16.10 테스트

- Advisor prompt에 추천·반론·대안 포함
- 원안이 타당하면 승인하는 규칙 포함
- 근거 없는 반론 금지
- must-fix와 optional 분리
- 사용자 최종 결정 존중
- 제품 파일 mutation 금지
- 상세 비교 output exception
- provider별 role 전달과 history 비노출

---

# 17. Finisher Agent

## 17.1 목적

Finisher는 단순 Builder가 아니라 목표 완결 책임을 가진다.

```text
요청/계획
→ Definition of Done
→ bounded goals
→ 구현
→ targeted verification
→ review
→ QA / adversarial check
→ blocker 해결
→ 전체 재검증
→ 증거 기반 완료
```

## 17.2 Standard와 차이

| 항목 | Standard | Finisher |
|---|---|---|
| 일반 구현 | provider 판단 | 목표 완료 책임 |
| 작업 분해 | 필요 시 | 복합 작업이면 명시적 |
| 검증 | 기본 harness 판단 | 완료 조건으로 강제 |
| 독립 QA | 선택적 | standard/strict에서 우선 |
| blocker | 보고할 수 있음 | 해결 가능하면 계속 해결 |
| 완료 선언 | 일반 응답 | evidence gate 통과 후 |

## 17.3 Definition of Done

Finisher는 작업 시작 전 내부적으로 다음을 확정한다.

- 사용자가 요청한 실제 동작
- 영향 surface
- 필요한 artifact
- 필수 test
- regression 범위
- 금지된 변경
- 완료 판정 기준

사용자 요구가 materially 모호하면 한 번에 핵심 질문 하나를 한다. 하지만 Planner처럼 광범위한 인터뷰를 기본으로 하지 않는다.

## 17.4 실행 강도

### Light

적합 조건:

- 로컬 저위험 변경
- 대략 2개 이하 파일
- 대략 200 net lines 미만
- 단일 surface
- 보안·데이터·호환성 위험 없음

동작:

```text
직접 구현
→ targeted verification
→ self-review
→ 전체 관련 검증 한 번 재실행
```

### Standard

기본 복합 작업:

- 다중 파일
- cross-layer
- 상태/이벤트 흐름 변경
- 일반 기능 추가
- 회귀 가능성 존재

동작:

```text
bounded goal 분해
→ 구현
→ targeted test
→ Architect 관점 review
→ QA 관점 검증
→ blocker 수정
→ 전체 재실행
```

### Strict

다음 위험에서 자동 승격한다.

- 인증·권한
- 보안
- 결제
- destructive data path
- migration
- concurrency/race
- public API compatibility
- production infrastructure
- 사용자 명시 maximum assurance

동작:

```text
더 작은 goal 분해
→ 독립 구현·검토 lane 우선
→ 확대 regression
→ adversarial case
→ compatibility 확인
→ 전체 재실행
```

v1 UI에는 intensity를 별도 picker로 노출하지 않는다. Finisher가 가장 낮은 안전 강도를 선택하고 작업 초기에 한 줄로 알린다.

## 17.5 bounded goal 원칙

각 goal은 다음 조건을 만족한다.

- 독립적으로 설명 가능
- 변경 범위가 제한됨
- 완료 조건이 명확함
- 검증 방법이 존재함
- 관련 없는 사용자 변경을 건드리지 않음

검증 경계가 같은 작은 작업을 과도하게 쪼개지 않는다.

## 17.6 구현 loop

```text
1. 현재 goal 조사
2. 필요한 파일만 수정
3. targeted verification
4. 결과 review
5. blocker가 있으면 수정
6. goal 검증 재실행
7. 다음 goal
8. 마지막에 전체 검증
```

종료 직전에 여러 task를 한꺼번에 완료 처리하지 않는다. 기존 Devez Vibe task 진행 규칙을 따른다.

## 17.7 QA contract

가능하면 구현 context와 분리된 QA lane을 사용한다.

QA는 다음을 본다.

- 실제 사용자 surface
- acceptance criteria
- regression
- boundary case
- adversarial case
- artifact 존재
- 검증 명령 결과

같은 모델·같은 context의 self-review만 수행했다면 “독립 QA”라고 표현하지 않는다.

## 17.8 blocker 처리

### 해결 가능 blocker

- failing test
- 누락 구현
- 잘못된 가정
- 재현 가능한 bug
- 설치 가능한 개발 의존성
- 저장소에서 조사 가능한 모호성

가능한 범위에서 계속 해결한다.

### 사용자만 해결 가능한 blocker

- credential/secret
- 외부 승인
- 물리적 작업
- 접근 권한
- 제품 의도 결정

이 경우 필요한 정보와 중단 지점을 정확히 설명하고 질문한다.

### 금지

- 해결 가능한 실패를 “나중에 확인”으로 넘기기
- 검증 실패를 warning으로 약화해 완료 선언
- artifact가 없는데 완료 주장
- 실행하지 않은 test를 통과했다고 보고

## 17.9 완료 보고 계약

완료 보고에는 필요한 범위에서 다음을 포함한다.

- 실제 변경 결과
- 실행한 핵심 검증
- 남은 risk 또는 없음
- 사용자 조치가 필요한 blocker

일반적으로는 간결하게 유지한다. 다만 증거를 생략하면 완료 여부 판단이 왜곡될 때만 상세 output 예외를 사용한다.

## 17.10 구현 계획

- `finisher.md` compile-time embedding
- role metadata: `mutation=allowed`, `output=concise-evidence`
- light/standard/strict 선택 규칙
- goal decomposition
- QA/review degradation ladder
- no false completion 규칙
- blocker classification
- UI badge `Finisher`

## 17.11 테스트

- Finisher prompt에 light/standard/strict 존재
- related user changes 보존
- inspect before edit
- targeted verification
- regression/adversarial check
- evidence completion gate
- human-only blocker 구분
- independent lane 부재 시 truthful degradation
- final whole-scope rerun
- provider별 role 전달과 history 비노출

---

## 18. subagent와 capability degradation

### 18.1 capability ladder

```text
Level 1: native subagent + task 지원
Level 2: task 지원, subagent 없음
Level 3: 일반 tool만 지원
```

### 18.2 Planner

| Level | 방식 |
|---|---|
| 1 | Planner 초안 → Architect/Critic subagent 검토 |
| 2 | task 단계로 초안 → architecture pass → critic pass |
| 3 | 한 context에서 명시적 checklist self-review |

### 18.3 Advisor

| Level | 방식 |
|---|---|
| 1 | 필요 시 독립 architecture/critic 의견 수집 |
| 2 | 같은 모델에서 evidence pass와 recommendation pass 분리 |
| 3 | 단일 답변에서 facts/risks/options/recommendation checklist |

### 18.4 Finisher

| Level | 방식 |
|---|---|
| 1 | implementation lane + review/QA lane |
| 2 | task별 구현 후 별도 sequential review pass |
| 3 | 구현 후 clean-context에 가까운 self-review checklist와 전체 test 재실행 |

### 18.5 화면 표시

실제 provider가 subagent lifecycle event를 보낼 때만 subagent row를 표시한다.

prompt 내부에서 “Architect 관점으로 검토”했다고 해서 가짜 subagent row를 만들지 않는다.

---

## 19. 파일별 구현 계획

### 19.1 `src/agent.rs` 신규

책임:

- `AgentMode`
- label/description/cycle
- prompt include
- output/mutation policy
- role context rendering
- parser/alias
- 관련 pure unit test

### 19.2 `prompts/agents/planner.md` 신규

- Planner identity
- clarification gate
- repo-first investigation
- no mutation
- architecture/critic review
- final plan output contract

### 19.3 `prompts/agents/advisor.md` 신규

- recommendation role
- evidence policy
- meaningful pushback
- no fabricated objection
- alternatives/trade-offs
- no mutation

### 19.4 `prompts/agents/finisher.md` 신규

- Definition of Done
- intensity
- goal loop
- verification/QA
- blocker policy
- completion gate

### 19.5 `src/state.rs`

예상 변경:

- `AgentMode` import
- `AppState.selected_agent`
- `AppState.active_turn_agent`
- getter/setter/cycle
- Agent picker overlay
- `/agent` command parsing
- `Action::SetAgentMode` 또는 local state change action
- composer mode view에 Agent label
- `forked_side_state` 상속
- new/resume reset
- busy 전환 거부
- state unit test

### 19.6 `src/main.rs`

예상 변경:

- `mod agent`
- `turn_additional_context(vibe, agent)`
- `start_turn` snapshot
- `start_split_turn` snapshot
- turn 완료/실패/interrupt에서 snapshot 정리 연결
- bare Tab fallback cycle
- `/btw` Tab 우선순위 보존
- badge click action
- 공통 prompt의 role output exception
- integration test

### 19.7 `src/backend.rs`

예상 변경:

- `combined_turn_instructions`에 `devez-vibe-agent`
- role context 순서 보장
- Codex context cleanup에서 role key 보존
- provider별 전달 test

agent mode persistence를 하지 않으므로 config key 추가는 필요 없다.

### 19.8 `src/renderer.rs`

예상 변경:

- `ComposerMode.agent`
- `Pick::AgentMode`
- badge paint
- width 축약
- click geometry
- narrow terminal test

### 19.9 `src/claude.rs`

원칙적으로 변경하지 않는다.

역할 context는 backend에서 기존 Claude bridge handoff 경로로 전달한다.

### 19.10 `npm/bridge/claude-agent-sdk-bridge.mjs`

원칙적으로 변경하지 않는다.

다만 재검증에서 role contract가 history에 노출되거나 handoff wrapper에서 잘리면 exact strip/length handling만 최소 수정한다.

### 19.11 `src/open_code.rs`

원칙적으로 변경하지 않는다.

기존 `start_prompt_content(..., turn_context, ...)`가 역할 contract를 받는지 test로 확인한다.

### 19.12 문서

- `README.md`
- `npm/README.md`
- `/help` command 설명
- Tab 우선순위
- Agent 역할 표
- v1 non-goal

---

## 20. 제안 API와 의사 코드

### 20.1 Agent 정의

```rust
pub enum AgentOutputPolicy {
    Common,
    ExpandedArtifact,
    AdaptiveExpanded,
    ConciseEvidence,
}

pub enum AgentMutationPolicy {
    DefaultHarness,
    Forbidden,
    Allowed,
}
```

### 20.2 role context

```rust
pub fn role_context(mode: AgentMode) -> Option<String> {
    let instructions = mode.instructions()?;
    Some(format!(
        "<devez-vibe-agent mode=\"{}\" output=\"{}\" mutation=\"{}\">\n{}\n</devez-vibe-agent>",
        mode.config_value(),
        mode.output_policy().wire(),
        mode.mutation_policy().wire(),
        instructions,
    ))
}
```

### 20.3 턴 context

```rust
fn turn_additional_context(vibe: VibeMode, agent: AgentMode) -> Value {
    let mut context = existing_context(vibe);
    if let Some(value) = agent::role_context(agent) {
        context["devez-vibe-agent"] = json!({
            "value": value,
            "kind": "application"
        });
    }
    context
}
```

### 20.4 submit snapshot

```rust
let agent = state.begin_turn_agent();
let additional_context = turn_additional_context(state.vibe_mode(), agent);
```

`begin_turn_agent`는 이미 busy이면 새 snapshot을 만들지 않는다.

### 20.5 completion

```rust
if method == "turn/completed" {
    state.finish_turn_agent();
}
```

실패·interrupt·stall recovery에서도 같은 cleanup path를 사용한다.

### 20.6 Tab 조건

```rust
if can_cycle_agent_with_tab(state, key, btw_state.as_ref(), completion_state) {
    state.cycle_agent();
    return Action::Tick(true);
}
```

`can_cycle_agent_with_tab`은 pure function으로 만들고 우선순위 회귀를 unit test한다.

### 20.7 직접 선택

```rust
/agent            → picker
/agent planner    → set when idle
```

busy이면 state를 바꾸지 않고 notice만 표시한다.

---

## 21. prompt 품질 검증 계획

### 21.1 정적 invariant test

prompt text에서 다음을 확인한다.

Planner:

- 저장소 조사 우선
- 제품 파일 수정 금지
- 사실/가정 구분
- 질문 gate
- 계획 영향 경로·계약·위험·검증

Advisor:

- recommendation
- meaningful pushback
- no fabricated objections
- must-fix/optional
- user final decision
- mutation 금지

Finisher:

- Definition of Done
- intensity
- bounded goals
- targeted verification
- QA/review
- blocker
- false completion 금지

### 21.2 금지 문구 test

- Planner/Advisor에 “바로 구현” 강제 없음
- Standard에 role prompt 없음
- Finisher에 `.hoje` 의존 없음
- research/Insane Search 자동 호출 없음
- OpenCode native mode 요구 없음

### 21.3 token budget

권장 목표:

| role | 대략적 prompt 크기 |
|---|---:|
| Standard | 0 추가 token |
| Planner | 1,500~2,500 token |
| Advisor | 900~1,600 token |
| Finisher | 1,500~2,500 token |

원문 Hoje SKILL.md 전체를 넣지 않는다.

---

## 22. 테스트 매트릭스

### 22.1 Agent enum·parser

- default = Standard
- cycle 순서 정확
- full name parse
- alias parse
- unknown value 거부
- label/short label 정확

### 22.2 state

- AppState 생성 Standard
- set/cycle
- busy에서 변경 거부
- turn snapshot
- complete/fail/interrupt cleanup
- model/effort/Vibe 변경 후 유지
- `/new` reset
- resume 성공 reset
- resume 실패 시 유지

### 22.3 `/agent`

- no argument picker
- direct selection
- alias
- unknown mode notice
- busy notice
- focused Btw pane 적용
- picker click
- close/cancel

### 22.4 Tab

- idle empty composer에서 cycle
- composer text가 있으면 cycle 안 함
- completion이 있으면 completion 우선
- overlay가 있으면 overlay 우선
- 질문이 있으면 질문 우선
- Btw split이면 focus 전환 우선
- Shift+Tab 기존 동작 보존
- busy/thread pending/compaction에서 cycle 안 함

### 22.5 UI

- full label
- narrow label
- badge click
- theme별 표시
- fullscreen/inline
- side panel open/closed
- xterm width profile
- 한글/전각 폭 회귀 없음

### 22.6 turn payload

- Standard role key 없음
- Planner key 있음
- Advisor key 있음
- Finisher key 있음
- role metadata 정확
- selected/active snapshot 일치
- queued prompt behavior
- steer에서 role 교체 없음

### 22.7 Claude

- combined context에 Agent 포함
- common system role output exception 존재
- Planner 상세 artifact가 공통 cap에 잘리지 않음
- user history에서 role tag 제거
- resume preview에 tag 없음
- model/effort/permission 변화 없음

### 22.8 Codex

- Agent application context 전달
- prepare cleanup이 role key를 지우지 않음
- Standard no-delta
- provider handoff와 role 동시 전달
- history에 internal context 노출 없음

### 22.9 OpenCode

- turn context에 role 포함
- native set_mode 호출 없음
- 기존 Agent/task 표시와 충돌 없음
- history replay에 internal role 노출 없음

### 22.10 Btw

- fork 시 Agent 상속
- 이후 pane별 독립 변경
- Tab focus 전환 보존
- focused pane `/agent`
- main turn agent와 Btw turn agent 혼동 없음
- close 후 main 상태 보존

### 22.11 prompt behavior scenario

Standard:

- 질문 답변
- 단순 수정
- 일반 bug fix

Planner:

- 명확한 계획 요청은 질문 없이 계획
- 모호한 요청은 핵심 질문 하나
- repo 사실은 직접 조사
- 구현 요구에도 소스 미수정

Advisor:

- 좋은 원안은 승인
- 위험한 원안은 근거와 반론
- 의미 없는 대안 생성 안 함
- must-fix/optional 분리

Finisher:

- light task 직접 구현·검증
- standard task review/QA
- strict surface 승격
- failing test에서 완료 선언 금지
- human blocker 질문

### 22.12 전체 회귀

- `cargo fmt --check`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -D warnings`가 현재 CI 계약에 맞는지 확인
- Claude bridge self-test
- npm staging/dry-run은 제품 코드 구현 완료 시 수행
- 기존 session/resume/provider/Btw test 통과

---

## 23. 구현 단계

### Phase 1 — contract 고정

- Agent 이름·순서 확정
- 역할별 prompt 작성
- output/mutation policy 작성
- 공통 system role exception 작성
- 정적 prompt test 작성

완료 조건:

- 네 역할 경계가 문서와 prompt에서 일치
- Standard no-role contract
- research/automatic 없음

### Phase 2 — state와 picker

- `AgentMode`
- AppState 필드
- `/agent`
- picker
- busy guard
- reset/inheritance 규칙

완료 조건:

- provider 호출 없이 UI state 전환 test 통과

### Phase 3 — renderer와 key handling

- composer badge
- click target
- bare Tab fallback
- Btw precedence
- narrow layout

완료 조건:

- 기존 입력 회귀 없음
- 네 역할 화면 식별 가능

### Phase 4 — turn injection

- `turn_additional_context` 확장
- active turn snapshot
- completion cleanup
- Claude/Codex/OpenCode context 전달

완료 조건:

- provider별 payload test 통과
- history 비노출 test 통과

### Phase 5 — 역할 behavior 검증

- Planner scenario
- Advisor scenario
- Finisher intensity/degradation
- output exception

완료 조건:

- 역할별 대표 prompt golden/behavior test 또는 수동 검증 기록

### Phase 6 — 문서와 release 준비

- README
- npm README
- `/help`
- 변경 로그
- 전체 test

완료 조건:

- Standard 기본 사용자에게 migration 작업 없음
- 설치 후 즉시 수동 Agent 선택 가능

---

## 24. 위험과 대응

### 24.1 공통 prompt가 Planner 출력을 자름

위험:

- Claude system prompt의 200자 cap이 role turn context보다 우선

대응:

- common prompt에 role-aware exception
- Agent context metadata
- Planner long-output test

### 24.2 역할 instruction이 history에 보임

위험:

- resume preview 오염
- provider handoff에 prompt contract 반복

대응:

- existing internal handoff wrapper 사용
- exact strip test
- user prompt 원문 비교 test

### 24.3 Tab 충돌

위험:

- completion 선택 실패
- Btw focus 전환 실패
- 입력 중 mode가 바뀜

대응:

- Agent cycle을 마지막 fallback으로 제한
- empty composer 조건
- pure precedence test

### 24.4 실행 중 UI 역할 불일치

위험:

- 화면은 Advisor인데 turn은 Planner

대응:

- active turn snapshot
- busy 전환 금지
- active badge 우선 표시 또는 selected와 active가 항상 같게 유지

### 24.5 Planner/Advisor가 파일 수정

위험:

- prompt contract 위반

대응:

- 강한 no-mutation prompt
- tool event 관찰 test
- 후속 hard guard 후보 기록
- v1에서 보안 보장이라고 표현하지 않음

### 24.6 Finisher가 너무 무거움

위험:

- 작은 변경에도 subagent·review 반복

대응:

- lowest safe intensity
- light 기준
- 실제 위험 증가 시에만 승격

### 24.7 provider별 tool 차이

위험:

- Agent가 없는 tool을 요구

대응:

- capability-neutral prompt
- degradation ladder
- 실제 수행 수준을 정직하게 보고

### 24.8 prompt token 증가

위험:

- 매 턴 role prompt 비용

대응:

- Standard 0 token
- Hoje workflow 원문 미포함
- role prompt 압축
- 이후 cache 가능성 별도 검토

### 24.9 Agent와 Vibe 혼동

위험:

- Agent가 출력 표시 preset처럼 보임

대응:

- badge 분리
- `/agent`와 `/vibemode` 명확한 명칭
- help에 역할 vs 표시 차이 설명

### 24.10 resume에서 특수 역할 부활

위험:

- 과거 Planner 세션을 resume했는데 구현 요청도 read-only 처리

대응:

- resume 성공 시 Standard reset
- role을 history에 저장하지 않음

---

## 25. acceptance criteria

### 25.1 기능

- 네 Agent를 picker와 command로 선택 가능
- 기본값 Standard
- idle empty composer Tab 순환
- Btw Tab focus 보존
- badge에서 현재 Agent 확인
- Claude/Codex/OpenCode 공통 동작

### 25.2 Standard

- role context 없음
- 현재 기본 동작과 실질적 차이 없음
- model/effort/provider/Vibe 독립

### 25.3 Planner

- 요구 명확화와 계획 통합
- 저장소 조사 우선
- 제품 소스 미수정
- architecture/critic 재검토
- 상세 계획 출력 가능

### 25.4 Advisor

- 추천·반론·대안
- 근거 없는 반대 없음
- must-fix/optional 구분
- 제품 소스 미수정

### 25.5 Finisher

- goal 분해
- intensity 선택
- 구현·검증·review/QA
- blocker 해결
- evidence 없는 완료 금지

### 25.6 안정성

- role context history 비노출
- turn snapshot 일치
- resume/new reset
- provider switch 유지
- 전체 기존 test 통과

---

## 26. 1차 초안 재검수 결과

초안 작성 후 현재 `main`, Hoje-code v0.15.5 역할 계약, Claude/Codex/OpenCode 전달 구조를 다시 대조했다.

### 26.1 발견한 문제와 수정

| 번호 | 초안 문제 | 최종 수정 |
|---:|---|---|
| 1 | role turn context가 공통 prompt보다 뒤에 오면 Planner 분량 제한을 자동으로 이긴다고 가정 | Claude system hierarchy를 반영해 common prompt에 조건부 role output exception 추가 |
| 2 | Standard를 byte-identical prompt로 정의 | 공통 조건문 추가 필요성을 반영해 “role context 없음 + 행동 무변경”으로 수정 |
| 3 | Tab cycle 조건이 넓어 입력·completion 충돌 가능 | split 없음, overlay 없음, completion 없음, idle, empty composer 조건 추가 |
| 4 | 실행 중 Agent 변경 상태가 불명확 | active turn snapshot과 busy 변경 금지 확정 |
| 5 | steer가 새 역할을 받을 가능성 미정 | steer는 active turn agent 고정으로 확정 |
| 6 | resume/new에서 role 유지 여부 미정 | 성공 시 Standard reset으로 확정 |
| 7 | Btw가 main Agent를 공유하는지 독립인지 불명확 | fork 때 상속 후 pane별 독립으로 확정 |
| 8 | OpenCode native agent 전환 가능성을 남김 | v1에서 사용하지 않는 것으로 확정 |
| 9 | Planner/Advisor read-only가 hard guard처럼 읽힐 수 있음 | prompt contract이며 security sandbox가 아님을 명시 |
| 10 | subagent 없는 provider에서 독립 검토를 가장할 위험 | capability degradation과 truthful reporting 추가 |
| 11 | role context가 history에 남는 문제 누락 | provider별 history strip test 추가 |
| 12 | provider handoff에 role을 넣을지 미정 | UI state만 소유하고 handoff snapshot에는 넣지 않기로 확정 |
| 13 | Finisher가 작은 작업에도 과도하게 무거울 수 있음 | lowest safe intensity와 light 기준 추가 |
| 14 | Advisor가 반론 자체를 목적으로 할 위험 | no manufactured objection와 원안 승인 규칙 추가 |
| 15 | Research 논의가 설계 범위에 섞일 가능성 | v1 non-goal에 Hoje Research·Insane Search를 명시 |
| 16 | Automatic router가 나중에 암묵적으로 들어갈 여지 | 수동 선택만 지원하고 자동 전환 금지 명시 |
| 17 | 역할별 output 형식이 공통 200자 규칙과 충돌 | AgentOutputPolicy와 role별 예외 범위 추가 |
| 18 | role prompt가 특정 provider tool 이름에 종속될 위험 | capability-neutral wording와 단계적 degradation 추가 |

### 26.2 누락 점검

다음 항목이 최종 문서에 포함됐는지 확인했다.

- [x] 전체 Agent 시스템 아키텍처
- [x] Standard 상세 계약
- [x] Planner 상세 flow와 구현 계획
- [x] Advisor 상세 flow와 구현 계획
- [x] Finisher 상세 flow와 구현 계획
- [x] Hoje-code 참고 매핑
- [x] 가져오지 않을 Hoje runtime 요소
- [x] provider별 전달 방식
- [x] Claude system hierarchy 문제
- [x] history 비노출
- [x] state lifecycle
- [x] new/resume/provider switch
- [x] Btw 상속과 Tab 충돌
- [x] UI picker와 badge
- [x] prompt 파일 구조
- [x] capability degradation
- [x] 테스트 매트릭스
- [x] 위험과 대응
- [x] 단계별 구현 순서
- [x] acceptance criteria
- [x] Automatic/Research 제외

### 26.3 재검수 결론

본 계획은 현재 Devez Vibe의 공통 UI·provider routing 구조에 맞게 구현 가능하다.

가장 중요한 구현 조건은 다음 세 가지다.

1. `Standard`에는 role context를 넣지 않는다.
2. Claude 공통 system prompt에 좁은 role output 예외를 추가한다.
3. Agent는 턴 시작 시 고정하고 실행 중에는 전환하지 않는다.

이 세 조건을 지키면 기존 기본 하네스를 보존하면서 Planner, Advisor, Finisher의 행동 차이를 provider 공통으로 제공할 수 있다.

---

## 27. 최종 구현 의사결정 요약

| 항목 | 최종 결정 |
|---|---|
| 기본 Agent | Standard |
| Agent 수 | 4개 |
| 선택 방식 | 수동 |
| Tab 순서 | Standard → Planner → Advisor → Finisher |
| busy 전환 | 금지 |
| new/resume | Standard reset |
| provider switch | 현재 선택 유지 |
| Btw | 생성 시 상속, 이후 독립 |
| Standard role prompt | 없음 |
| Planner | Ask + Plan 통합, read-only contract |
| Advisor | 평가·추천·반론, read-only contract |
| Finisher | Goals 기반 완결 실행 |
| role 전달 | 매 턴 application context |
| Claude 세션 재시작 | 하지 않음 |
| OpenCode native mode | 사용하지 않음 |
| mode persistence | v1 없음 |
| Automatic | v1 없음 |
| Research | v1 없음 |
| `.hoje` runtime | 이식하지 않음 |
| hard mutation guard | 후속 후보 |

---

## 28. 구현 시작 전 체크리스트

- [ ] Agent 이름과 UI 표기 최종 승인
- [ ] Tab empty-composer 조건 승인
- [ ] new/resume Standard reset 승인
- [ ] common prompt role output exception 문구 리뷰
- [ ] Planner/Advisor no-mutation가 v1 prompt contract임을 승인
- [ ] Finisher intensity 기준 리뷰
- [ ] prompt 파일 경로 결정
- [ ] provider별 history strip test fixture 확보
- [ ] narrow terminal badge 우선순위 확정
- [ ] 전체 test 기준과 CI 명령 확인

이 체크리스트가 승인된 뒤에만 제품 코드 구현을 시작한다.
