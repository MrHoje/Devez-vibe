# Devez Vibe

[![npm](https://img.shields.io/npm/v/devez-vibe)](https://www.npmjs.com/package/devez-vibe)
[![license](https://img.shields.io/npm/l/devez-vibe)](LICENSE)

공식 Codex `app-server`를 사용하는 터미널 클라이언트입니다.

인증, 하네스 프롬프트, 도구, 스킬, `AGENTS.md`, 샌드박스는 공식 `codex app-server`가
그대로 담당합니다. 이 프로젝트는 **화면과 입력 계층만** 소유합니다.

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

바이너리만 직접 받으려면:

```powershell
npm pack devez-vibe
```

## 사용

Codex CLI가 설치되고 로그인된 환경에서:

```powershell
dvz
```

주요 옵션:

```text
dvz [--resume [SESSION] | --continue] [--model MODEL] [--effort EFFORT]
    [--cwd PATH] [--codex PATH] [--theme THEME]
dvz update
```

`--resume`만 입력하면 검색 가능한 세션 피커를 열고, `--continue`는 현재 폴더의
가장 최근 세션을 바로 이어갑니다. 실행 중에는 `/resume [SESSION]` 또는 별칭
`/continue`로 세션을 전환할 수 있습니다. 입력창의 전체 명령은 `/help`에서 확인합니다.

OpenCode provider 연동은 후속 개선 전까지 비활성화되어 있습니다.

### 업데이트

새 버전이 배포되면 시작 시 안내 배너가 표시됩니다.

```powershell
dvz update
```

`npm install -g devez-vibe@latest`와 동일하며, 실행 중인 바이너리를 교체할 수 있도록
별도 창에서 설치가 진행됩니다. 업데이트 확인을 끄려면 `DEVEZ_VIBE_NO_UPDATE_CHECK`
환경변수를 설정합니다.

## 기능

### 세션

- 새 스레드 시작, 세션 검색 피커, ID/이름 기반 resume
- `--resume [SESSION]`, `--continue` 및 실행 중 `/resume`
- `/new` 성공 시 이전 대화와 화면을 비우고 새 세션으로 전환
- 시작과 `/new` 세션 전환 시 화면과 스크롤백을 비우고 새 화면으로 전환

### 모델과 effort

- 모델 카탈로그 기반 `/model` 선택과 좌우 effort 조절
- Codex CLI 정렬을 유지하고 hidden 모델을 제외한 `/model` 피커
- `/model sol`, `/model terra`, `/model luna`, `/model spark` 등 짧은 모델 별칭
- 모델 번호 표시 및 피커에서 `1`~`9` 숫자키 즉시 선택
- `/effort` 슬라이더에서 서버 지원 effort만 노출 (`max`·`ultra` 포함)
- 설명·Auto 없이 모델명과 지원 수준만 표시하는 `/effort` 슬라이더
- Sol·Terra·Luna·GPT-5.5 모델 색상을 picker와 statusline에 공통 적용

### 스트리밍과 승인

- 응답, reasoning summary, 명령, 파일 변경, MCP 호출 스트리밍
- 명령/파일 변경 승인
- 실행 중 입력 steer 및 `Esc`/`Ctrl+C` 중단
- 실행 시간, 파일 diff 통계, 진행 상태 표시

### 입력 (composer)

- `/` 명령 자동완성 및 키보드 모델 선택기
- `/` 명령 자동완성 패널을 하단 입력 영역 바로 위에 고정
- `$`로 Plugin·Skill·App을, `@`로 Plugin·Skill·파일·폴더를 검색하고
  Codex와 같은 표기로 입력하는 composer 자동완성
- `Ctrl+Backspace`/`Ctrl+W` 단어 삭제, `Ctrl+K`/`Ctrl+U` 줄 삭제,
  `Ctrl+Y` 복원, `Alt+B`/`Alt+F` 단어 이동, `Ctrl+J` 줄바꿈
- `/exit`을 `/quit`과 같은 정상 종료 명령으로 지원

### 렌더링

- 일반 터미널 스크롤백을 보존하는 증분 렌더링
- 활성 영역 전체 삭제 없이 변경된 터미널 행만 갱신
- 좌우 테두리와 복사용 공백이 없는 하단 composer
- `You`/`Codex` 헤더 없이 마커와 본문으로 이어지는 대화 출력
- 보낸 프롬프트는 전체 행에 은은한 배경색 적용
- Markdown 제목·목록·인용·코드 블록 표현
- Git 브랜치, 모델, effort, context, 5h/주간 한도, Fast 상태를 표시하는 하단 상태줄
- 모델별 실제 유효 context window를 첫 입력 전부터 표시

## 소스에서 빌드

```powershell
cargo build --release
```

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

`app-server` 프로토콜은 CLI 버전에 따라 변할 수 있습니다. 렌더러 변경은
독립적으로 관리하고, 업스트림에서는 프로토콜/인증/모델 카탈로그 변경만 호환성
대상으로 봅니다. 업데이트가 필요할 때는 [Codex CLI 호환성 업데이트 절차](.knowledge/Codex-CLI-호환성-업데이트.md)를
따라 최신 버전을 확인하고 필요한 변경만 반영합니다.

## 라이선스

MIT. [LICENSE](LICENSE)를 참고하세요.
