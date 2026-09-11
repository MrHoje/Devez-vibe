# 지식 문서 전체 점검 — 2026-09-11

## 범위와 기준

기준 소스는 `ac99985`다. 기존 `.knowledge` 주제 문서 13개와 색인, `docs`에 남아 있던 지식 Markdown 2개를 대상으로 구조·검색 설명·로컬 링크·현재 구현과의 충돌을 점검했다. 날짜별 전체 배포나 외부 서비스 상태를 재현한 작업은 아니다. 문서 이관 후 주제 문서 15개에 자동 지식 안내 1개를 추가해 색인 외 16개가 됐다.

현재 설명은 관련 소스와 교차 확인하고, 초기 설계·과거 실험은 원문을 보존한 채 적용 범위를 표시했다. README·CLAUDE의 로컬 문서 링크도 검사했다. 실행 코드·모델 단가·프롬프트·제품 버전은 변경하지 않았다.

## 문서별 조치

| 문서 | 확인한 문제와 조치 |
| --- | --- |
| knowledge-index.md | 본문 주요 증상이 색인에 없던 문제를 보완. 현재 안내와 과거 설계를 구분하고 본문 검색 보완 절차 추가 |
| Claude-Agent-SDK-호환성-업데이트.md | 당시 미배포와 후속 1.8.18·1.8.22 배포 기록 연결. PowerShell의 `diff` 별칭 혼동을 피하는 명령과 관리 실행 버전 전환 안내 |
| Codex-CLI-호환성-업데이트.md | 초기 제외 범위·당시 최신 버전과 현재 요청을 구분. 후속 배포 및 실제 실행 경로 확인 위치 연결 |
| 배포-버전-갱신.md | 다른 브랜치에 무조건 main 병합하는 안내 제거. 잠금 파일·공개 해시·관리 실행 버전 확인 보완. 개인 PC 경로·별칭 가정 제거 |
| 토큰사용량-단가-갱신.md | 현재 미표시인 비용 배지 설명 수정. 모델별 원장·Fable 캐시 읽기 예외 반영. 6배 오산을 코드 기준 25배로 정정. 과거 요금을 현재 공식 가격으로 제시하지 않도록 정리 |
| 컨텍스트-표시-복원.md | 현재 소스·브리지와 비용 원장 문서 연결. 기존 복원 원칙 보존 |
| 스트리밍-링크-지연.md | 렌더러의 링크 범위와 상태의 TextPace 위치 구분. 출력 지연과 제공자 대기 구분 |
| 한글-글리프-깨짐-진단.md | '원인은 하나'라는 일반화 제거. 재현 범위·입력 문제 구분. 독립 터미널과 DevezCode 폭 프로필 구분 |
| Windows-호스트-통합-주의.md | 미응답 질문의 완료 표시·영문 덧붙음·빈 상태줄을 증상별로 안내. 당시 상태와 후속 배포 구분 |
| 에이전트-역할-확장-주의.md | 현재 목록의 소스 연결과 이관된 사용자 정의 역할 문서 링크 갱신 |
| custom-agent-roles.md | docs에서 지식 폴더로 이관. Reviewer 표시·Tab 순서·매 턴 역할 전달 정정. 개인 배치 파일을 과거 도구로 구분 |
| builder-ponytail.md | 현재 Implementation 절·200자 예외·다섯 역할·매 턴 전달 기준 추가. 오래된 제거 절차·토큰 수는 과거 기록으로 분리 |
| 지침-축약-검증.md | 현재 Planner 문서 분리와 다른 역할 통합 구조 확인. 오래된 제한 폐지 설명이 현재 규칙과 충돌하지 않게 정정 |
| agent-system-implementation-plan.md | 초기 Advisor·Finisher·Research 제외·일회성 reset을 현행으로 오인하지 않도록 과거 설계 표시 및 현재 문서 연결 |
| side-panel-reference.md | 삭제된 과거 정보 패널과 현재 설정 사이드패널 구분 |
| 2026-09-05-supervibe-response-all-prompt-toggle.md | docs/plans에서 지식 폴더로 이관. 본문을 보존하고 당시 계획이 새 실행 승인이나 현재 미완료 작업이 아님을 명시 |
| 자동-지식-기록과-검색.md | 새 문서. 읽기/쓰기 분리, 프로젝트별 상태 저장·실패, 제공자 지침 전달, 실제 LLM 행동과 호스트 검사의 차이 정리 |

## 검증 결과

- 고정 질문 10개의 색인 후보 포함 수는 7/10에서 10/10으로 바뀌었다. 같은 알려진 질문을 바탕으로 색인을 보완했으므로 새로운 질문에 대한 일반 성능 향상의 증거가 아니다.
- 본문은 두 번 모두 10/10의 기대 문서를 후보에 포함했다. 문서 수가 13개에서 16개로 늘었고 정비 설명에도 용어가 추가돼 본문 후보 수가 2~8개에서 3~11개로 늘었다. 검색 정밀도가 좋아졌다고 주장하지 않는다.
- 비용 계산 6개, 비용 미표시 1개, auto knowledge 11개, 역할 12개로 총 30개 기존 검사가 통과했다. 역할 입력 추출 시험 1개는 기본 제외이며 실제 LLM을 호출하지 않았다. 기존 링커 안내가 남는다.
- `check_documents.py`는 색인 중복·누락·제목 일치와 로컬 Markdown 파일 링크를 확인한다. 외부 URL 응답, 문서 내부 앵커, 과거 개인 임시 파일의 존재는 검사하지 않는다.
- `git diff --check`로 서식을 확인한다. 공개 배포·설치 갱신·실제 앱 화면 검증은 하지 않는다.

## 재현

```powershell
python -X utf8 artifacts/knowledge-search-evaluation/check_documents.py
powershell -NoProfile -ExecutionPolicy Bypass -File artifacts/knowledge-search-evaluation/evaluate.ps1
cargo test pricing::tests
cargo test estimated_cost_is_not_shown_above_the_composer
cargo test auto_knowledge
cargo test agent::tests
git diff --check
```

검색 원본은 `results.json`, 정비 후 결과는 `results-current.json`이다. 장기 대화에서 실제 모델이 올바른 문서·시점을 선택하는지는 별도 시험이 필요하다.
