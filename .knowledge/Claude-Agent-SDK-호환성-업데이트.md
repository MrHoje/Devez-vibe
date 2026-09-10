# Claude Agent SDK 호환성 업데이트 절차

## 목적

Devez Vibe는 `@anthropic-ai/claude-agent-sdk`가 함께 배포하는 Claude Code 실행 파일을
브리지(`npm/bridge/claude-agent-sdk-bridge.mjs`)로 감싸 쓰는 독립 UI다. Claude Code가
업데이트되면 SDK 타입·제어 요청·이벤트 변화가 Devez Vibe에 영향을 주는지 확인하고,
필요할 때만 코드를 최신화한다.

자동 CI 감시는 사용하지 않는다. 유지보수자는 대화에서 다음처럼 지시한다.

```text
claude code 최신버전 변경사항 중 devezVibe에 적용대상 점검해서 처리해
Claude Agent SDK 업데이트 영향 확인해
```

## 작업 순서

1. 고정 버전과 npm 최신 버전을 확인한다.

   ```powershell
   node -e "console.log(require('./npm/package.json').dependencies['@anthropic-ai/claude-agent-sdk'])"
   npm view @anthropic-ai/claude-agent-sdk version
   ```

2. 최신 SDK를 임시 폴더에 받아 타입 정의를 대조한다. 실제 계약 변화는 CHANGELOG보다
   `sdk.d.ts`에 먼저 드러난다.

   ```powershell
   npm pack @anthropic-ai/claude-agent-sdk@<버전>
   tar -xzf anthropic-ai-claude-agent-sdk-<버전>.tgz
   diff -u npm/node_modules/@anthropic-ai/claude-agent-sdk/sdk.d.ts package/sdk.d.ts
   ```

   사용자 눈에 보이는 변화는 `anthropics/claude-code`의 `CHANGELOG.md`로 함께 본다.

3. Devez Vibe에서 영향을 받을 지점을 대조한다.

   - `Query` 메서드(`setModel`·`applyFlagSettings`·`setPermissionMode`·`interrupt`·
     `mcpServerStatus`·`reconnectMcpServer`·`toggleMcpServer`)와 브리지 호출: `npm/bridge/claude-agent-sdk-bridge.mjs`
   - 모델 카탈로그와 브리지 기동: `src/claude.rs`
   - 런타임 분기와 turn 요청 조립: `src/backend.rs`
   - 이벤트 해석과 UI 상태: `src/state.rs`

4. 영향이 없으면 확인한 버전과 판단 근거를 이 문서의 `확인 기록`에 추가한다. 영향이
   있으면 코드를 수정하고 테스트를 추가한다.

5. 다음을 검증한다.

   ```powershell
   npm --prefix npm install
   node npm/bridge/claude-agent-sdk-bridge.mjs --self-test
   cargo test
   ```

6. 변경 사항과 검증 결과를 사용자에게 보고하고 확인 기록을 갱신한다.

## 신모델 반영

- Windows에서는 SDK 내장 Claude Code를 기본으로 사용하고, 설치된 `claude` 실행 파일이 더 새 버전일 때만 그쪽을 사용한다. 따라서 신모델은 먼저 SDK 버전 상향으로 반영 가능한지 확인한다.
- 실시간 모델 목록 조회가 실패해도 선택할 수 있도록 `src/claude.rs`의 예비 모델 목록을 함께 갱신한다.
- 새 모델 계열이면 브리지의 계열별 기능·표시명 정규화, 모델 검색 별칭, 렌더러 색상까지 함께 확인한다. 기존 계열이면 불필요한 분기를 추가하지 않는다.
- 단가가 기존 계열과 다르면 `.knowledge/토큰사용량-단가-갱신.md` 절차를 따른다.
- 검증 후 사용자는 전역 패키지를 갱신하고 `dvz`를 완전히 재시작해야 한다. 기존 세션은 기존 모델을 유지하므로 필요하면 `/model`에서 바꾼다.

## 판단 기준

- **반영 필요**: 브리지가 호출하는 `Query` 메서드·보내는 제어 요청·해석하는 메시지
  형태가 바뀜.
- **반영 불필요**: Devez Vibe가 쓰지 않는 기능(엔터프라이즈 정책·게이트웨이·원격
  제어·크로스 세션 수신 등)이 추가됐거나 기존 동작이 호환됨.
- **버전 상향만으로 충분**: 결함 수정이 CLI 내부에 있어 고정 버전을 올리면 그대로
  들어옴.

## 확인 기록

### 2026-09-10 SDK 0.3.267 반영 및 검수 — 미배포

- 사용자 승인 범위 중 SDK 상향과 사용량 조회 최적화를 반영했다. npm 의존성·잠금 파일·Windows 네이티브 번들은 모두 `0.3.267`이다. 제품 버전은 `1.8.17`을 유지하며, 잠금 파일에 남아 있던 `1.8.16`만 현재 제품 선언과 일치시켰다. 설치 스크립트는 실행하지 않았다.
- 브리지 `safeUsage()`는 `{ skipBehaviors: true }`를 전달한다. 요금제·사용 한도 응답은 그대로 유지하고 실패 시 기존처럼 `null`을 반환한다. 최근 대화 기록 스캔 결과인 `behaviors`는 제품 표시에서 사용하지 않는다.
- 자체 검사에 옵션 전달, 정상 응답 보존, 조회 실패 복구를 추가했다. Rust의 기존 SDK 버전 기대값 두 곳도 갱신했다. 독립 검수에서 제품 결함을 발견하지 않았고 검수자도 브리지 자체 검사를 실행해 통과했다.
- 실제 번들 검사: `claudePath`를 생략한 브리지에서 임시 작업 폴더를 사용했다. 번들 실행 버전은 `2.1.267`, SHA-256은 `23DDE2A47CF1D7D9C4A2D96D21FA80EA9BFC872DFDE0EE06E9982D2908603350`이며 manifest와 일치했다. 모델 조회·세션 시작·사용량 응답(`behaviors: null`)·기본 대화·기록 조회·종료 후 재개·시작 직후 중단·후속 대화가 통과했다. 중단 완료는 해당 실행에서 95 ms였다.
- 실제 `live_claude_auto_permission_mode`도 통과해 과거 bypass 요청과 재개에서도 기존 자동 모드를 유지함을 확인했다. Haiku를 사용한 첫 별도 실행은 기존 자동 승인 정책 미지원으로 거절됐고, 정책을 바꾸지 않고 기본 모델로 대화 흐름을 확인했다.
- 최종 통합 일반 시험은 1,276개 통과·13개 기본 제외이며, 제외된 실제 제공자 검사 중 Codex 10개와 Claude 1개를 별도 실행해 모두 통과했다. 별도 터미널 화면 검사 2개는 실행하지 않았다. Release 빌드와 `git diff --check`도 통과했다.
- 임시 재현 스크립트: `%TEMP%/dvz-claude-267-live.mjs`. 사용자 기존 세션은 변경하지 않았다. 대규모 기록 복원·프롬프트 캐시 내부 동작·다른 운영체제 실행과 사용량 조회 성능 수치는 이번 검증 범위에 포함하지 않았다.
- 배포 대기 노트에 반영했고 커밋·푸시·공개 배포는 수행하지 않았다. 아래 표는 구현 전 조사 당시의 판단 기록이다.

### 2026-09-10 최신 기능 점검 — 구현 전

- 기준 소스: `main`의 `c74b0f1`, Devez Vibe 1.8.17. `git pull --ff-only`로 갱신했고 시작 시 로컬 변경은 없었다.
- npm 최신 SDK는 `0.3.267`, Claude Code는 `2.1.267`이다. 설치된 CLI도 `2.1.267`이지만 프로젝트 의존성과 로컬 SDK는 `0.3.260`이다. 최신 SDK는 임시 폴더에 받아 `sdk.d.ts`를 대조했다. 이번 작업은 점검이며 의존성·제품 코드는 변경하지 않았다.
- 공식 근거: [Claude Code 변경 이력](https://code.claude.com/docs/en/changelog), [공식 저장소 변경 이력](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md), npm 패키지의 타입 정의. 공개 변경 기록이 있는 2.1.261·263·265·266·267을 2.1.260 이후 범위로 확인했다.

| 항목 | 판단 | 현재 구현과 필요한 조치 |
| --- | --- | --- |
| SDK 0.3.267 상향 | 적용 권장 | 2.1.261의 시작 직후 interrupt 무시, 2.1.265의 SDK 세션 종료 중 인증 갱신·턴 사이 작업 디렉터리 유지, 2.1.267의 5 MB 이상 대화 복원·도구/시스템 프롬프트 캐시 안정성 수정이 포함된다. 더 최신인 설치 `.exe`를 우선하는 기존 경로가 있어 일부 CLI 수정은 이미 적용될 수 있으나, 모든 설치 환경을 보장하려면 SDK 고정 버전도 올려야 한다. 실제 실행 중인 사용자 세션의 CLI 선택은 이번에 확인하지 않았다. |
| 사용량 조회의 `skipBehaviors` | 적용 권장 | `safeUsage()`가 세션 시작·결과 처리·계정 조회에서 인자 없이 호출한다. 새 `{ skipBehaviors: true }`는 최근 7일간 transcript 스캔을 생략한다. 제품 사용량 표시는 요금제·제한 데이터를 사용하므로 SDK 상향과 함께 적용할 후보이며, 조회 시간 감소량은 미측정이다. |
| `maxEffortLevel` | 조건부 검증 필요 | 최상위·모델별 추론 상한이 추가됐다. 현재 카탈로그는 `supportedEffortLevels`를 사용하고 세션의 effort는 요청값으로 보관한다. 제한된 계정/설정에서 실제 적용 수준과 UI가 일치하는지 확인해야 하며, 불일치는 아직 재현하지 않았다. |
| 재시도 원인과 대기 시간 | 선택 개선 | 새 `api_retry.no_response`에는 응답 헤더 대기·다음 재시도 대기 시간이 있다. 브리지는 현재 재시도 횟수만 표시하므로 장시간 무응답 이유를 보여 주려면 표시를 확장해야 한다. |
| 명령 출력 상한 설정 | SDK 갱신으로 활용 가능 | `bashOutputMaxChars`, `taskOutputMaxChars`가 추가됐다. 기존 Claude 설정 전달을 이용할 수 있으며 별도 Devez 설정 화면은 필수가 아니다. |
| 출력 스타일 재조회·플러그인 초기화 최적화 | 선택 기능 | `reloadOutputStyles()`, `pluginDelivery`, MCP handshake 캐시가 추가됐으나 기존 Query 호출을 깨는 필수 인자는 없다. 해당 관리 UI나 시작 시간 최적화가 필요할 때 도입한다. |
| 원격 제어·웹·VS Code 전용 화면 | 직접 적용 대상 아님 | Devez의 독립 터미널 UI에 자동으로 들어오는 기능과 구분한다. |

- 확인: 현재 0.3.260 기반 브리지 자체 검사 통과. Rust 일반 시험 1,268개 통과, 13개 제외. 최신 0.3.267로 의존성을 교체한 브리지·실제 모델 턴·시작/재개/중단 검증은 하지 않았다.
- 후속 순서: SDK 상향과 사용량 조회 최적화 → 상한 설정과 실제 모델 수준 대조 → 시작/재개/중단·도구·질문 검증. 점검만으로 배포 가능 판정을 내리지 않는다.

### 자동 승인 모드 고정

- 사용자 지시에 따라 Claude는 `auto`로 고정한다. 기존 `bypassPermissions` 우선 시도와 대체 모드 분기를 제거했고, `allowDangerouslySkipPermissions`도 설정하지 않는다.
- 초기화는 도구를 실행하지 않는 `default`에서 시작한 뒤 SDK의 `setPermissionMode("auto")` 성공을 확인하고 세션을 공개한다. 자동 모드가 정책상 거절되면 다른 모드로 몰래 전환하지 않고 오류를 반환한다.
- 새 대화와 재개 요청은 과거에 저장된 모드와 관계없이 자동 모드를 사용한다. 고정 모드 표시 및 `/status`도 자동 승인 검토로 맞췄다.
- 브리지 자체 검사와 실제 SDK 세션 시작·재개 후 `session/permissionMode` 응답의 `auto`를 확인했다. 지원하지 않는 모델이나 조직에서 자동 모드를 거절하는 경우는 우회하지 않는다.

| 날짜 | 확인 SDK / Claude Code 버전 | 결과 | 비고 |
| --- | --- | --- | --- |
| 2026-08-13 | 0.3.231 / 2.1.231 | 버전 상향 | 0.3.223 → 0.3.231. 브리지가 쓰는 `Query` 메서드 계약은 그대로다. 신규 타입(`OnElicitation`/`OnUserDialog`의 `requestId`·null 반환, `terminal_slash_commands`, `policyHelpers`, `dialogExpiry`, `crossSessionInbound`, plugin `command` 소스, AWS sigv4 정책)은 모두 Devez Vibe가 쓰지 않는 경로라 미적용. 상향으로 들어오는 수정: 공백뿐인 메시지의 400, Windows 확장 길이·UNC 경로 처리, 좁은 터미널·비문자열 도구 인자 크래시, `/model` 이후 이전 모델 되돌아감, 스트리밍 중 응답 일부 소실·중복. `set_model` 중간 전환은 Devez Vibe가 모델을 turn 시작에만 적용하는 설계라 미적용(steer는 진행 중 turn에 합류). |
