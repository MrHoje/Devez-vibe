# Builder 에이전트의 Ponytail 규칙 적용 기록

## 현재 수정 기준 — 2026-09-11

- 현재 원문은 [prompts/agents/builder.md](../prompts/agents/builder.md)이며 최소 코드 규칙은 `Implementation` 절에 통합돼 있다. 규칙만 바꿀 때는 이 절을 수정하고 응답 분량·역할 전환 안내는 별도로 보존한다.
- Builder의 일반 진행·최종 응답은 공백·탭·줄바꿈 제외 200자 제한이며 선택·승인 설명·코드 블록과 명시적 상세 분석 요청에 예외가 있다. 현재 원문을 기준으로 하며 예전 '공통 지침의 제한' 설명은 적용하지 않는다.
- [src/agent.rs](../src/agent.rs)의 `render_turn_block`은 모든 역할에 매 턴 블록을 보낸다. 내장 역할은 다섯 개이며 별도 일회성 reset은 없다. 최신 구성과 계측 범위는 [지침 검증](지침-축약-검증.md)을 본다.

도입 당시(2026-09-04)의 파일별 변경 목록과 되돌리기 절차는 git 이력에서 확인한다. 원본 규칙은 [DietrichGebert/ponytail](https://github.com/dietrichgebert/ponytail)(MIT)의 `skills/ponytail/SKILL.md`이며, DevezVibe는 세 제공자에 같은 역할 텍스트를 보내야 해서 플러그인 훅 대신 역할 프롬프트에 규칙만 넣었다.

원본이 주장하는 코드량·비용 감소가 이 축약본에서도 나는지는 검증하지 않았다.
