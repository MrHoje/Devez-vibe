# Devez Vibe

공식 `codex app-server`를 위한 차분한 터미널 클라이언트입니다. 인증, 하네스 프롬프트,
도구, 스킬, `AGENTS.md`, 샌드박스는 Codex가 그대로 담당하고, 이 프로젝트는 화면과 입력
계층만 소유합니다.

## 설치

```powershell
npm install -g devez-vibe
```

설치하면 `dvz` 명령을 사용할 수 있습니다.

| 요건 | 값 |
| --- | --- |
| OS | Windows x64 |
| Node.js | 18 이상 (설치용. 실행 자체는 네이티브 바이너리) |
| 전제 | Codex CLI 설치 및 로그인 완료 |

현재 Windows x64 빌드만 배포합니다. 다른 플랫폼에서는 `EBADPLATFORM`으로 설치가 거부됩니다.

## 사용

Codex CLI가 설치되고 로그인된 환경에서:

```powershell
dvz
```

주요 옵션:

```text
dvz [--resume [SESSION] | --continue] [--model MODEL] [--effort EFFORT]
    [--cwd PATH] [--codex PATH] [--theme THEME]
```

`--resume`만 입력하면 검색 가능한 세션 피커를 열고, `--continue`는 현재 폴더의 가장 최근
세션을 바로 이어갑니다. 실행 중 전체 명령은 `/help`에서 확인합니다.

## 업데이트

새 버전이 배포되면 시작 시 안내 배너가 표시됩니다. 배너의 안내대로 실행하면 됩니다.

```powershell
dvz update
```

`npm install -g devez-vibe@latest`와 동일하며, 실행 중인 바이너리를 교체할 수 있도록
별도 창에서 설치가 진행됩니다.

업데이트 확인을 끄려면 `DEVEZ_VIBE_NO_UPDATE_CHECK` 환경변수를 설정합니다.

## 라이선스

MIT. 자세한 내용은 [저장소](https://github.com/MrHoje/Devez-vibe)를 참고하세요.
