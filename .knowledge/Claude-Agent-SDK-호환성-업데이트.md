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

| 날짜 | 확인 SDK / Claude Code 버전 | 결과 | 비고 |
| --- | --- | --- | --- |
| 2026-08-13 | 0.3.231 / 2.1.231 | 버전 상향 | 0.3.223 → 0.3.231. 브리지가 쓰는 `Query` 메서드 계약은 그대로다. 신규 타입(`OnElicitation`/`OnUserDialog`의 `requestId`·null 반환, `terminal_slash_commands`, `policyHelpers`, `dialogExpiry`, `crossSessionInbound`, plugin `command` 소스, AWS sigv4 정책)은 모두 Devez Vibe가 쓰지 않는 경로라 미적용. 상향으로 들어오는 수정: 공백뿐인 메시지의 400, Windows 확장 길이·UNC 경로 처리, 좁은 터미널·비문자열 도구 인자 크래시, `/model` 이후 이전 모델 되돌아감, 스트리밍 중 응답 일부 소실·중복. `set_model` 중간 전환은 Devez Vibe가 모델을 turn 시작에만 적용하는 설계라 미적용(steer는 진행 중 turn에 합류). |
