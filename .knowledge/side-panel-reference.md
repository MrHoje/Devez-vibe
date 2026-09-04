# 사이드패널(정보 패널) 예전 구현 참고 자료

2026-07-27 커밋 `b4f1216`(feat: add OpenTUI panel rendering probe)에서 우측 도킹 정보 패널이
완성 상태로 구현되었고, 같은 날 `60d5eff`(chore: preserve current renderer updates)에서 전량
제거되었다. 현재 `main`에는 `print_line_with_selection_bounded`의 주석 한 줄만 남아 있다.

이 문서는 재구현 시 참고할 핵심 조각을 원문 그대로 옮긴 것이다. 현재 렌더러와 구조가 달라
그대로 붙여넣을 수 없으므로 설계 근거와 상수 값을 참고 대상으로 삼는다.

## 전체 원본 확인 방법

```
git show b4f1216:src/renderer.rs
git show b4f1216:src/state.rs
git show b4f1216 --stat
```

## 1. 레이아웃 규칙

```rust
const INFO_PANEL_WIDTH: usize = 24;
const INFO_PANEL_GAP: usize = 3;
const INFO_PANEL_MIN_MAIN_WIDTH: usize = 44;
/// Writing into a terminal's final cell can trigger an implicit wrap and move
/// the cursor before the next absolute paint command arrives.
const INFO_PANEL_AUTOWRAP_GUARD: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
struct InfoPanelLayout {
    main_width: usize,
    panel_left: usize,
    panel_width: usize,
}

impl InfoPanelLayout {
    const HORIZONTAL_PADDING: usize = 2;

    fn content_left(self) -> usize {
        self.panel_left + Self::HORIZONTAL_PADDING
    }

    fn content_width(self) -> usize {
        self.panel_width - 2 * Self::HORIZONTAL_PADDING
    }
}

/// Leaves the conversation enough room to remain readable before docking the
/// fixed-width information panel at the right edge.
fn info_panel_content_width(width: u16) -> Option<u16> {
    let width = usize::from(width);
    (width
        >= INFO_PANEL_MIN_MAIN_WIDTH
            + INFO_PANEL_GAP
            + INFO_PANEL_WIDTH
            + INFO_PANEL_AUTOWRAP_GUARD)
        .then(|| (width - INFO_PANEL_GAP - INFO_PANEL_WIDTH - INFO_PANEL_AUTOWRAP_GUARD) as u16)
}

fn info_panel_layout(total_width: u16) -> Option<InfoPanelLayout> {
    let main_width = usize::from(info_panel_content_width(total_width)?);
    Some(InfoPanelLayout {
        main_width,
        panel_left: main_width + INFO_PANEL_GAP,
        panel_width: INFO_PANEL_WIDTH,
    })
}
```

터미널 폭이 최소 요구치에 못 미치면 `None`을 돌려주고 패널을 열지 않는다.

## 2. 셀 프레임 페인트

```rust
impl CellStyle {
    fn panel() -> Self {
        Self {
            background: Some(blend(theme::palette().background, theme::palette().border, 72)),
            ..Self::plain()
        }
    }
}

fn info_panel_row(row: usize, rows: usize, content_width: usize) -> String {
    let text = match row {
        0 => "Info panel",
        1 if rows > 1 => "No information yet",
        _ => "",
    };
    format!("{text:<content_width$}")
}

fn paint_info_panel_into_frame(frame: &mut CellFrame, layout: InfoPanelLayout, rows: usize) {
    let panel_style = CellStyle::panel();
    frame.fill(layout.panel_left, 0, frame.width, rows, panel_style);
    for row in 0..rows.min(frame.height) {
        frame.write(
            layout.content_left(),
            row,
            &info_panel_row(row, rows, layout.content_width()),
            CellStyle {
                foreground: tone_rgb(Tone::Muted),
                ..panel_style
            },
        );
    }
}
```

## 3. 직접 출력 경로(비프레임) 페인트

```rust
/// Draw the panel only after conversation rows settle. Clearing from its left
/// edge through the terminal's right edge makes this an overlay, so no fixed
/// width right border can inherit a line's width or partial repaint state.
fn paint_info_panel(out: &mut impl Write, layout: InfoPanelLayout, rows: usize) -> Result<()> {
    let background = blend(theme::palette().background, theme::palette().border, 72);
    for row in 0..rows {
        queue!(
            out,
            MoveTo(
                layout.panel_left.min(u16::MAX as usize) as u16,
                row.min(u16::MAX as usize) as u16
            ),
            SetBackgroundColor(rgb_color(background)),
            Clear(ClearType::UntilNewLine),
            MoveTo(
                layout.content_left().min(u16::MAX as usize) as u16,
                row.min(u16::MAX as usize) as u16
            )
        )?;
        set_tone(out, Tone::Muted)?;
        queue!(
            out,
            Print(info_panel_row(row, rows, layout.content_width())),
            ResetColor
        )?;
    }
    Ok(())
}
```

## 4. 리사이즈·잔상 처리

패널이 열려 있을 때 본문 행 배경이 패널 영역까지 지워지는 문제를 막기 위해
`background_width` 인자가 도입되었다. 현재 `main`에도 이 인자는 남아 있으나 모든 호출부가
`None`을 넘긴다.

```rust
fn info_panel_main_clear_range(
    painted_info_panel: Option<InfoPanelLayout>,
    layout: InfoPanelLayout,
) -> Range<usize> {
    0..if painted_info_panel != Some(layout) {
        layout.panel_left
    } else {
        layout.main_width
    }
}

/// When the terminal narrows, the old fixed-width panel moves left. Its former
/// final cell is now the current frame's autowrap guard, so no normal main or
/// panel paint reaches it. Clear that vacated tail before drawing the new frame.
fn vacated_info_panel_right_clear_start(
    painted_info_panel: Option<InfoPanelLayout>,
    info_panel: Option<InfoPanelLayout>,
) -> Option<usize> {
    let (painted, current) = (painted_info_panel?, info_panel?);
    let painted_right = painted.panel_left + painted.panel_width;
    let current_right = current.panel_left + current.panel_width;
    (painted_right > current_right).then_some(current_right)
}
```

렌더러는 직전에 그린 레이아웃을 `painted_info_panel: Option<InfoPanelLayout>` 필드에 보관하고,
화면을 새로 그릴 때마다 갱신했다. 전체 재도색이 필요한 시점에는 `None`으로 초기화했다.

## 5. 렌더 진입점 배선

```rust
let info_panel = (self.mode == RenderMode::Fullscreen && view.info_panel_open)
    .then(|| info_panel_layout(width))
    .flatten();
let frame_width = info_panel.map_or(width, |layout| layout.main_width);
// ...
paint_line_into_frame(&mut frame, row, line, selected, hovered,
                      info_panel.map(|layout| layout.main_width));
if let Some(layout) = info_panel {
    paint_info_panel_into_frame(&mut frame, layout, lines.len());
}
self.painted_info_panel = info_panel;
```

패널은 풀스크린 모드에서만 열렸다.

## 6. 상태와 토글

```rust
// state.rs
info_panel_open: bool,           // Session 필드, 기본값 false

pub fn toggle_info_panel(&mut self) {
    self.info_panel_open = !self.info_panel_open;
}

// 키 입력: Shift+P
if key.modifiers == KeyModifiers::SHIFT {
    match key.code {
        KeyCode::Char('P') | KeyCode::Char('p') => {
            self.toggle_info_panel();
            return Action::Tick(true);
        }
        // ...
    }
}
```

`info_panel_open`은 `SessionView`와 렌더 뷰 구조체 양쪽에 전달되었다.

상태 줄에도 클릭 가능한 배지가 있었다.

```rust
// main.rs
Pick::InfoPanel => {
    state.toggle_info_panel();
    Action::Tick(true)
}

// renderer.rs 상태 줄
let info_panel_span = PaintSpan { /* mode.info_panel_open 에 따라 라벨 변경 */ };
// info_panel_index: Some(cost_width + 8) 형태로 클릭 영역 지정
```

## 7. 당시 회귀 테스트 목록

재구현 시 같은 시나리오를 다시 덮으면 좋다.

- `info_panel_layout_keeps_a_gap_and_the_last_column_unpainted`
- `info_panel_opening_clear_reaches_panel_but_steady_clear_stops_at_main_frame`
- `shrinking_an_open_panel_clears_its_vacated_rightmost_cell`
- `frame_panel_fill_reaches_the_last_visible_cell_on_every_row`
- `panel_overlay_wins_over_a_full_width_row_background`
- `changing_input_cells_does_not_redraw_the_open_panel_badge`
- `info_panel_overlay_clears_from_its_left_edge_to_the_terminal_edge`
- `bounded_background_row_never_clears_into_the_info_panel`
- `shift_p_toggles_the_info_panel_without_editing_the_composer`
- `clicking_the_panel_badge_toggles_the_info_panel`

## 8. 함께 남은 실험 도구

`tools/opentui-panel-probe/`는 같은 커밋에서 추가된 OpenTUI 패널 렌더링 확인용 Bun 스크립트다.
