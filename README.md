# Devez Vibe

[![npm](https://img.shields.io/npm/v/devez-vibe)](https://www.npmjs.com/package/devez-vibe)
[![license](https://img.shields.io/npm/l/devez-vibe)](LICENSE)

공식 Codex `app-server`와 Claude Agent SDK를 사용하는 터미널 클라이언트입니다.

Codex는 공식 `app-server`, Claude는 설치된 Claude Code와 Agent SDK가 인증·도구·스킬·
프로젝트 지침을 담당하고, OpenCode는 `opencode acp`로 연결합니다. 이 프로젝트는 공통
화면과 입력 계층을 소유합니다.

## 설치

```powershell
npm install -g devez-vibe
```

설치하면 `dvz` 명령을 사용할 수 있습니다.

| 요건 | 값 |
| --- | --- |
| OS | Windows x64 |
| Node.js | 18 이상 (Claude Agent SDK 런타임 포함) |
| Codex 전제 | Codex CLI 설치 및 로그인 완료 |
| Claude 전제 | Claude Code 설치 후 `claude` 구독 로그인 완료 |

현재 Windows x64 빌드만 배포합니다. 다른 플랫폼에서는 `EBADPLATFORM`으로 설치가 거부됩니다.

바이너리만 직접 받으려면:

```powershell
npm pack devez-vibe
```

## 사용

Codex CLI가 설치되고 로그인된 환경에서:

```powershell
dvz
```

현재 Claude Code 구독 로그인을 그대로 사용하는 개인용 Claude 세션:

```powershell
dvz --model claude
dvz --model sonnet --effort high
```

별도 API 키는 사용하지 않습니다. `ANTHROPIC_API_KEY`와 `ANTHROPIC_AUTH_TOKEN`은 Claude
SDK 자식 프로세스에서 제거되며, 기존 `claude` 로그인 저장소만 사용합니다.
이 경로는 본인 계정의 로컬 개인 사용용입니다. 로그인 공유나 제3자용 인증 화면으로 제공하지 않습니다.

주요 옵션:

```text
dvz [-r|--resume [SESSION] | -c|--continue] [--model MODEL] [--effort EFFORT]
    [--cwd PATH] [--codex PATH] [--claude PATH] [--theme THEME] [--renderer RENDERER]
dvz doctor
dvz update
dvz version        # `dvz --version`과 동일
```

`--resume`만 입력하면 검색 가능한 세션 피커를 열고, `--continue`는 현재 폴더의
가장 최근 세션을 바로 이어갑니다. 실행 중에는 `/resume [SESSION]` 또는 별칭
`/continue`로 세션을 전환할 수 있습니다. 입력창의 전체 명령은 `/help`에서 확인합니다.

`--theme`는 `minimal`, `soft`, `dark`, `gray`, `softpink`, `midnight`를 받습니다.
`--renderer`는 `fullscreen`(하단 고정 composer와 자체 스크롤)과 `inline`(터미널
스크롤백 사용) 중 하나이며, 선택은 `%APPDATA%\DevezVibe\renderer.txt`에 저장되고
`DEVEZ_VIBE_RENDERER`로도 지정할 수 있습니다.

OpenCode provider는 실행 중 `/provider opencode` 또는 `/connect`로 연결하며,
API key 또는 OAuth로 인증합니다 (`opencode-go` 포함).

`dvz doctor`는 현재 실행 파일, 작업 폴더, 업데이트 전환 경로와 각 제공자의 실행 준비 상태를 점검합니다.

### 업데이트

새 버전이 배포되면 시작 시 안내 배너가 표시됩니다.

```powershell
dvz update
```

새 버전을 별도 경로에 내려받아 실행 파일을 검증한 뒤 다음 실행 버전으로 전환합니다.
실행 중인 `dvz` 세션은 이전 버전을 유지하고, 새로 시작하는 세션부터 새 버전을 사용합니다.
업데이트 확인을 끄려면 `DEVEZ_VIBE_NO_UPDATE_CHECK` 환경변수를 설정합니다.

## 기능

### 세션

- 새 스레드 시작, 검색 가능한 세션 피커, ID/이름 기반 resume
- `/btw`(별칭 `/side`)로 현재 대화를 유지한 채 임시 곁가지 대화 진행
- `/worktree`로 현재 대화를 이어받아 Git 작업 트리로 진입

### 에이전트 역할

역할은 다음 턴에 전달되는 지침과 쓰기 권한을 함께 바꿉니다. `/agent`로 고르고 `Tab`으로
순환합니다.

| 역할 | 성격 | 쓰기 범위 |
| --- | --- | --- |
| Builder | 일상적인 개발 작업 전반 | 제한 없음 |
| Planner | 요구사항 인터뷰 후 구현 계획 수립 | `docs/plans`만 |
| Researcher | 여러 출처를 조사해 근거와 한계를 보고 | 없음 |
| Reviewer | 변경과 계획을 검토해 심각도와 판정 | 없음 |
| Goal Runner | 목표를 정하고 끝까지 완수 | 제한 없음 |

- `%APPDATA%\DevezVibe\agents`에 역할당 `.md` 파일 하나를 두면 사용자 정의 역할 추가
- `/auto-knowledge`로 반복 실수와 필요한 지식의 자동 기록 켜고 끄기

### 모델과 effort

- 모델 카탈로그 기반 `/model` 선택과 `/effort`에서 서버가 지원하는 수준만 노출
- `/model sol`, `/model terra`, `/model luna`, `/model spark` 등 짧은 별칭과 숫자키 선택
- `/provider`로 Claude·Codex·OpenCode 전환, `/fast`로 fast 서비스 등급 전환

### 스트리밍과 승인

- 응답, reasoning summary, 명령, 파일 변경, MCP 호출 스트리밍
- 명령·파일 변경 승인과 `/permissions`의 provider별 권한 규칙 관리
- 실행 중 입력 steer 및 `Esc`/`Ctrl+C` 중단

### 입력 (composer)

- `/` 명령 자동완성과 `$`(Plugin·Skill·App), `@`(Plugin·Skill·파일·폴더) 검색
- `Ctrl+Z`/`Ctrl+Y` 실행 취소·다시 실행, `Ctrl+W`/`Ctrl+K` 단어·줄 삭제, `Ctrl+J` 줄바꿈
- `Alt+B`/`Alt+F` 단어 이동

### 렌더링

- 터미널 스크롤백을 보존하며 변경된 행만 갱신하는 증분 렌더링
- Git 브랜치, 모델, effort, context, 5h/주간 한도, Fast 상태를 표시하는 하단 상태줄
- `/vibemode`(`Alt+V`)로 응답·shell·diff 표시를 한 번에 조절

## 소스에서 빌드

```powershell
cargo build --release
```

공개 client ID만 실행 파일에 포함되며 토큰이나 client secret은 포함하지 않습니다.

빌드 결과는 `target/release/dvz.exe`입니다. 바로 실행하려면:

```powershell
cargo run --release
```

npm 패키지 배포용 스테이징까지 한 번에 처리하려면:

```powershell
node scripts/release-npm.mjs              # 빌드 + 스테이징 + publish --dry-run
node scripts/release-npm.mjs --publish    # 실제 배포
```

버전은 `Cargo.toml`이 단일 기준이며 `npm/package.json`은 실행 시 자동으로 동기화됩니다.

## 경계

`app-server`와 Claude Agent SDK 프로토콜은 CLI/SDK 버전에 따라 변할 수 있습니다. 렌더러 변경은
독립적으로 관리하고, 업스트림에서는 프로토콜/인증/모델 카탈로그 변경만 호환성
대상으로 봅니다. 업데이트가 필요할 때는 [Codex CLI 호환성 업데이트 절차](.knowledge/Codex-CLI-호환성-업데이트.md)를
따라 최신 버전을 확인하고 필요한 변경만 반영합니다.

## 라이선스

MIT. [LICENSE](LICENSE)를 참고하세요.
