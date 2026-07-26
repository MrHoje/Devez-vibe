# Codex CLI 호환성 자동화 설계

## 목적

Codex CLI의 `app-server` 프로토콜 변경을 조기에 감지하고, DevezCLI가 검증한 기준
버전과 최신 Codex 간의 호환성 차이를 재현 가능한 CI 결과로 남긴다.

## 범위

- Codex CLI 버전과 `app-server` TypeScript 스키마의 기준 스냅샷을 저장한다.
- PR에서는 고정된 기준 버전으로 스키마 생성과 비교를 실행한다.
- 매주 및 수동 실행에서는 npm의 최신 Codex CLI로 같은 검사를 실행한다.
- 스키마가 달라지면 Markdown 요약과 전체 diff를 artifact로 보관하고 작업을 실패시킨다.
- 변경 검토자는 호환 여부를 분류한 뒤, 필요할 때만 기준 스냅샷과 버전을 갱신한다.

Codex의 출시 알림을 별도로 구독하거나 DevezCLI 런타임에서 업데이트를 자동 설치하는
기능은 이 범위에 포함하지 않는다.

## 기준 자료

`compatibility/codex-app-server/`는 다음 파일을 소유한다.

- `baseline.json`: 검증한 Codex CLI 버전, 스냅샷 생성 명령, 갱신 날짜
- `schema.ts`: 해당 버전의 `codex app-server generate-ts --experimental` 출력

스키마 생성은 CI와 개발자 로컬에서 같은 명령을 사용한다. 스냅샷은 손으로 수정하지
않고, 검토를 마친 생성 결과만 커밋한다.

## CI 워크플로

`.github/workflows/codex-compatibility.yml`은 다음 두 검사 경로를 가진다.

### 기준 검증: pull request와 수동 실행

1. `baseline.json`의 Codex 버전을 설치한다.
2. `codex app-server generate-ts --experimental --out <임시 디렉터리>`를 실행한다.
3. 생성한 `schema.ts`를 저장된 기준 스냅샷과 비교한다.
4. 같으면 성공한다. 다르면 `compatibility-report.md`와 unified diff를 artifact로 올리고
   실패한다.

이 검사는 의도한 기준 갱신 PR에도 동작한다. 스냅샷과 메타데이터를 함께 갱신하면 새
버전을 기준으로 다시 통과한다.

### 업스트림 감시: 매주 및 수동 실행

1. npm에서 최신 `@openai/codex`를 설치한다.
2. 설치된 `codex --version`과 기준 버전을 기록한다.
3. 같은 스키마 생성·비교를 실행한다.
4. 차이가 없으면 성공한다. 차이가 있으면 리포트·diff artifact를 남기고 실패한다.

예약 실행은 매주 월요일 09:00 UTC에 수행한다. 수동 실행은 기준 검증 또는 업스트림
감시 중 하나를 선택할 수 있어야 한다.

## 리포트 계약

`compatibility-report.md`에는 반드시 다음을 포함한다.

- 검사 모드(`baseline` 또는 `latest`)
- 설치된 Codex CLI 버전과 기준 버전
- 스키마 비교 결과와 diff artifact 이름
- 후속 조치: `호환`, `코드 수정 필요`, `기준 스냅샷 갱신 필요` 중 하나
- 실패 시 로컬 재현 명령

스키마 차이만으로 DevezCLI의 기능 장애를 단정하지 않는다. 리포트는 호환성 검토를
시작시키는 신호다.

## 변경 대응 절차

1. CI artifact에서 변경된 메서드, 타입, 이벤트를 확인한다.
2. DevezCLI의 JSON-RPC 사용 지점(`src/app_server.rs`, `src/main.rs`, `src/state.rs`,
   `src/integrations.rs`)과 대조한다.
3. 영향이 없으면 최신 스키마와 `baseline.json` 버전을 갱신하는 PR을 만든다.
4. 영향이 있으면 코드를 수정하고 관련 테스트를 추가한 뒤, 설치된 최신 CLI로
   `initialize`, 모델 목록 조회, 스레드 시작의 smoke test를 수행한다.
5. 수정과 검증이 끝난 PR에서 기준 스냅샷을 갱신한다.

## 실패 원칙

- Codex 설치 또는 스키마 생성 실패는 CI 실패다. 이전 스냅샷을 성공으로 간주하지 않는다.
- artifact 업로드는 실패 이후에도 실행돼야 한다.
- npm의 일시적 오류는 한 번 재시도하고, 다시 실패하면 원인과 재현 명령을 리포트에 남긴다.
- 스키마 diff는 보안 비밀값을 포함하지 않는 생성 산출물만 저장한다.

## 완료 기준

- PR마다 기준 버전 스키마의 재현성을 검증한다.
- 매주 최신 Codex 변경을 탐지한다.
- 실패한 검사에서 검토자가 바로 사용할 Markdown 리포트와 diff artifact를 얻는다.
- 기준 갱신과 코드 대응의 책임 경계가 문서화된다.
