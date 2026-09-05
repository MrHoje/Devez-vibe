# Devez Vibe

공식 `codex app-server`와 Claude Agent SDK를 위한 차분한 터미널 클라이언트입니다.
각 공식 런타임이 인증·도구·스킬·프로젝트 지침을 담당하고, 이 프로젝트는 공통 화면과
입력 계층을 소유합니다.

## 설치

```powershell
npm install -g devez-vibe
```

설치하면 `dvz` 명령을 사용할 수 있습니다.

설치 과정에서 `luna-loop` 스킬도 Codex와 Claude의 사용자 스킬 경로에 함께 설치됩니다.
스킬은 자동 적용되지 않으며 `$luna-loop`로 직접 호출해야 합니다. 실제
`gpt-5.6-luna` 검증을 사용하려면 Codex provider와 Luna 위임 기능이 연결돼 있어야 합니다.

| 요건 | 값 |
| --- | --- |
| OS | Windows x64 |
| Node.js | 18 이상 (Claude Agent SDK 런타임 포함) |
| Codex 전제 | Codex CLI 설치 및 로그인 완료 |
| Claude 전제 | Claude Code 설치 후 `claude` 구독 로그인 완료 |

현재 Windows x64 빌드만 배포합니다. 다른 플랫폼에서는 `EBADPLATFORM`으로 설치가 거부됩니다.

## 사용

Codex CLI가 설치되고 로그인된 환경에서:

```powershell
dvz
```

개인 Claude 구독 로그인 재사용:

```powershell
dvz --model claude
dvz --model sonnet --effort high
```

Claude SDK 자식 프로세스는 API 키 환경변수를 제거하고 설치된 Claude Code 로그인만 사용합니다.
본인 계정의 로컬 개인 사용만 대상으로 하며 로그인 공유나 제3자 인증은 제공하지 않습니다.

주요 옵션:

```text
dvz [--resume [SESSION] | --continue] [--model MODEL] [--effort EFFORT]
    [--cwd PATH] [--codex PATH] [--claude PATH] [--theme THEME]
```

`--resume`만 입력하면 검색 가능한 세션 피커를 열고, `--continue`는 현재 폴더의 가장 최근
세션을 바로 이어갑니다. 실행 중 전체 명령은 `/help`에서 확인합니다.

설치 상태와 제공자 연결 준비 상태는 다음 명령으로 점검합니다.

```powershell
dvz doctor
```

## 업데이트

새 버전이 배포되면 시작 시 안내 배너가 표시됩니다. 배너의 안내대로 실행하면 됩니다.

```powershell
dvz update
```

새 버전을 별도 경로에 내려받아 실행 파일을 검증한 뒤 다음 실행 버전으로 전환합니다.
실행 중인 `dvz` 세션은 이전 버전을 유지하고, 새로 시작하는 세션부터 새 버전을 사용합니다.

업데이트 확인을 끄려면 `DEVEZ_VIBE_NO_UPDATE_CHECK` 환경변수를 설정합니다.

## 라이선스

MIT. 자세한 내용은 [저장소](https://github.com/MrHoje/Devez-vibe)를 참고하세요.
