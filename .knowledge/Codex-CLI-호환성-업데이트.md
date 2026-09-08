# Codex CLI 호환성 업데이트 절차

## 목적

DevezCLI는 공식 `codex app-server`를 사용하는 독립 UI다. Codex CLI가 업데이트되면
프로토콜·모델 카탈로그·이벤트 변화가 DevezCLI에 영향을 주는지 확인하고, 필요할 때만
코드를 최신화한다.

자동 CI 감시는 사용하지 않는다. 유지보수자는 대화에서 다음처럼 지시한다.

```text
최신 Codex CLI 확인해서 DevezCLI에 반영해
Codex 업데이트 영향 확인해
Codex 0.x.y 기준으로 호환성 업데이트해
```

## 작업 순서

1. 현재 설치 버전과 npm 최신 버전을 확인한다.

   ```powershell
   codex --version
   npm view @openai/codex version
   ```

2. 최신 Codex의 공개 변경 사항과 app-server 스키마를 확인한다.

   ```powershell
   codex app-server generate-ts --experimental --out <임시-폴더>
   ```

   최신 버전이 설치되지 않았다면 `npm exec --yes --package=@openai/codex@<버전> -- codex`
   로 실행한다.

3. DevezCLI에서 영향을 받을 지점을 대조한다.

   - JSON-RPC 연결·초기화: `src/app_server.rs`
   - 스레드·모델·승인 요청: `src/main.rs`
   - 이벤트 해석과 UI 상태: `src/state.rs`
   - Plugin·MCP·Marketplace: `src/integrations.rs`
   - 세션 재개 형식: `src/rollout.rs`

4. 영향이 없으면 확인한 Codex 버전과 판단 근거를 이 문서의 `확인 기록`에 추가한다.
   영향이 있으면 필요한 코드를 수정하고 관련 테스트를 추가한다.

5. 다음을 검증한다.

   ```powershell
   cargo test
   ```

   가능하면 실제 최신 Codex로 `initialize`, 모델 목록 조회, 새 스레드 시작을 확인한다.

6. 변경 사항과 검증 결과를 사용자에게 간단히 보고한다. 이 문서의 확인 기록도 갱신한다.

## 신모델 반영

- Devez Vibe는 `PATH`의 `codex`를 실행하므로 CLI 갱신만으로 모델 목록이 나타나는지 먼저 확인한다. 목록 형식이 그대로라면 Devez Vibe 배포는 필요 없다.
- 비용 표시는 별도이므로 `.knowledge/토큰사용량-단가-갱신.md` 절차에 따라 구체적인 티어를 일반 계열보다 먼저 배치한다.
- 짧은 모델 검색어가 필요하면 모델 별칭을, 새 계열의 시각 구분이 필요하면 렌더러 색상을 함께 갱신한다.
- 릴리스 노트의 기능 항목뿐 아니라 기본값 변경도 확인한다. 선택 활성화로 바뀐 도구는 app-server 실행 설정에서 명시적으로 켜지 않으면 UI 기능이 조용히 사라질 수 있다.

## 판단 기준

- **반영 필요**: DevezCLI가 호출하는 메서드·보내는 파라미터·해석하는 이벤트·모델 목록
  형식이 변경됨.
- **반영 불필요**: UI가 사용하지 않는 새 기능이 추가됐거나 기존 동작이 호환됨.
- **주의 필요**: app-server 스키마는 바뀌지 않았지만 Codex CLI의 사용자 경험·명령 의미가
  달라져 DevezCLI의 Codex 정렬 목표에 영향을 줌.

## 확인 기록

### 선택 질문의 응답 대기

- Codex 0.153.4의 기본 질문은 `features.default_mode_request_user_input=true`로 활성화한다. 매 `turn/start`에 `collaborationMode.mode: default`를 지정하고 질문 전용 지침을 함께 전달한다. 기존 1.8.4 대화에 남은 계획 모드도 여기서 되돌린다. 선택 모델·추론 수준·역할별 쓰기 권한은 유지하고 `turn/steer`는 현재 설정을 바꾸지 않는다.
- **1.8.4 판단 정정:** `isBlocking: false`를 모델이 답변 전에 실행을 계속한다는 뜻으로 해석했으나, 이는 화면 표시 방식의 힌트다. 기본 질문 RPC는 일반 모드에서도 사용자의 응답을 보낼 때까지 도구 결과를 반환하지 않는다. 계획 모드를 강제하면 `update_plan`이 금지되는 별도의 문제가 있으므로 그 방식을 사용하지 않는다.
- Astra의 `request_user_input_async`는 별개다. `agentMessage`의 `delivery: async`와 `questions`로 전달되며 이를 기본 질문 RPC와 혼동하지 않는다. 비동기 질문이나 사용자 답변 없는 질문 해제 알림이 들어오면 작업을 중단한다.
- 질문은 시간 경과로 자동 응답하지 않는다. Codex 취소는 빈 답변을 보내지 않고 턴을 중단한다. 일부 질문의 답만 제출하지 않으며, 밀린 화면 상태를 먼저 반영하여 취소 대상 턴을 맞춘다. 질문 대기 중 오래된 상태 조회 결과로 대기를 끝내지 않는다.
- 확인 키를 누르고 있을 때 다음 질문까지 자동 선택되지 않게 한다. Ctrl+C는 질문을 취소하고, Ctrl+Enter·Shift+Enter·Alt+Enter가 답변을 제출하지 않게 한다. 질문의 직접 입력은 기존 단일 줄 표시를 유지한다.
- 다음 Codex 갱신 때 다음 시험을 명시적으로 실행한다. 실제 모델과 로그인 상태를 사용한다.
  - `cargo test live_codex_question_catalog_matrix -- --ignored --nocapture`: 모든 페이지의 모델 목록에서 GPT-5.6·GPT-6 계열과 지원 추론 수준을 열거하고 작업 목록·질문 대기·마우스 선택·답변 전달을 검사한다.
  - `cargo test live_codex_question_ -- --ignored --nocapture --skip catalog_matrix --skip disconnect --test-threads=4`: 네 모델의 90초 무응답, 직접 입력 후 파일 생성, 취소 후 파일 미생성, 새 작업 재개와 이전 계획 모드 대화의 재접속을 검사한다.
- 연결 종료 전용 시험은 `cargo test live_codex_question_disconnect -- --ignored --nocapture`로 별도 실행한다.
- 1.8.4의 기본 시험 1,201개와 네 모델의 30초 대기는 통과했지만, 작업 목록 도구와 함께 쓰는 조합은 빠져 있었다. 이후 검증에서는 도구 조합과 모든 지원 추론 수준을 포함한다.

### 2026-09-09 재검증 결과

| 모델 | 검증한 추론 수준 | 결과 |
| --- | --- | --- |
| gpt-5.6-luna | low, medium, high, xhigh, max | 5개 조합 통과 |
| gpt-5.6-sol | low, medium, high, xhigh, max, ultra | 6개 조합 통과 |
| gpt-5.6-terra | low, medium, high, xhigh, max, ultra | 6개 조합 통과 |
| gpt-6-astra | low, medium, high, xhigh, max, ultra | 6개 조합 통과 |

- 설치된 0.153.4의 숨겨진 모델을 포함한 전체 목록을 확인했다. 대상은 네 모델·23개 조합이며, 작업 목록 갱신·10초 무응답 대기·마우스 선택·답변 전달을 모두 통과했다.
- 별도로 네 모델에서 90초 무응답 중 실행 없음, 직접 입력 후 파일 생성, 30초 후 취소와 새 작업 재개를 통과했다. Esc·Ctrl+C·번호·마우스로 질문을 취소했으며 취소한 작업의 파일은 생성되지 않았다.
- 이전 계획 모드 대화를 실제로 저장·종료·재접속한 후 작업 목록과 질문이 모두 작동했다. 네 모델 모두 답변 대기 중 연결을 종료해도 파일을 생성하지 않았다.
- 재검증 중 계획 모드의 작업 목록 도구 금지, Ctrl+C 무반응, 확인 키 반복과 보조 키 조합의 오제출, 연결 종료 후 질문 잔류를 수정했다. 서버가 이미 끝낸 질문에 중단 요청을 다시 보내지 않고, 주 대화와 곁가지 대화의 불가능한 입력을 함께 정리한다. 연결 종료로 전송하지 못한 답변과 작성 중인 내용은 기존 입력창 초안과 함께 보관한다.
- 일반 시험 1,208개를 통과했다. 업데이트 노트는 1.8.4와 같은 두 항목을 유지한다.

| 날짜 | 확인 Codex 버전 | 결과 | 비고 |
| --- | --- | --- | --- |
| 2026-08-13 | 0.147.0 | 반영 완료 | MCP 2026-07-28 프로토콜이 `features.mcp_2026_07_28` opt-in으로 추가돼 app-server 실행 시 `-c`로 켠다(사용자 config 선언이 있으면 존중). 이 opt-in은 Codex가 개발 중 기능 경고를 출력하게 하므로 같은 실행에 `suppress_unstable_features_warning=true`도 함께 넘긴다(사용자 config 선언이 있으면 존중). `initialize` capabilities는 `extensions`에 `openai/form`을 선언하도록 바뀌어 legacy alias와 함께 보낸다. `mcpServerStatus/list`의 `nextCursor`는 `limit: 100` 단일 조회로 계속 충분해 미적용. |
| 2026-07-29 | 0.146.0 | 호환 유지, 기능 반영 후보 확인 | `app-server generate-ts --experimental` 스키마와 현재 요청 경로를 대조했다. 세션 이름·고정과 유지형 사이드 대화는 적용 후보이며, Plugin 발행 기능은 필요 시 적용한다. |
| 2026-07-26 | 0.145.0 | 기준 설정 | DevezCLI 현 구현 기준 |
