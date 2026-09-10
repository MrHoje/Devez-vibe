## Design gate and decomposition

Architectural work only, after confirmed requirements: present the goal, two or three viable approaches and trade-offs (recommended first), and architecture/components/interfaces/data flow/error handling/testing at useful depth. Remove unneeded features. If only one approach works, explain why. Ask for design approval and stop; revisions require renewed approval before plan tasks. Record approved design and rejected alternatives.

Split independent subsystems into independently verifiable plans; identify the current one and follow-ups. Map created/changed files and responsibilities before tasks, following existing layout. Each task is an independently verifiable deliverable worth a review gate. Combine setup/config/docs with their deliverable and files that change together. Do not split shared acceptance surfaces merely to increase task count.

## Plan document

Write in Korean; keep technical identifiers, exact values, code and commands verbatim. Use the following sections; mark an inapplicable section with its reason rather than invent content.

```markdown
# <기능명> 구현 계획

**분류:** 기존 기능 개선 / 구조 설계
**목표:** <관찰 가능한 완료 상태>
**접근:** <구조와 이유>
**의도 차이:** <요청 밖 추가·변경·제외와 승인 근거, 없으면 없음>

## 결정 기록
- 결정, 주요 결정 요인, 대안과 기각 이유, 결과·후속 조치, 전제

## 요구사항 인터뷰 기록
- 요약 승인: <최신 요약과 사용자 응답 근거; 실행 승인이 아님>
| 항목 | 상태 | 결정과 근거 | 관찰 가능한 완료 기준 |
| --- | --- | --- | --- |
| <항목> | 확정 / 선택 위임 / 해당 없음 | <응답 또는 제외 근거> | <검사 또는 해당 없음의 이유> |
- 미확정: 없음. 있으면 작성 전에 인터뷰를 계속한다.

## 범위
- 포함 / 제외

## 전역 제약
- 버전, 의존성, 명명, 플랫폼 등 정확한 제약값

## 변경 파일 지도
| 파일 | 작업 | 책임 |
| --- | --- | --- |
| <정확한 경로> | 생성 / 수정 / 테스트 | <책임> |

## 태스크
### 태스크 N: <독립된 검증 단위>
**의존:** <선행 번호 또는 없음>
**파일:** <생성 경로, 수정 경로와 현재 줄 범위, 검사 경로>
**인터페이스:** <소비·제공하는 정확한 시그니처>
**완료 조건:** <실패할 수 있는 관찰 기준>
- [ ] 1단계: 실패하는 테스트 작성 — <코드>
- [ ] 2단계: 실패 확인 — <명령, 동작 누락으로 실패하는 기대 결과>
- [ ] 3단계: 최소 구현 — <코드>
- [ ] 4단계: 통과 확인 — <명령과 기대 결과>

## 최종 검증
- 전체 검사·빌드·실제 표면 확인의 명령과 기대 결과

## 위험과 완화
- 실패 가능성과 감지·완화 방법, 다루지 않는 위험

## 실행 중단 기준
- 이 작업에 해당하는 권한·파괴적 작업·범위 결정·실행 불가능 조건

## 검토 기록
| 회차 | 검토 방식 | 아키텍처 | 실행 가능성 | 요구 변경 | 반영 |
| --- | --- | --- | --- | --- | --- |
| 1 | 독립 검토 / 자체 검토 | CLEAR / WATCH / BLOCK | OKAY / ITERATE / REJECT | <내용> | <내용> |

## 의도 조정
- 검토 후 재확인한 결정과 근거; 보류는 미확정, 없으면 없음

## 실행 기록
<Goal Runner가 채운다>
```

With an applicable harness use the test-first sequence: failure must demonstrate missing behavior, not setup errors. Without one replace the first two steps with explicit verification to run after implementation. Every code step contains code; every run step contains command and expected result. Steps are small executable actions, not vague instructions.
Tests derive expectations independently and detect real broken behavior, not mock presence or incidental wording/constants. Explain unexpected output rather than treating harmless existing warnings as failures.
No TODO/TBD, “handle errors/edges,” missing test code, “same as task N,” undefined interfaces, or unexplained placeholders. Repeat necessary task-local details because tasks may be read separately.

High-risk work (auth/security/payments/destructive data/migration/concurrency/personal data/public API/production infrastructure) includes three concrete post-release failure scenarios with detection, and distinct unit/integration/end-to-end/observability coverage.
