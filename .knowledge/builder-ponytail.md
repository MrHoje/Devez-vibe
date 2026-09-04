# Builder 에이전트의 Ponytail 규칙 적용 기록

> 적용일: 2026-09-04
> 대상 역할: `Builder` (코드상 `AgentMode::Standard`)
> 원본: [DietrichGebert/ponytail](https://github.com/dietrichgebert/ponytail) (MIT)
> 원본 규칙 파일: `skills/ponytail/SKILL.md`

이 문서는 Builder 역할에 Ponytail 규칙을 넣으면서 무엇을, 왜, 어떻게 바꿨는지와
나중에 되돌릴 때 손대야 할 지점을 기록한다.

---

## 1. Ponytail이란

- AI 코딩 에이전트가 "가장 게으른 시니어 개발자"처럼 필요한 최소 코드만 쓰게 만드는
  오픈소스 규칙 세트다. 모델 파인튜닝이나 외부 서버 호출은 없고, 마크다운 규칙을
  세션 컨텍스트에 주입하는 순수 프롬프트 엔지니어링이다.
- 원본은 Claude Code 플러그인 형태로, 세션 시작 훅과 사용자 입력 훅 두 개가
  규칙 주입과 강도(`lite`/`full`/`ultra`) 상태 파일 관리를 맡는다.
- 핵심 규칙은 세 덩어리다.
  - 7단계 사다리: 필요한가 → 코드베이스에 있는가 → 표준 라이브러리 → 플랫폼 기본 기능
    → 설치된 의존성 → 한 줄로 가능한가 → 그다음에야 최소 코드. 첫 해결 단계에서 멈춘다.
  - 절대 깎지 않는 항목: 신뢰 경계 검증, 데이터 손실 방지 오류 처리, 보안, 접근성,
    사용자가 명시적으로 요청한 것.
  - 금지 목록: 구현 하나뿐인 인터페이스, 제품 하나용 팩토리, 바뀌지 않는 값의 설정,
    나중을 위한 보일러플레이트, 요청에 없는 추상화.

## 2. 왜 플러그인 대신 직접 넣었나

- 원본 플러그인의 훅은 Claude Code 전용이다. DevezVibe는 Claude, Codex, OpenCode 세
  제공자에 같은 역할 텍스트를 `additionalContext`로 보내므로, 규칙을 역할 프롬프트로
  넣어야 세 제공자에 똑같이 적용된다.
- 사용자 요구는 Builder에서만 동작하는 것이었다. 공용 시스템 프롬프트
  (`src/main.rs`의 `DEVEZ_INSTRUCTIONS`, `CLAUDE_DEVEZ_INSTRUCTIONS`)에 넣으면
  Planner·Goal Runner·Reviewer에도 번지므로 건드리지 않았다.
- 강도 전환은 Builder 하나에 고정하는 용도라 빼고, 원본 기본값인 `full`에 해당하는
  규칙만 남겼다. `lite`(다 만든 뒤 대안 제시)나 `ultra`(요구사항 재검토·삭제)는 아니다.

## 3. 변경된 파일

### 3.1 새 파일: `prompts/agents/builder.md`

- Builder 역할 프롬프트. 영문 약 1,900자, 325단어, 토큰 450개 안팎.
- 원본 `SKILL.md`(약 1,080단어, 토큰 1,400개 안팎)의 3할 크기다. 줄인 방식은 다음과 같다.
  - 통째로 제외: `Persistence`(모드 켜기·끄기 명령), `Intensity`(강도 3단계),
    `Boundaries`(다른 스킬과의 경계).
  - 항목은 유지하고 예시·설명만 제거: `The ladder`(263단어 → 약 120단어),
    `Rules`(171단어 → 약 60단어), `When NOT to be lazy`(194단어 → 절대 깎지 않는 항목 5개),
    `Output`(80단어 → 의도적 단순화 한 줄 보고, 자명한 한 줄은 테스트 생략).
- 첫 문단에 "이전 턴이 Planner·Goal Runner·Reviewer를 골랐다는 이유만으로 그 역할을
  이어가지 말라"는 문장을 넣어, 예전 `STANDARD_RESET` 블록의 역할을 대신한다.

### 3.2 `src/agent.rs`

- `BUILDER_PROMPT` 상수를 추가하고 `Standard`가 이를 자기 지침으로 갖는다.
- 이전 구조는 `Standard`만 지침이 없었다. 그래서 다른 역할에서 `Standard`로 돌아올 때
  한 번만 보내는 `STANDARD_RESET` 문구와, 이를 실어 나르는
  `AgentTurnContext { Specialized(AgentMode), StandardReset }` 열거형이 있었다.
- 네 역할이 모두 자기 블록을 매 턴 싣게 되면서 위 둘을 지우고, `AgentMode`에
  `instruction()`과 `render_turn_block()`을 두었다.
- 응답 분량 제한은 Builder만 유지한다. `render_turn_block()`에서 `Standard`는
  "표준 지침의 분량 제한이 그대로 적용된다"는 문구를, 나머지 셋은 "분량 제한 해제"
  문구를 붙인다.
- 테스트: `standard_carries_no_instruction_of_its_own` 대신
  `every_role_ships_a_prompt`, `builder_keeps_the_length_caps_and_specialized_roles_lift_them`.

### 3.3 `src/state.rs`

- 제거: `standard_reset_required` 필드, `next_agent_context()`,
  `note_resumed_transcript()`, `note_agent_dispatch_succeeded()`.
- 추가: `agent_mode()` 접근자.
- `prepare_resume()`는 여전히 역할을 `Standard`로 되돌리되, reset 부채 플래그는
  세우지 않는다. 재개된 첫 턴이 Builder 블록을 실어 이전 역할을 대체한다.
- 테스트: reset 부채를 검증하던 세 개(`standard_sends_one_reset_after_a_specialized_role`,
  `resume_owes_a_reset_and_a_new_thread_does_not`,
  `a_process_opened_on_a_resumed_transcript_owes_a_reset`)를 지우고
  `resume_returns_to_builder` 하나로 대체.

### 3.4 `src/main.rs`

- `turn_additional_context(vibe, agent: AgentMode)`가 `devez-vibe-agent` 키를 항상 넣는다.
  이전에는 `Option<AgentTurnContext>`를 받아 `None`이면 키를 생략했다.
- 두 곳의 턴 전송 함수에서 `next_agent_context()` 대신 `agent_mode()`를 쓰고,
  전송 성공 시 호출하던 `note_agent_dispatch_succeeded()`를 없앴다.
- 세션 시작 함수에서 재개 시 호출하던 `note_resumed_transcript()`를 없앴다.
- 공용 시스템 프롬프트 상수(`DEVEZ_INSTRUCTIONS`, `CLAUDE_DEVEZ_INSTRUCTIONS`,
  `CLAUDE_TURN_REMINDER`)는 수정하지 않았다.

## 4. 런타임 동작

- 세션 시작: 공용 시스템 프롬프트가 `developerInstructions`로 들어간다. 여기는 변경 없음.
- 매 턴: `additionalContext`에 공용 규칙, 응답 모드 안내와 함께 `devez-vibe-agent` 키로
  현재 역할 블록이 실린다. Builder는 `builder.md` 본문에 포장 문구가 붙어 턴마다
  약 2,300자, 토큰 550개 안팎이 추가된다. 이전 Builder는 턴마다 아무것도 보내지 않았으므로
  이 전부가 순증분이다.
- 역할 블록은 "이 대화의 이전 `devez-vibe-agent` 블록을 모두 대체한다"고 선언하므로
  역할 전환에 별도 reset이 필요 없다.

## 5. 제거 절차

Ponytail 규칙만 빼고 싶은지, 예전 구조(Builder는 아무 블록도 안 보냄)까지 복원하고
싶은지에 따라 두 갈래다.

### 5.1 규칙만 빼기 (구조 유지, 권장)

1. `prompts/agents/builder.md`에서 `## Understand first, then climb the ladder`,
   `## Never cut`, `## Do not build`, `## Reporting a deliberate simplification` 절을 지운다.
2. 첫 문단은 남긴다. "이전 턴의 다른 역할을 이어가지 말라"는 문장이 역할 전환 reset을
   대신하고 있기 때문이다. Ponytail을 언급하는 문장만 다듬는다.
3. `src/agent.rs`의 테스트 `every_role_ships_a_prompt`에 있는
   `contains("Builder role")` 검증이 통과하는지 확인한다.
4. `cargo test` 실행.

이 경우 매 턴 추가되는 분량은 포장 문구와 짧은 첫 문단만 남아 토큰 200개 미만이 된다.

### 5.2 예전 구조까지 복원하기

`git log`에서 이 변경 직전 커밋의 `src/agent.rs`, `src/state.rs`, `src/main.rs`를
참고하면 되고, 손으로 되돌릴 때의 요지는 다음과 같다.

1. `src/agent.rs`
   - `BUILDER_PROMPT`와 `prompts/agents/builder.md`를 삭제한다.
   - `instruction()`을 `Option<&'static str>`을 돌려주는 형태로 바꾸고 `Standard`는 `None`.
   - `STANDARD_RESET` 문구와 `AgentTurnContext { Specialized(AgentMode), StandardReset }`
     열거형을 되살리고, 렌더링을 `AgentTurnContext::render()`로 옮긴다.
2. `src/state.rs`
   - `standard_reset_required: bool` 필드를 되살린다. 초기값 `false`,
     `prepare_resume()`에서 `true`, `prepare_new_thread()`에서 다시 `false`.
   - `next_agent_context()`: `Standard`이고 플래그가 켜져 있으면 `StandardReset`,
     `Standard`이고 꺼져 있으면 `None`, 그 외는 `Specialized(mode)`.
   - `note_resumed_transcript()`: 플래그를 `true`로.
   - `note_agent_dispatch_succeeded(context)`: `Specialized`면 `true`, `StandardReset`이면 `false`.
3. `src/main.rs`
   - `turn_additional_context`가 `Option<AgentTurnContext>`를 받고 `None`이면
     `devez-vibe-agent` 키를 생략한다.
   - 두 턴 전송 함수에서 `next_agent_context()`로 컨텍스트를 얻고, 요청이 성공했을 때만
     `note_agent_dispatch_succeeded()`를 호출한다. 실패한 전송은 reset 부채를 남겨야 한다.
   - 세션 시작 함수의 재개 분기에서 `note_resumed_transcript()`를 호출한다.
4. 삭제했던 테스트 네 개(agent.rs 하나, state.rs 세 개)와 main.rs의
   `the_turn_carries_the_selected_role_and_nothing_when_standard`를 되살린다.
5. `cargo test` 실행.

## 6. 확인하지 않은 것

- 원본 Ponytail이 주장하는 코드량·비용 감소 효과가 이 축약본에서도 나는지는 벤치마크로
  확인하지 않았다. 실제 Builder 작업에서 코드량이 줄지 않으면 원본 `SKILL.md`의 사다리
  예시 문장을 `builder.md`에 몇 개 되살리는 것이 다음 조치다.
