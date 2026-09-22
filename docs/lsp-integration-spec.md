# Devez Vibe LSP Integration Specification

Status: Draft for implementation  
Target branch: `feature/lsp-manager`  
Primary platforms: Windows first, cross-platform compatible design  
Primary language target: C# / Roslyn-compatible language server

## 1. Goal

Devez Vibe에 Language Server Protocol(LSP) 기반의 semantic code intelligence 계층을 추가한다.

목표는 다음과 같다.

- 사용자가 별도의 IDE를 열지 않아도 Devez Vibe 안에서 language server 상태를 확인하고 관리할 수 있다.
- Codex, Claude, OpenCode가 동일한 Devez LSP tool surface를 사용할 수 있다.
- LSP 프로세스는 필요할 때만 시작하는 lazy-load 방식으로 동작한다.
- 모델은 정의, 구현체, 참조, 타입 정보, diagnostics를 grep보다 정확한 semantic lookup으로 조회할 수 있다.
- provider별로 LSP를 중복 구현하지 않고 공통 `LspManager`를 사용한다.
- 기존 provider runtime 및 session/cache locality를 유지한다.

## 2. Non-goals

초기 구현에서는 다음 항목을 목표로 하지 않는다.

- 모든 언어 서버 자동 설치
- 모든 language server 최신 버전 자동 추종
- rename/code action/formatting 등 mutation 기능
- provider별 별도 LSP 구현
- 모든 workspace에 language server를 항상 선기동
- VS/VS Code가 관리하는 language server 파일을 Devez가 직접 덮어쓰기

## 3. High-level architecture

```text
                         Devez Vibe
                             |
                    +--------+--------+
                    |                 |
                 /lsp UI          Agent tool surface
                    |                 |
                    +--------+--------+
                             |
                         LspManager
                             |
               +-------------+-------------+
               |             |             |
             C#/Roslyn   rust-analyzer   tsserver ...
                             |
                         Workspace
```

Provider 연결:

```text
Codex      -> MCP adapter ----+
Claude     -> MCP/SDK adapter +--> Devez LSP tools -> LspManager
OpenCode   -> ACP MCP adapter +
```

## 4. User-facing command

### 4.1 `/lsp`

기본 동작은 현재 workspace에서 language server를 탐지하고 상태를 표시한다.

```text
/lsp
/lsp status
```

초기 상태 예시:

```text
Language servers

workspace: D:\Eghis
discovery: lazy (no language-server process was started)

C#         installed · Microsoft.CodeAnalysis.LanguageServer · VS Code extension · ...
Rust       installed · rust-analyzer · PATH · ...
TypeScript not installed · available
Python     not installed · available
Go         not installed · available
```

### 4.2 Future commands

후속 단계에서 아래 명령을 고려한다.

```text
/lsp reload
/lsp stop
/lsp install <language>
/lsp update <language>
```

원칙:

- `/lsp`는 사람용 관리 UI다.
- 모델용 semantic lookup은 별도 tool surface로 제공한다.
- 사용자가 `/lsp start`를 실행하지 않아도 첫 LSP tool 호출 시 자동으로 시작되어야 한다.

## 5. Discovery model

Devez는 language server를 자동 설치하는 대신 우선 탐지한다.

탐지 우선순위:

1. PATH
2. 알려진 editor extension 위치
3. 향후 Devez-managed install 위치
4. 필요 시 project-local tool 위치

현재 1차 구현 대상:

- C#: `omnisharp`, `roslyn-language-server`, VS Code/Cursor C# extension 내부 실행 파일
- Rust: `rust-analyzer`
- TypeScript: `typescript-language-server`
- Python: `basedpyright-langserver`, `pyright-langserver`, `pylsp`
- Go: `gopls`
- C/C++: `clangd`

Workspace marker 예:

- C#: `*.sln`, `*.csproj`
- Rust: `Cargo.toml`
- TypeScript: `tsconfig.json`, `package.json`
- Python: `pyproject.toml`, `pyrightconfig.json`
- Go: `go.mod`
- C/C++: `compile_commands.json`, `CMakeLists.txt`

## 6. Ownership model

language server는 설치 출처에 따라 구분한다.

### External

예:

- Visual Studio / VS Code extension 관리
- PATH에 이미 설치
- rustup/npm/go 등의 외부 package manager 관리

Devez가 할 수 있는 것:

- detect
- start
- stop
- reload
- capability 표시

Devez가 하지 않는 것:

- 외부 관리 파일 직접 overwrite
- 외부 설치본을 임의로 update

### Managed

향후 `/lsp install`로 Devez가 직접 설치한 경우:

- install
- update
- remove
- start
- stop

를 Devez가 관리할 수 있다.

## 7. Lazy-load lifecycle

기본 정책은 lazy start다.

```text
dvz start
  |
  +-- LSP process 없음
  |
first lsp_* tool call
  |
  +-- workspace/server resolve
  +-- process spawn
  +-- initialize
  +-- initialized
  +-- workspace ready
  |
subsequent calls reuse same process
```

초기에는 workspace마다 server instance를 하나 유지하는 것을 목표로 한다.

후속 최적화:

- idle timeout
- crash recovery
- restart
- workspace별 instance cache
- provider 전환 시 같은 instance 재사용

## 8. Core LspManager responsibilities

`LspManager`는 아래 책임을 가진다.

- workspace root 식별
- 언어별 server registry
- installed server discovery
- lazy process startup
- initialize/initialized handshake
- request id 관리
- JSON-RPC framing
- request/response correlation
- notification 처리
- graceful shutdown
- crash detection/restart
- capability cache
- document synchronization
- path/URI conversion
- UTF-16 LSP position conversion

초기 인터페이스 예:

```rust
LspManager::status(workspace)
LspManager::definition(...)
LspManager::type_definition(...)
LspManager::implementation(...)
LspManager::references(...)
LspManager::hover(...)
LspManager::document_symbols(...)
LspManager::workspace_symbols(...)
LspManager::diagnostics(...)
LspManager::reload(...)
LspManager::stop(...)
```

## 9. LSP protocol requirements

초기 구현에서 최소 지원:

Lifecycle:

- `initialize`
- `initialized`
- `shutdown`
- `exit`

Document sync:

- `textDocument/didOpen`
- `textDocument/didChange`
- `textDocument/didSave`
- `textDocument/didClose`

Read-only semantic requests:

- `textDocument/definition`
- `textDocument/typeDefinition`
- `textDocument/implementation`
- `textDocument/references`
- `textDocument/hover`
- `textDocument/documentSymbol`
- `workspace/symbol`

Diagnostics:

- publish diagnostics notification
- workspace diagnostics는 server capability에 따라 후속 지원

## 10. Position encoding

LSP position은 Rust string byte offset과 다르다.

특히 UTF-16 character offset을 정확히 처리해야 한다.

필수 사항:

- file byte offset -> LSP line/character conversion
- LSP line/character -> file offset conversion
- 한글, emoji, surrogate pair 테스트
- server가 position encoding capability를 광고하면 이를 반영

이 부분이 틀리면 한글이 포함된 C# 파일에서 정의/참조 위치가 어긋날 수 있다.

## 11. Document synchronization

AI가 파일을 수정한 직후 LSP가 stale buffer를 사용하면 안 된다.

초기 안전 정책:

1. LSP가 열지 않은 파일은 disk state 기준 사용
2. Devez가 edit를 관찰한 파일은 didOpen/didChange/didSave 반영
3. server restart 시 열린 document state 재전송
4. file version monotonically 증가

장기적으로 Devez의 file mutation path와 LSP sync를 한 곳에서 통합한다.

## 12. Model tool surface

모델에게 노출할 tool은 provider-independent하게 유지한다.

Phase 1 read-only tools:

- `lsp_definition`
- `lsp_type_definition`
- `lsp_implementation`
- `lsp_references`
- `lsp_hover`
- `lsp_document_symbols`
- `lsp_workspace_symbols`
- `lsp_diagnostics`

Tool input은 가능하면 다음 형태로 통일한다.

```json
{
  "file": "src/PatientService.cs",
  "line": 142,
  "character": 24
}
```

Tool result는 LLM token 사용을 줄이기 위해 compact하게 반환한다.

예:

```text
IPatientRepository.SaveAsync references
- src/ReceptionService.cs:81:20
- src/ClaimService.cs:143:18
- src/PatientBatch.cs:90:12
```

전체 파일 내용을 tool result에 포함하지 않는다.

## 13. Model usage policy

Agent instruction에는 다음 원칙을 추가한다.

- symbol definition -> LSP 우선
- interface/abstract implementation -> LSP 우선
- symbol reference/impact analysis -> LSP 우선
- type/signature/docs -> LSP hover 우선
- LSP unavailable 또는 결과 부족 시 grep/search/read 보조 사용
- conceptual/string search는 grep/search가 더 적합할 수 있음

즉:

```text
search/grep = 심볼 또는 개념 발견
LSP         = 발견한 심볼의 정확한 semantic relation 확인
```

## 14. Provider integration

### Codex

권장 경로:

```text
Codex app-server
 -> local MCP
 -> Devez LSP tool adapter
 -> LspManager
```

현재 Devez는 Codex app-server와 MCP lifecycle을 이미 다루므로 기존 runtime을 유지하면서 tool만 추가한다.

### Claude

Claude Agent SDK에 Devez LSP tool을 공급한다.

Claude 자체 native LSP 기능과 중복될 수 있으므로:

- native LSP 우선
- Devez LSP는 provider 공통 behavior 또는 fallback
- 동일 기능이 중복 노출되지 않도록 capability 정책 필요

### OpenCode

ACP session 생성의 `mcpServers`를 통해 Devez LSP MCP server를 전달하는 방향으로 연결한다.

현재 Devez 코드가 `mcpServers: []`를 보내고 있으므로 명확한 연결 지점이 존재한다.

## 15. MCP adapter design

최초 구현은 단순한 stdio MCP sidecar로 시작할 수 있다.

```text
dvz lsp-mcp
```

이 process가 LSP tools를 expose한다.

초기 구조:

```text
provider
 -> stdio MCP process
 -> LspManager
 -> language server
```

후속 최적화에서는 provider마다 별도 MCP process가 떠도 language server 자체는 workspace 기준으로 공유할 수 있도록 shared host/daemon 구조를 검토한다.

## 16. Mutation features - later phase

read-only semantic navigation이 안정된 이후에만 추가한다.

- rename
- code action
- formatting
- WorkspaceEdit apply
- file rename lifecycle
  - `workspace/willRenameFiles`
  - `workspace/didRenameFiles`

필수 안전 조건:

- Builder/Goal Runner 등 write-capable role에만 허용
- Planner/Reviewer는 read-only
- WorkspaceEdit 적용 전 path scope/permission 검증
- multi-file edit rollback/error handling
- 적용 후 LSP sync

## 17. Reliability requirements

- startup timeout
- request timeout
- process crash detection
- server stderr capture
- one automatic restart attempt
- invalid JSON-RPC response isolation
- unknown capability graceful fallback
- server unavailable 시 모델에게 명확한 tool error 반환
- LSP 장애가 Devez 메인 session을 종료시키면 안 됨

## 18. Performance policy

- startup 시 language server를 실행하지 않는다.
- first-use lazy start
- workspace당 server 재사용
- 요청마다 server process를 새로 만들지 않는다.
- tool result는 compact하게 반환한다.
- diagnostics 대량 반환은 개수 제한/요약을 둔다.
- 향후 idle timeout을 선택적으로 추가한다.

## 19. Security / permissions

Read-only LSP tools는 기본 허용할 수 있다.

Mutation tools는 기존 Devez permission model과 통합한다.

권장 정책:

```text
Planner / Reviewer
  -> read-only LSP only

Builder
  -> read-only + approved mutation tools

Goal Runner
  -> configured workspace permission policy
```

language server가 반환한 command를 무조건 실행하면 안 된다.

## 20. Implementation phases

### Phase 0 - Discovery UI

Status: started

- [x] `/lsp`
- [x] `/lsp status`
- [x] slash completion
- [x] workspace marker discovery
- [x] PATH server discovery
- [x] VS Code/Cursor C# extension discovery
- [ ] compile/test verification on Windows
- [ ] Visual Studio installation discovery refinement

### Phase 1 - LSP transport

- [ ] `LspManager`
- [ ] process spawn
- [ ] stdio JSON-RPC framing
- [ ] initialize/initialized
- [ ] request correlation
- [ ] shutdown/exit
- [ ] lazy start
- [ ] capability cache
- [ ] crash handling

### Phase 2 - Read-only semantic operations

- [ ] definition
- [ ] type definition
- [ ] implementation
- [ ] references
- [ ] hover
- [ ] document symbols
- [ ] workspace symbols
- [ ] diagnostics
- [ ] UTF-16 position tests

### Phase 3 - Model tool exposure

- [ ] local MCP adapter
- [ ] Codex connection
- [ ] OpenCode ACP `mcpServers`
- [ ] Claude SDK connection/fallback policy
- [ ] shared tool descriptions/instructions

### Phase 4 - Lifecycle UX

- [ ] `/lsp reload`
- [ ] `/lsp stop`
- [ ] running/stopped/error state
- [ ] PID/root/capabilities display
- [ ] idle timeout optional
- [ ] auto restart

### Phase 5 - Managed installation

- [ ] `/lsp install <language>`
- [ ] Devez-managed install directory
- [ ] external vs managed ownership
- [ ] `/lsp update`
- [ ] `/lsp remove`
- [ ] no overwrite of externally managed servers

### Phase 6 - Mutation features

- [ ] rename
- [ ] code actions
- [ ] formatting
- [ ] WorkspaceEdit validation/apply
- [ ] permission integration
- [ ] file rename lifecycle

## 21. Phase 1 acceptance criteria

다음 조건을 만족하면 transport 단계 완료로 본다.

1. `dvz` 실행 시 language server process가 뜨지 않는다.
2. 첫 semantic request 시에만 해당 server가 시작된다.
3. 같은 workspace의 두 번째 요청은 같은 process를 재사용한다.
4. `initialize -> initialized` handshake가 정상 완료된다.
5. request id에 맞는 response를 정확히 반환한다.
6. server가 죽어도 Devez UI/session은 유지된다.
7. shutdown 시 child process가 남지 않는다.
8. C# workspace에서 최소 `definition` 요청 하나가 end-to-end로 동작한다.
9. 한글이 포함된 source line에서도 position mapping이 정확하다.
10. 관련 unit test가 추가된다.

## 22. Phase 2 acceptance criteria

1. C# interface implementation을 정확히 찾는다.
2. overloaded method reference가 단순 문자열 검색보다 정확하게 분리된다.
3. hover에서 resolved type/signature를 반환한다.
4. diagnostics가 file/line/severity/message 형태로 compact하게 반환된다.
5. LSP unavailable 시 grep fallback을 막지 않는다.
6. result 개수 제한과 truncation metadata가 존재한다.

## 23. Recommended next task

다음 작업은 Phase 1이다.

구현 순서:

1. `LspProcess` / `LspManager` 자료구조
2. Content-Length 기반 stdio JSON-RPC codec
3. initialize/initialized
4. lazy spawn
5. shutdown
6. C# server 대상으로 `textDocument/definition` 1개 end-to-end
7. process reuse test
8. UTF-16 position utility/test

이 단계에서는 MCP와 provider 연결을 아직 넣지 않는다.

먼저 Devez 내부에서 LSP transport가 안정적으로 동작해야 Codex/Claude/OpenCode에 동일한 tool adapter를 안전하게 붙일 수 있다.
