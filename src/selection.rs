use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CellPosition {
    pub row: usize,
    pub column: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellRange {
    pub start: CellPosition,
    pub end: CellPosition,
}

impl CellRange {
    pub fn columns_for_row(self, row: usize, line_width: usize) -> Option<Range<usize>> {
        if row < self.start.row || row > self.end.row {
            return None;
        }
        let start = if row == self.start.row {
            usize::from(self.start.column)
        } else {
            0
        }
        .min(line_width);
        let end = if row == self.end.row {
            usize::from(self.end.column).saturating_add(1)
        } else {
            line_width
        }
        .min(line_width);
        (start < end).then_some(start..end)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionFinish {
    Copy(CellRange),
    /// A press and release on the same cell. The cell itself is reported, not
    /// just its row: chrome like the composer badges is clickable per span.
    Click(CellPosition),
    None,
}

#[derive(Default)]
pub(crate) struct Selection {
    anchor: Option<CellPosition>,
    focus: Option<CellPosition>,
    dragging: bool,
    moved: bool,
}

impl Selection {
    pub fn range(&self) -> Option<CellRange> {
        let anchor = self.anchor?;
        let focus = self.focus?;
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        Some(CellRange { start, end })
    }

    pub const fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Where the drag last stood. A release the renderer cannot resolve to a
    /// cell finishes here rather than leaving the drag open.
    pub const fn focus(&self) -> Option<CellPosition> {
        self.focus
    }

    pub fn begin(&mut self, point: CellPosition) {
        self.anchor = Some(point);
        self.focus = Some(point);
        self.dragging = true;
        self.moved = false;
    }

    pub fn update(&mut self, point: CellPosition) -> bool {
        if !self.dragging || self.anchor.is_none() || self.focus == Some(point) {
            return false;
        }
        self.focus = Some(point);
        self.moved = true;
        true
    }

    pub fn set_range(&mut self, range: CellRange) {
        self.anchor = Some(range.start);
        self.focus = Some(range.end);
        self.dragging = false;
        self.moved = true;
    }

    pub fn finish(&mut self, point: CellPosition) -> SelectionFinish {
        let Some(anchor) = self.anchor else {
            return SelectionFinish::None;
        };
        if !self.dragging {
            return SelectionFinish::None;
        }
        self.focus = Some(point);
        self.dragging = false;
        if !self.moved && anchor == point {
            self.anchor = None;
            self.focus = None;
            return SelectionFinish::Click(point);
        }
        let (start, end) = if anchor <= point {
            (anchor, point)
        } else {
            (point, anchor)
        };
        SelectionFinish::Copy(CellRange { start, end })
    }

    pub fn clear(&mut self) -> bool {
        let changed = self.anchor.is_some();
        self.anchor = None;
        self.focus = None;
        self.dragging = false;
        self.moved = false;
        changed
    }
}

pub(crate) struct CopyLine {
    pub text: String,
    pub join_next: bool,
    pub marker_width: usize,
    pub prefix_width: usize,
    pub content_columns: Option<Range<usize>>,
}

pub(crate) fn extract_text(lines: &[CopyLine], range: CellRange) -> String {
    let mut output = String::new();
    let mut previous_row: Option<usize> = None;

    for row in range.start.row..=range.end.row {
        let Some(line) = lines.get(row) else {
            break;
        };
        let width = UnicodeWidthStr::width(line.text.as_str());

        if let Some(previous) = previous_row
            && !lines[previous].join_next
        {
            output.push('\n');
        }

        if let Some(mut columns) = range.columns_for_row(row, width) {
            if let Some(content) = &line.content_columns {
                columns.start = columns.start.max(content.start);
                columns.end = columns.end.min(content.end);
            }
            if columns.start >= columns.end {
                previous_row = Some(row);
                continue;
            }
            let continuation_width = if row > 0 && lines[row - 1].join_next {
                line.prefix_width
            } else {
                0
            };
            let skip_width = line.marker_width.max(continuation_width);
            if columns.start == 0 && columns.end >= skip_width {
                columns.start = skip_width;
            }
            output.push_str(&slice_cells(&line.text, columns));
        }
        previous_row = Some(row);
    }

    output
}

fn slice_cells(text: &str, range: Range<usize>) -> String {
    let mut output = String::new();
    let mut column = 0;
    let mut previous_was_selected = false;

    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width == 0 {
            if previous_was_selected {
                output.push(ch);
            }
            continue;
        }

        let selected = column < range.end && column + width > range.start;
        if selected {
            output.push(ch);
        }
        previous_was_selected = selected;
        column += width;
    }

    output
}

/// How many characters `columns` covers in `text`, counting no further than
/// `limit` so a drag across a whole transcript stops after the first couple.
/// Zero-width marks ride along with the character they attach to, and a wide
/// character counts once even though it fills two cells — the caller wants
/// glyphs, not cells.
pub(crate) fn selected_char_count(text: &str, columns: &Range<usize>, limit: usize) -> usize {
    let mut count = 0;
    let mut column = 0;

    for ch in text.chars() {
        if count >= limit {
            break;
        }
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width == 0 {
            continue;
        }
        if column < columns.end && column + width > columns.start {
            count += 1;
        }
        column += width;
    }

    count
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SelectionChunk {
    pub text: String,
    pub selected: bool,
}

pub(crate) fn selection_chunks(
    text: &str,
    start_column: usize,
    selected_columns: Option<Range<usize>>,
) -> Vec<SelectionChunk> {
    let mut chunks: Vec<SelectionChunk> = Vec::new();
    let mut column = start_column;

    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let selected = if width == 0 {
            chunks.last().is_some_and(|chunk| chunk.selected)
        } else {
            selected_columns
                .as_ref()
                .is_some_and(|range| column < range.end && column + width > range.start)
        };

        if let Some(last) = chunks.last_mut()
            && last.selected == selected
        {
            last.text.push(ch);
        } else {
            chunks.push(SelectionChunk {
                text: ch.to_string(),
                selected,
            });
        }
        column += width;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(column: u16, row: usize) -> CellPosition {
        CellPosition { column, row }
    }

    #[test]
    fn drag_normalizes_both_directions_and_includes_the_focus_cell() {
        for (anchor, focus) in [(point(2, 0), point(4, 0)), (point(4, 0), point(2, 0))] {
            let mut selection = Selection::default();
            assert_eq!(selection.range(), None);
            selection.begin(anchor);
            assert!(selection.is_dragging());
            selection.update(focus);

            assert_eq!(
                selection.range(),
                Some(CellRange {
                    start: point(2, 0),
                    end: point(4, 0),
                })
            );
            assert_eq!(
                selection.finish(focus),
                SelectionFinish::Copy(CellRange {
                    start: point(2, 0),
                    end: point(4, 0),
                })
            );
            assert!(!selection.is_dragging());
        }
    }

    #[test]
    fn multiline_drag_normalizes_by_row_before_column() {
        let mut selection = Selection::default();
        selection.begin(point(1, 2));
        selection.update(point(4, 0));

        assert_eq!(
            selection.finish(point(4, 0)),
            SelectionFinish::Copy(CellRange {
                start: point(4, 0),
                end: point(1, 2),
            })
        );
    }

    #[test]
    fn same_cell_release_is_a_click_not_a_copy() {
        let mut selection = Selection::default();
        selection.begin(point(2, 7));

        assert_eq!(
            selection.finish(point(2, 7)),
            SelectionFinish::Click(point(2, 7))
        );
    }

    #[test]
    fn returning_to_the_anchor_after_dragging_is_not_a_click() {
        let mut selection = Selection::default();
        selection.begin(point(2, 7));
        selection.update(point(4, 7));
        selection.update(point(2, 7));

        assert_eq!(
            selection.finish(point(2, 7)),
            SelectionFinish::Copy(CellRange {
                start: point(2, 7),
                end: point(2, 7),
            })
        );
    }

    #[test]
    fn char_count_measures_glyphs_and_stops_at_the_limit() {
        // A wide character spans two cells but is still one glyph, and a
        // combining mark rides along with its base.
        assert_eq!(selected_char_count("한글", &(0..2), 2), 1);
        assert_eq!(selected_char_count("한글", &(0..4), 2), 2);
        assert_eq!(selected_char_count("e\u{301}x", &(0..1), 2), 1);
        assert_eq!(selected_char_count("abcdef", &(1..6), 2), 2);
        assert_eq!(selected_char_count("abcdef", &(2..3), 2), 1);
        assert_eq!(selected_char_count("abcdef", &(0..0), 2), 0);
    }

    #[test]
    fn clear_discards_a_drag_in_progress() {
        let mut selection = Selection::default();
        selection.begin(point(1, 2));
        selection.update(point(3, 4));

        assert!(selection.clear());
        assert_eq!(selection.finish(point(3, 4)), SelectionFinish::None);
        assert!(!selection.clear());
    }

    fn line(text: &str) -> CopyLine {
        CopyLine {
            text: text.to_owned(),
            join_next: false,
            marker_width: 0,
            prefix_width: 0,
            content_columns: None,
        }
    }

    #[test]
    fn extract_single_line_uses_inclusive_cell_endpoints() {
        let lines = vec![line("abcdef")];

        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(2, 0),
                    end: point(4, 0),
                }
            ),
            "cde"
        );
    }

    #[test]
    fn extract_wide_and_combining_characters_as_whole_units() {
        let lines = vec![line("A한B"), line("e\u{301}x")];

        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(2, 0),
                    end: point(2, 0),
                }
            ),
            "한"
        );
        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(0, 1),
                    end: point(0, 1),
                }
            ),
            "e\u{301}"
        );
    }

    #[test]
    fn extract_joins_visual_wraps_and_removes_continuation_padding() {
        let lines = vec![
            CopyLine {
                text: "● hello ".to_owned(),
                join_next: true,
                marker_width: 2,
                prefix_width: 2,
                content_columns: None,
            },
            CopyLine {
                text: "  world".to_owned(),
                join_next: false,
                marker_width: 0,
                prefix_width: 2,
                content_columns: None,
            },
            line("next"),
        ];

        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(0, 0),
                    end: point(6, 2),
                }
            ),
            "hello world\nnext"
        );
    }

    #[test]
    fn extract_preserves_blank_rows_inside_a_multiline_selection() {
        let lines = vec![line("one"), line(""), line("three")];

        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(0, 0),
                    end: point(4, 2),
                }
            ),
            "one\n\nthree"
        );
    }

    #[test]
    fn extract_removes_only_a_fully_selected_decorative_marker() {
        let lines = vec![CopyLine {
            text: "● hello".to_owned(),
            join_next: false,
            marker_width: 2,
            prefix_width: 2,
            content_columns: None,
        }];

        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(0, 0),
                    end: point(0, 0),
                }
            ),
            "●"
        );
        assert_eq!(
            extract_text(
                &lines,
                CellRange {
                    start: point(0, 0),
                    end: point(4, 0),
                }
            ),
            "hel"
        );
    }

    #[test]
    fn selection_chunks_keep_wide_and_combining_characters_intact() {
        assert_eq!(
            selection_chunks("ab한cd", 0, Some(2..4)),
            vec![
                SelectionChunk {
                    text: "ab".to_owned(),
                    selected: false,
                },
                SelectionChunk {
                    text: "한".to_owned(),
                    selected: true,
                },
                SelectionChunk {
                    text: "cd".to_owned(),
                    selected: false,
                },
            ]
        );
        assert_eq!(
            selection_chunks("e\u{301}x", 0, Some(0..1)),
            vec![
                SelectionChunk {
                    text: "e\u{301}".to_owned(),
                    selected: true,
                },
                SelectionChunk {
                    text: "x".to_owned(),
                    selected: false,
                },
            ]
        );
    }
}
