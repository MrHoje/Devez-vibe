# Codex CLI 호환성 점검

DevezCLI는 Codex `app-server` 스키마를 기준 스냅샷과 비교해 업스트림 변경을 감지합니다.

## 실행

```powershell
# 승인된 기준 버전이 재현되는지 확인
node scripts/check-codex-compatibility.mjs --mode baseline

# npm 최신 Codex CLI와 비교
node scripts/check-codex-compatibility.mjs --mode latest

# 검토를 마친 최신 스키마를 새 기준으로 저장
node scripts/check-codex-compatibility.mjs --mode baseline --update-baseline
```

결과는 `artifacts/codex-compatibility/compatibility-report.md`에, 변경된 스키마는
`schema.ts`와 `schema.diff`에 남습니다. GitHub Actions 화면에서 **Codex CLI
compatibility**를 열고 **Run workflow**를 눌러 원하는 검사 모드를 선택해 실행합니다.

## 변경이 발견됐을 때

1. `schema.diff`에서 바뀐 메서드·이벤트·타입을 확인합니다.
2. `src/app_server.rs`, `src/main.rs`, `src/state.rs`, `src/integrations.rs`의 JSON-RPC
   처리와 대조합니다.
3. 영향이 없으면 결과를 **호환**으로 분류하고 기준 스냅샷을 갱신합니다.
4. 영향이 있으면 **코드 수정 필요**로 분류해 구현·테스트 후 다시 검사합니다.
5. 검증된 새 버전을 채택할 때만 **기준 스냅샷 갱신 필요**를 완료 처리합니다.

코드 변경 후에는 `cargo test`와 `initialize`, 모델 목록 조회, 새 스레드 시작 smoke test를
실행합니다.
