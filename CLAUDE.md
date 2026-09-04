# Devez Vibe 작업 지침

## 목차

1. [작업 원칙](#%EC%9E%91%EC%97%85-%EC%9B%90%EC%B9%99)
2. [계획과 실행](#%EA%B3%84%ED%9A%8D%EA%B3%BC-%EC%8B%A4%ED%96%89)
3. [리소스 관리](#%EB%A6%AC%EC%86%8C%EC%8A%A4-%EA%B4%80%EB%A6%AC)
4. [기록](#%EA%B8%B0%EB%A1%9D)
5. [지식베이스](#%EC%A7%80%EC%8B%9D%EB%B2%A0%EC%9D%B4%EC%8A%A4)

## 작업 원칙

* 단순함을 우선한다. 필요한 일만 수행한다.
* 기존의 사용자 변경과 무관한 파일은 수정하거나 되돌리지 않는다.
* DevezVibe의 프롬프트 주입 지침을 수정할 때는 `src/main.rs`의 `DEVEZ_INSTRUCTIONS` 정의를 참고해 변경한다.
* 이 문서에서 `시스템 프롬프트` 또는 `DevezVibe 지침`은 `src/main.rs`에서 제공자별로 DevezVibe가 주입하는 지침을 뜻한다.

## 계획과 실행

* 설명과 구현은 가장 단순한 접근을 우선하며, 불필요한 추상화를 만들지 않는다.

## 리소스 관리

* 서브에이전트는 병렬 작업 또는 큰 컨텍스트가 필요한 경우에만 사용한다.
* 문서를 전달할 때는 필요한 내용을 요약하고 전체 내용을 무분별하게 전달하지 않는다.
* 응답은 짧고 구조화한다. 긴 설명은 필요할 때만 제공한다.

## 기록

* 반복될 가능성이 큰 실수나 중요한 교훈만 `.Codex.md`에 기록한다.

## 지식베이스

* 절차, 설계 근거, 진단 기록 같은 지식 문서는 `.knowledge/` 폴더에 적재한다. `docs/`에는 HTML 미리보기 같은 산출물만 둔다.
* 새 지식 문서를 추가하거나 이름을 바꾸면 아래 목록도 함께 갱신한다.
* 문서 목록
  * `Claude-Agent-SDK-호환성-업데이트.md` — Claude Code·SDK 업데이트 시 브리지 영향 확인과 최신화 절차
  * `Codex-CLI-호환성-업데이트.md` — Codex CLI 업데이트 시 app-server 프로토콜·모델 카탈로그 영향 확인 절차
  * `배포-버전-갱신.md` — Cargo.toml 버전과 웰컴 UI 표기, npm 패키지 배포 절차
  * `토큰사용량-단가-갱신.md` — 컴포저 추정 비용의 모델별 단가표 갱신 절차, DevezCode와 동일 단가 유지
  * `한글-글리프-깨짐-진단.md` — 전각 글리프가 다른 글자로 보이는 증상의 원인과 수정 방법
  * `agent-system-implementation-plan.md` — Builder·Planner·Goal Runner·Reviewer 역할 시스템의 초기 구현 계획서
  * `side-panel-reference.md` — 제거된 우측 도킹 정보 패널의 예전 구현 참고 자료
  * `builder-ponytail.md` — Builder 역할에 넣은 Ponytail 최소 코드 규칙의 적용 내용과 제거 절차
