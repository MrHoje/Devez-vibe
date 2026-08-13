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

## 판단 기준

- **반영 필요**: DevezCLI가 호출하는 메서드·보내는 파라미터·해석하는 이벤트·모델 목록
  형식이 변경됨.
- **반영 불필요**: UI가 사용하지 않는 새 기능이 추가됐거나 기존 동작이 호환됨.
- **주의 필요**: app-server 스키마는 바뀌지 않았지만 Codex CLI의 사용자 경험·명령 의미가
  달라져 DevezCLI의 Codex 정렬 목표에 영향을 줌.

## 확인 기록

| 날짜 | 확인 Codex 버전 | 결과 | 비고 |
| --- | --- | --- | --- |
| 2026-08-13 | 0.147.0 | 반영 완료 | MCP 2026-07-28 프로토콜이 `features.mcp_2026_07_28` opt-in으로 추가돼 app-server 실행 시 `-c`로 켠다(사용자 config 선언이 있으면 존중). `initialize` capabilities는 `extensions`에 `openai/form`을 선언하도록 바뀌어 legacy alias와 함께 보낸다. `mcpServerStatus/list`의 `nextCursor`는 `limit: 100` 단일 조회로 계속 충분해 미적용. |
| 2026-07-29 | 0.146.0 | 호환 유지, 기능 반영 후보 확인 | `app-server generate-ts --experimental` 스키마와 현재 요청 경로를 대조했다. 세션 이름·고정과 유지형 사이드 대화는 적용 후보이며, Plugin 발행 기능은 필요 시 적용한다. |
| 2026-07-26 | 0.145.0 | 기준 설정 | DevezCLI 현 구현 기준 |
