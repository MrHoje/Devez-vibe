# SuperVibe Response All 프롬프트 접기 확장 구현 계획

**분류:** 경계 내
**목표:** SuperVibe 모드에서 Response 표시가 All일 때도 Completed처럼 프롬프트를 눌러 이전 응답 묶음을 접고 펼 수 있고, All은 기본 펼침·Completed는 기본 접힘으로 다시 불러오기 때도 유지된다.
**접근:** 화면 접기 조건을 SuperVibe 전체로 넓히고 펼침 기본값만 표시 모드에 따라 다르게 계산한다. 실시간 턴도 All에서 같은 묶음을 만들고, 프롬프트 바닥글 클릭 연결을 All까지 확장한다.
**의도 차이:** 요청 문면에 없는 다음을 추가한다. 모드 전환과 다시 불러오기 때 사용자 토글을 비워 기본값으로 되돌리는 것, 접기 펼치기 전환 애니메이션은 Completed에만 두는 것, All 실시간 턴도 다시 불러오기와 같은 묶음 구조로 만드는 것이며, 모두 기본값 차이만 둔다는 요청을 실제로 구현하기 위해 필요하다. 제외한 것은 없음.

## 결정 기록
- 결정: 단일 토글 집합과 표시 모드별 기본값 계산을 함께 쓴다.
- 결정 요인: All 기본 펼침과 Completed 기본 접힘을 한 구조로 설명할 수 있다. 다시 불러오기와 모드 전환 때 집합을 비우면 초기 상태가 자동으로 맞는다. 실시간과 다시 불러오기 묶음 구조를 일치시켜 클릭 대상이 항상 같다.
- 검토한 대안: All 진입 때 펼침 집합을 미리 채우는 방식은 기각한다. 새로 들어오는 묶음마다 채워야 해서 실시간과 다시 불러오기 동기화가 깨지기 쉽다. All에서 묶음을 만들지 않고 개별 답변을 그대로 두는 방식은 기각한다. 클릭해서 접을 대상이 없어 요청한 동작이 성립하지 않는다.
- 결과와 후속: 화면은 `ResponseDisplayMode`를 알아야 하고, 토글 집합은 기본값에서 벗어난 것만 담는다. 모드 전환과 다시 불러오기 때 집합을 비우는 동작이 뒤따라야 한다.
- 전제: SuperVibe가 아닌 모드에서는 이번 동작을 바꾸지 않는다.
- 전제: 전환 애니메이션(`response_collapse`)은 Completed에만 둔다.
- 전제: All에서 접으면 중간 응답만 숨기고 마지막 답변과 프롬프트는 남긴다.
- 전제: 영속 저장에는 토글 집합을 저장하지 않고 표시 모드만 저장한다.

## 범위
- 포함: SuperVibe와 All 조합에서 프롬프트 바닥글에 응답 묶음 표시와 클릭 영역 연결, 기본 펼침 계산, 클릭 때 접기·펼치기 전환, 실시간 턴 묶음 생성 확대, 다시 불러오기 때 모드별 초기 상태 적용.
- 포함: 모드 전환 때 토글 초기화, 관련 단위 테스트와 화면 테스트 추가.
- 제외: SuperVibe가 아닌 모드의 표시 방식 변경.
- 제외: 토글 상태의 세션 영속화와 서버 저장 형식 변경.
- 제외: 키보드 단축키 추가와 바닥글 문구 디자인 변경.

## 전역 제약
- Rust edition `2024`, 패키지 `devez-vibe` `1.7.58`, 바이너리 `dvz`, 진입점 `src/main.rs`.
- 기존 레이아웃 유지: 상태 `src/state.rs`, 화면 `src/renderer.rs`, 입력·연결 `src/main.rs`.
- 기존 표시 모드 값 유지: `ResponseDisplayMode::All`, `ResponseDisplayMode::Completed`.
- 기존 묶음 종류 유지: `BlockKind::ProgressGroup`, `BlockKind::User`, `BlockKind::Assistant`.
- 확인 명령은 저장소 루트에서 실행한다.

## 변경 파일 지도
| 파일 | 작업 | 책임 |
| --- | --- | --- |
| `src/state.rs` | 수정 | 응답 표시 모드를 화면에 전달하고, 접기 조건과 실시간 묶음 생성 범위를 All까지 넓힌다. |
| `src/renderer.rs` | 수정 | All 기본 펼침 계산, 프롬프트 바닥글과 클릭 영역 표시, 묶음 숨김·보임 렌더링을 맡는다. |
| `src/main.rs` | 수정 | 프롬프트 클릭 연결이 All에서도 동작하게 유지하고, 다시 불러오기 때 화면 토글 초기화를 맡는다. |
| `src/state.rs` 테스트 모듈 | 테스트 | 모드별 접기 조건, 실시간 묶음 생성, 다시 불러오기 초기 상태를 검증한다. |
| `src/renderer.rs` 테스트 모듈 | 테스트 | All 기본 펼침 표시, 프롬프트 클릭 전환, 접힌 묶음 숨김을 검증한다. |

## 태스크

### 태스크 1: 모드별 기본값 계산으로 화면 접기 확대

**의존:** 없음

**파일:**
- 수정: `src/state.rs`
- 수정: `src/renderer.rs`
- 테스트: `src/state.rs` 테스트 모듈
- 테스트: `src/renderer.rs` 테스트 모듈

**인터페이스:**
- 소비: `AppState::vibe_mode`, `AppState::response_display_mode`, `Block::progress_group`, `BlockKind::ProgressGroup`.
- 제공: `View` 구조체에 `response_display_mode: ResponseDisplayMode` 필드 추가, `Renderer`에 같은 필드와 `is_prompt_group_expanded` 규칙 추가. All에서는 토글 집합에 없을 때 펼침, Completed에서는 토글 집합에 있을 때 펼침.

**완료 조건:** SuperVibe와 All에서도 화면 접기 표시가 켜지고, 아무것도 누르지 않은 묶음은 All에서 펼쳐지고 Completed에서 접혀 보인다.

- [ ] **1단계: 실패하는 테스트 작성**
```rust
#[test]
fn supervibe_all_keeps_fold_view() {
    let mut state = test_state();
    while state.vibe_mode() != VibeMode::SuperVibe {
        state.cycle_vibe_mode();
    }
    state.set_response_display_mode(ResponseDisplayMode::All);
    assert!(state.view().fold_progress_groups);
    assert_eq!(state.view().response_display_mode, ResponseDisplayMode::All);
}
```
```rust
#[test]
fn supervibe_all_prompt_starts_expanded() {
    let prompt = Block::new(BlockKind::User, "gpt-5.6-sol", "다시 불러온 요청");
    let progress = Block::progress_group(vec![Block::new(
        BlockKind::Assistant,
        "Codex",
        "중간 진행 기록",
    )]);
    let progress_id = progress.id();
    let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
    renderer.chat_layout = true;
    renderer.fold_progress_groups = true;
    renderer.response_display_mode = ResponseDisplayMode::All;
    renderer.history.extend([prompt, progress]);
    renderer.rewrap(80);
    let text = renderer.wrapped.iter().map(painted).collect::<Vec<_>>().join("\n");
    assert!(text.contains("Hide"));
    assert!(text.contains("중간 진행 기록"));
    assert!(renderer.wrapped.iter().any(|line| line.pick.as_ref().is_some_and(|regions| {
        regions.columns_of(&Pick::History(progress_id)).is_some()
    })));
}
```
- [ ] **2단계: 실패 확인**
  실행: `cargo test --bin dvz supervibe_all_keeps_fold_view`
  기대: FAIL, SuperVibe와 All에서 `fold_progress_groups`가 거짓이라 첫 번째 단언에서 실패한다.
  실행: `cargo test --bin dvz supervibe_all_prompt_starts_expanded`
  기대: FAIL, `response_display_mode` 필드나 All 기본 펼침 분기가 없어 컴파일 실패 또는 `Hide` 단언 실패로 끝난다.
- [ ] **3단계: 최소 구현**
```rust
pub struct View<'a> {
    pub response_display_mode: ResponseDisplayMode,
    pub fold_progress_groups: bool,
}
```
```rust
pub fn view(&self) -> View<'_> {
    View {
        response_display_mode: self.response_display_mode,
        fold_progress_groups: self.vibe_mode == VibeMode::SuperVibe,
    }
}
```
```rust
fn is_prompt_group_expanded(&self, group_id: u64) -> bool {
    match self.response_display_mode {
        ResponseDisplayMode::All => !self.expanded_tools.contains(&group_id),
        ResponseDisplayMode::Completed => self.expanded_tools.contains(&group_id),
    }
}
```
```rust
fn history_block_lines(&self, block: &Block, width: u16) -> Vec<PaintLine> {
    if matches!(block.kind, BlockKind::User)
        && self.fold_progress_groups
        && let Some(group) = self.progress_group_for_prompt(block.id())
    {
        let expanded = self.is_prompt_group_expanded(group.id());
        return user_prompt_lines_with_history(
            block,
            width,
            Some((group.id(), &group.title, expanded)),
            self.chat_layout,
        );
    }
    if matches!(block.kind, BlockKind::ProgressGroup)
        && self.prompt_for_progress_group(block.id()).is_some()
    {
        return embedded_progress_group_lines(
            block,
            width,
            self.is_prompt_group_expanded(block.id()),
            self.response_reveal_for(block.id()),
        );
    }
    block_group_lines_at(
        block,
        width,
        self.shell_display_mode,
        self.diff_display_mode,
        self.expanded_tools.contains(&block.id()),
        self.response_reveal_for(block.id()),
    )
}
```
- [ ] **4단계: 통과 확인**
  실행: `cargo test --bin dvz supervibe_all_keeps_fold_view`
  기대: PASS, 출력에 경고나 잡음 없음.
  실행: `cargo test --bin dvz supervibe_all_prompt_starts_expanded`
  기대: PASS, 출력에 경고나 잡음 없음.

### 태스크 2: 실시간 묶음과 프롬프트 클릭을 All까지 연결

**의존:** 태스크 1

**파일:**
- 수정: `src/state.rs:collapse_completed_response`
- 수정: `src/state.rs:collapse_progress_before_next_answer`
- 수정: `src/state.rs:set_response_display_mode`
- 수정: `src/renderer.rs:history_block_lines`
- 수정: `src/renderer.rs:render`
- 테스트: `src/state.rs` 테스트 모듈
- 테스트: `src/renderer.rs` 테스트 모듈

**인터페이스:**
- 소비: 태스크 1의 `is_prompt_group_expanded`, `View::response_display_mode`, `View::fold_progress_groups`.
- 제공: All 실시간 턴에서 `Block::progress_group` 묶음이 `committed`에 남는다. 프롬프트 클릭 `Pick::History(group_id)`가 `toggle_tool(group_id)`로 All에서도 같은 묶음을 접고 펼친다. `render`에서 표시 모드가 바뀌면 `expanded_tools`를 비운다.

**완료 조건:** All 실시간 턴이 끝난 뒤 중간 응답이 프롬프트에 묶여 보이고, 프롬프트를 한 번 누르면 묶음이 사라지고 다시 누르면 나타난다.

- [ ] **1단계: 실패하는 테스트 작성**
```rust
#[test]
fn supervibe_all_live_turn_groups_progress_behind_prompt() {
    let mut state = test_state();
    while state.vibe_mode() != VibeMode::SuperVibe {
        state.cycle_vibe_mode();
    }
    state.set_response_display_mode(ResponseDisplayMode::All);
    state.set_turn_started("turn-all-live".to_owned());
    for (id, text) in [("progress-all", "중간 진행 기록"), ("final-all", "마지막 답변")] {
        state.handle_notification(
            "item/completed",
            &json!({ "item": { "id": id, "type": "agentMessage", "text": text } }),
        );
        state.drain_committed();
    }
    state.handle_notification("turn/completed", &json!({ "turn": { "status": "completed" } }));
    let committed = state.drain_committed();
    assert!(committed.iter().any(|block| matches!(block.kind, BlockKind::ProgressGroup)));
}
```
```rust
#[test]
fn prompt_history_click_collapses_expanded_all_group() {
    let prompt = Block::new(BlockKind::User, "gpt-5.6-sol", "눌러서 접는 요청");
    let progress = Block::progress_group(vec![Block::new(
        BlockKind::Assistant,
        "Codex",
        "보이는 중간 기록",
    )]);
    let progress_id = progress.id();
    let mut renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
    renderer.chat_layout = true;
    renderer.fold_progress_groups = true;
    renderer.response_display_mode = ResponseDisplayMode::All;
    renderer.history.extend([prompt, progress]);
    renderer.last_width = 80;
    renderer.rewrap(80);
    renderer.previous_lines = renderer.wrapped.clone();
    let before = renderer.wrapped.iter().map(painted).collect::<Vec<_>>().join("\n");
    assert!(before.contains("Hide"));
    assert!(before.contains("보이는 중간 기록"));
    assert!(renderer.toggle_tool(progress_id));
    let after = renderer.wrapped.iter().map(painted).collect::<Vec<_>>().join("\n");
    assert!(after.contains("+1 Response"));
    assert!(!after.contains("보이는 중간 기록"));
    assert!(renderer.toggle_tool(progress_id));
    let reopened = renderer.wrapped.iter().map(painted).collect::<Vec<_>>().join("\n");
    assert!(reopened.contains("보이는 중간 기록"));
}
```
- [ ] **2단계: 실패 확인**
  실행: `cargo test --bin dvz supervibe_all_live_turn_groups_progress_behind_prompt`
  기대: FAIL, All 실시간 완료 경로가 묶음을 만들지 않아 `ProgressGroup` 단언에서 실패한다.
  실행: `cargo test --bin dvz prompt_history_click_collapses_expanded_all_group`
  기대: FAIL, 같은 이유 또는 프롬프트에 클릭 표시가 없어 두 번째 테스트도 실패한다.
- [ ] **3단계: 최소 구현**
```rust
fn collapse_completed_response(&mut self) {
    if self.vibe_mode != VibeMode::SuperVibe {
        return;
    }
    let assistant_indices = self.turn_response_blocks.iter().enumerate()
        .filter_map(|(index, block)| matches!(block.kind, BlockKind::Assistant).then_some(index))
        .collect::<Vec<_>>();
    if self.response_grouped || assistant_indices.is_empty() {
        return;
    }
    let final_index = assistant_indices.iter().copied()
        .rfind(|&index| self.turn_response_blocks[index].assistant_phase() == AssistantPhase::FinalAnswer)
        .unwrap_or(*assistant_indices.last().expect("assistant block"));
    let progress = self.turn_response_blocks[..final_index].iter()
        .filter(|block| {
            is_context_compaction(block)
                || (matches!(block.kind, BlockKind::Assistant) && !block.body.trim().is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    if progress.is_empty() {
        return;
    }
    let groups = progress_groups_for_prompts(progress, &self.turn_response_boundaries);
    let Some(last_group) = groups.last() else { return; };
    self.response_collapse = (self.response_display_mode == ResponseDisplayMode::Completed)
        .then(|| ResponseCollapseTransition {
            group_id: last_group.id(),
            started_at: Instant::now(),
        });
    self.response_grouped = true;
    self.committed.extend(groups);
}
```
```rust
fn collapse_progress_before_next_answer(&mut self) {
    if self.vibe_mode != VibeMode::SuperVibe {
        return;
    }
    let progress = self.turn_response_blocks.iter()
        .filter(|block| {
            is_context_compaction(block)
                || (matches!(block.kind, BlockKind::Assistant) && !block.body.trim().is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    if progress.is_empty() {
        return;
    }
    self.response_grouped = true;
    self.committed.extend(progress_groups_for_prompts(
        progress,
        &self.turn_response_boundaries,
    ));
}
```
```rust
fn set_response_display_mode(&mut self, mode: ResponseDisplayMode) {
    if self.response_display_mode == mode {
        return;
    }
    self.response_display_mode = mode;
    self.response_collapse = None;
}
```
```rust
pub fn render(&mut self, committed: &[Block], view: View<'_>) -> Result<()> {
    let display_mode_changed = self.response_display_mode != view.response_display_mode;
    if display_mode_changed {
        self.expanded_tools.clear();
        self.response_display_mode = view.response_display_mode;
    }
    let mode_changed = self.shell_display_mode != view.shell_display_mode
        || self.diff_display_mode != view.diff_display_mode
        || self.chat_layout != view.chat_layout
        || self.fold_progress_groups != view.fold_progress_groups
        || display_mode_changed;
}
```
- [ ] **4단계: 통과 확인**
  실행: `cargo test --bin dvz supervibe_all_live_turn_groups_progress_behind_prompt`
  기대: PASS, 출력에 경고나 잡음 없음.
  실행: `cargo test --bin dvz prompt_history_click_collapses_expanded_all_group`
  기대: PASS, 출력에 경고나 잡음 없음.

### 태스크 3: 다시 불러오기 초기 상태를 모드별 기본값으로 맞추기

**의존:** 태스크 1, 태스크 2

**파일:**
- 수정: `src/main.rs:resume_into_state`
- 수정: `src/state.rs:prepare_resume`
- 수정: `src/renderer.rs:clear_screen`
- 테스트: `src/state.rs` 테스트 모듈
- 테스트: `src/renderer.rs` 테스트 모듈

**인터페이스:**
- 소비: 태스크 1의 기본값 규칙, 태스크 2의 실시간 묶음 구조, `read_session_modes`, `write_session_modes`.
- 제공: 다시 불러오기 완료 뒤 `expanded_tools`가 비어 있어 Completed는 모두 접힘, All은 모두 펼침으로 보인다. 저장된 `response_display_mode` 복원 뒤 화면이 같은 규칙을 그대로 쓴다.

**완료 조건:** 같은 저장 묶음을 Completed로 다시 열면 프롬프트가 응답 개수로 보이고, All로 다시 열면 숨김 표시로 보이며 중간 응답이 보인다.

- [ ] **1단계: 실패하는 테스트 작성**
```rust
#[test]
fn resumed_history_follows_display_mode_default() {
    let thread = json!({
        "turns": [{
            "id": "turn-1",
            "status": "completed",
            "startedAt": 1_784_992_108_i64,
            "completedAt": 1_784_992_379_i64,
            "items": [
                { "type": "userMessage", "id": "user-1", "content": [{ "type": "text", "text": "다시 연 요청" }] },
                { "type": "agentMessage", "id": "item-1", "text": "중간 진행 기록" },
                { "type": "agentMessage", "id": "item-2", "text": "마지막 답변" }
            ]
        }]
    });
    let mut completed = test_state();
    while completed.vibe_mode() != VibeMode::SuperVibe {
        completed.cycle_vibe_mode();
    }
    completed.set_response_display_mode(ResponseDisplayMode::Completed);
    completed.load_history(&thread, None);
    let completed_blocks = completed.drain_committed();
    assert!(completed_blocks.iter().any(|block| matches!(block.kind, BlockKind::ProgressGroup)));
    let mut completed_renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
    completed_renderer.chat_layout = true;
    completed_renderer.fold_progress_groups = true;
    completed_renderer.response_display_mode = ResponseDisplayMode::Completed;
    completed_renderer.history.extend(completed_blocks);
    completed_renderer.rewrap(80);
    let completed_text = completed_renderer.wrapped.iter().map(painted).collect::<Vec<_>>().join("\n");
    assert!(completed_text.contains("+1 Response"));
    assert!(!completed_text.contains("중간 진행 기록"));

    let mut all = test_state();
    while all.vibe_mode() != VibeMode::SuperVibe {
        all.cycle_vibe_mode();
    }
    all.set_response_display_mode(ResponseDisplayMode::All);
    all.load_history(&thread, None);
    let all_blocks = all.drain_committed();
    assert!(all_blocks.iter().any(|block| matches!(block.kind, BlockKind::ProgressGroup)));
    let mut all_renderer = Renderer::new(ThemeKind::Minimal, RenderMode::Fullscreen);
    all_renderer.chat_layout = true;
    all_renderer.fold_progress_groups = true;
    all_renderer.response_display_mode = ResponseDisplayMode::All;
    all_renderer.history.extend(all_blocks);
    all_renderer.rewrap(80);
    let all_text = all_renderer.wrapped.iter().map(painted).collect::<Vec<_>>().join("\n");
    assert!(all_text.contains("Hide"));
    assert!(all_text.contains("중간 진행 기록"));
}
```
- [ ] **2단계: 실패 확인**
  실행: `cargo test --bin dvz resumed_history_follows_display_mode_default`
  기대: FAIL, All 다시 불러오기 표시가 묶음을 프롬프트 뒤가 아니라 별도 구간으로 그려 All 펼침 단언에서 실패한다.
- [ ] **3단계: 최소 구현**
```rust
pub fn prepare_resume(&mut self) {
    self.committed.clear();
    self.active.clear();
    self.active_order.clear();
    self.reset_turn_item_tracking();
    self.response_collapse = None;
    self.response_grouped = false;
}
```
```rust
pub fn clear_screen(&mut self) -> Result<()> {
    self.expanded_tools.clear();
    self.response_collapse = None;
    self.wrapped.clear();
    self.wrapped_width = 0;
    self.scroll_back = 0;
    self.reset_screen()
}
```
```rust
async fn resume_into_state(
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    thread_id: &str,
    protect_side_exit_keys: bool,
) -> Result<Switched> {
    renderer.clear_screen()?;
    state.prepare_resume();
    state.load_history(&history, rollout.as_ref());
    state.begin_cost_restore();
    Ok(Switched::Done(queued))
}
```
- [ ] **4단계: 통과 확인**
  실행: `cargo test --bin dvz resumed_history_follows_display_mode_default`
  기대: PASS, 출력에 경고나 잡음 없음.

## 최종 검증
- 실행: `cargo test --bin dvz prompt_history`
  기대: PASS, 프롬프트 묶음 관련 테스트가 모두 통과한다.
- 실행: `cargo test --bin dvz response_display`
  기대: PASS, 표시 모드 관련 테스트가 모두 통과한다.
- 실행: `cargo test --bin dvz resume`
  기대: PASS, 다시 불러오기 관련 테스트가 모두 통과한다.
- 실행: `cargo test`
  기대: PASS, 전체 테스트가 통과한다.
- 수동 확인: SuperVibe에서 `/Response All`로 바꾼 뒤 프롬프트 바닥글에 숨김 표시가 보이고, 프롬프트를 누르면 중간 응답이 접히고 다시 누르면 펼쳐진다.
- 수동 확인: `/Response Completed`로 바꾸면 같은 프롬프트가 응답 개수로 접혀 보이고, 누르면 펼쳐진다.
- 수동 확인: 각 모드에서 `/resume`으로 같은 대화를 다시 열면 All은 모두 펼쳐지고 Completed는 모두 접혀 있다.

## 위험과 완화
- All 실시간 턴까지 묶으면 기존 All 화면 행 수가 달라질 수 있다 — 태스크 2 테스트에서 묶음 생성과 기본 펼침 표시를 함께 확인한다.
- 토글 집합 의미를 모드별로 다르게 읽으면 모드 전환 때 반대로 보일 수 있다 — 모드 전환과 다시 불러오기 때 집합을 비우고 그 테스트를 최종 검증에 둔다.
- 다시 불러온 묶음과 실시간 묶음의 자식 구성이 다르면 접힌 개수가 어긋나 보인다 — `merged_turn_blocks`와 `group_turn_response`를 통째로 재사용하고 개수 표시를 기존 제목 생성에 맡긴다.
- 이 계획이 다루지 않는 것: 토글 영속화, 서버 저장 형식 변경, SuperVibe 밖의 동작 변경.

## 실행 중단 기준
- 저장된 대화 묶음 구조가 `ProgressGroup`이 아니라서 All과 Completed를 같은 대상으로 묶을 수 없으면 멈추고 묻는다.
- 클릭 좌표와 `Pick::History` 연결을 All에서 재사용할 수 없어 입력 처리 구조를 바꿔야 하면 멈추고 묻는다.
- 저장소 밖으로 나가는 부작용이 필요해지면 멈추고 묻는다.

## 검토 기록
| 회차 | 검토 방식 | 아키텍처 | 실행 가능성 | 요구 변경 | 반영 |
| --- | --- | --- | --- | --- | --- |
| 1 | 자체 검토 | WATCH | ITERATE | 없음 | 없음 |

## 의도 조정
- 없음.

## 실행 기록
- 비워 둔다.
