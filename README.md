# Devez CLI

Claude Code의 차분한 터미널 흐름을 참고해 새로 만든 Codex `app-server` 클라이언트입니다.
Codex의 인증, 하네스 프롬프트, 도구, 스킬, `AGENTS.md`, 샌드박스는 공식
`codex app-server`가 그대로 담당합니다. 이 프로젝트는 화면과 입력 계층만 소유합니다.

## 현재 범위

- 새 스레드 시작, 세션 검색 피커, ID/이름 기반 resume
- `/new` 성공 시 이전 대화와 화면을 비우고 새 세션으로 전환
- `--resume [SESSION]`, `--continue` 및 실행 중 `/resume`
- 모델 카탈로그 기반 `/model` 선택과 좌우 effort 조절
- `/effort` 슬라이더에서 서버 지원 effort만 노출 (`max`·`ultra` 포함)
- 응답, reasoning summary, 명령, 파일 변경, MCP 호출 스트리밍
- 명령/파일 변경 승인
- 실행 중 입력 steer 및 `Esc`/`Ctrl+C` 중단
- 일반 터미널 스크롤백을 보존하는 증분 렌더링
- 좌우 테두리와 복사용 공백이 없는 Claude Code형 하단 composer
- Git 브랜치, 모델, effort, context, 5h/주간 한도, Fast 상태를 표시하는 하단 상태줄
- 모델별 실제 유효 context window를 첫 입력 전부터 표시
- 시작과 `/new` 세션 전환 시 터미널을 비우고 새 화면으로 전환
- Codex CLI 정렬을 유지하고 hidden 모델을 제외한 `/model` 피커
- `/model sol`, `/model terra`, `/model luna`, `/model spark` 등 짧은 모델 별칭
- 모델 번호 표시 및 피커에서 `1`~`9` 숫자키 즉시 선택
- 설명·Auto 없이 모델명과 지원 수준만 표시하는 `/effort` 슬라이더
- `You`/`Codex` 헤더 없이 마커와 본문으로 이어지는 대화 출력
- 보낸 프롬프트는 전체 행에 은은한 Claude형 배경색 적용
- Sol·Terra·Luna·GPT-5.5 모델 색상을 picker와 statusline에 공통 적용
- `/` 명령 자동완성 및 키보드 모델 선택기
- `Ctrl+Backspace`/`Ctrl+W` 단어 삭제, `Ctrl+K`/`Ctrl+U` 줄 삭제,
  `Ctrl+Y` 복원, `Alt+B`/`Alt+F` 단어 이동, `Ctrl+J` 줄바꿈
- Markdown 제목·목록·인용·코드 블록 표현
- 실행 시간, 파일 diff 통계, 진행 상태 표시
- 활성 영역 전체 삭제 없이 변경된 터미널 행만 갱신

## 실행

Codex CLI가 설치되고 로그인된 환경에서:

```powershell
cargo run --release
```

주요 옵션:

```text
devez [--resume [SESSION] | --continue] [--model MODEL] [--effort EFFORT]
      [--cwd PATH] [--codex PATH]
```

`--resume`만 입력하면 검색 가능한 세션 피커를 열고, `--continue`는 현재 폴더의
가장 최근 세션을 바로 이어갑니다. 실행 중에는 `/resume [SESSION]` 또는 별칭
`/continue`로 세션을 전환할 수 있습니다. 입력창의 전체 명령은 `/help`에서 확인합니다.

## 경계

`app-server` 프로토콜은 Codex 버전에 따라 변할 수 있습니다. 렌더러 변경은 독립적으로
관리하고, 업스트림에서는 app-server 메서드/스키마/인증/모델 카탈로그 변경만 호환성
대상으로 봅니다.
