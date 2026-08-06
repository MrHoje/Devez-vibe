pub const ATTACHMENT_PLACEHOLDER: char = '\u{fffc}';

/// How many lines a paste needs before the composer shows it as one summary.
const COLLAPSE_MIN_LINES: usize = 6;

#[derive(Default)]
pub struct Editor {
    buffer: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    kill_buffer: String,
    collapsed_paste_lines: Option<usize>,
    collapsed_paste_start: Option<usize>,
    collapsed_paste_end: Option<usize>,
}

impl Editor {
    pub fn is_empty(&self) -> bool {
        self.buffer
            .iter()
            .all(|&ch| ch == ATTACHMENT_PLACEHOLDER)
    }

    pub fn text(&self) -> String {
        self.buffer
            .iter()
            .filter(|&&ch| ch != ATTACHMENT_PLACEHOLDER)
            .collect()
    }

    pub fn display_text(&self) -> String {
        self.buffer.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.buffer[..self.cursor]
            .iter()
            .filter(|&&ch| ch != ATTACHMENT_PLACEHOLDER)
            .count()
    }

    pub fn display_cursor(&self) -> usize {
        self.cursor
    }

    pub fn chars(&self) -> &[char] {
        &self.buffer
    }

    pub fn insert(&mut self, ch: char) {
        self.move_to_collapsed_paste_end();
        self.leave_history();
        if self
            .collapsed_paste_start
            .is_some_and(|start| self.cursor <= start)
        {
            self.collapsed_paste_start = self.collapsed_paste_start.map(|start| start + 1);
            self.collapsed_paste_end = self.collapsed_paste_end.map(|end| end + 1);
        }
        self.buffer.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn insert_attachment(&mut self) -> usize {
        self.move_to_collapsed_paste_end();
        let index = self.buffer[..self.cursor]
            .iter()
            .filter(|&&ch| ch == ATTACHMENT_PLACEHOLDER)
            .count();
        self.insert(ATTACHMENT_PLACEHOLDER);
        index
    }

    pub fn attachment_before_cursor(&self) -> Option<usize> {
        (self.cursor > 0 && self.buffer[self.cursor - 1] == ATTACHMENT_PLACEHOLDER).then(|| {
            self.buffer[..self.cursor - 1]
                .iter()
                .filter(|&&ch| ch == ATTACHMENT_PLACEHOLDER)
                .count()
        })
    }

    pub fn attachment_at_cursor(&self) -> Option<usize> {
        (self.cursor < self.buffer.len() && self.buffer[self.cursor] == ATTACHMENT_PLACEHOLDER)
            .then(|| {
                self.buffer[..self.cursor]
                    .iter()
                    .filter(|&&ch| ch == ATTACHMENT_PLACEHOLDER)
                    .count()
            })
    }

    pub fn insert_str(&mut self, text: &str) {
        self.move_to_collapsed_paste_end();
        self.leave_history();
        for ch in text.chars() {
            self.buffer.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    pub fn insert_paste_str(&mut self, text: &str) {
        if self.expand_collapsed_paste_if_same(text) {
            return;
        }
        self.move_to_collapsed_paste_end();
        let start = self.cursor;
        let lines = text.chars().filter(|&ch| ch == '\n').count() + 1;
        if lines < COLLAPSE_MIN_LINES {
            // Text too short to collapse is ordinary input — on Windows a fast
            // typing run reaches here as a "paste". It must leave the block
            // already collapsed alone, only shifting it when it lands ahead.
            let inserted = text.chars().count();
            self.insert_str(text);
            if self
                .collapsed_paste_start
                .is_some_and(|paste_start| start <= paste_start)
            {
                self.collapsed_paste_start =
                    self.collapsed_paste_start.map(|value| value + inserted);
                self.collapsed_paste_end = self.collapsed_paste_end.map(|value| value + inserted);
            }
            return;
        }
        self.insert_str(text);
        self.collapsed_paste_lines = Some(lines);
        self.collapsed_paste_start = Some(start);
        self.collapsed_paste_end = Some(self.cursor);
    }

    /// Shows a collapsed paste as its full text again. Reports whether one was
    /// collapsed to begin with.
    pub fn expand_collapsed_paste(&mut self) -> bool {
        let collapsed = self.collapsed_paste_lines.is_some();
        self.collapsed_paste_lines = None;
        self.collapsed_paste_start = None;
        self.collapsed_paste_end = None;
        collapsed
    }

    pub fn paste_summary_lines(&self) -> Option<usize> {
        self.collapsed_paste_lines
    }

    pub fn collapsed_paste_text(&self) -> Option<String> {
        let (Some(start), Some(end)) = (self.collapsed_paste_start, self.collapsed_paste_end)
        else {
            return None;
        };
        Some(self.buffer[start..end].iter().collect())
    }

    /// Where a collapsed paste sits in the buffer, so a caller working in
    /// display characters can tell which of them stand for the whole block.
    pub fn collapsed_paste_range(&self) -> Option<std::ops::Range<usize>> {
        self.collapsed_paste_lines?;
        let start = self.collapsed_paste_start?;
        let end = self.collapsed_paste_end?;
        Some(start..end)
    }

    pub fn collapsed_paste_display(&self) -> Option<(String, usize)> {
        let lines = self.paste_summary_lines()?;
        let start = self.collapsed_paste_start.unwrap_or(0);
        let end = self.collapsed_paste_end.unwrap_or(self.buffer.len());
        let prefix = self.buffer[..start].iter().collect::<String>();
        let summary = format!("[Pasted text · {lines} lines]");
        let tail = self.buffer[end..].iter().collect::<String>();
        let cursor = if self.cursor <= start {
            self.cursor
        } else {
            prefix.chars().count()
                + summary.chars().count()
                + self.cursor.saturating_sub(end)
        };
        Some((format!("{prefix}{summary}{tail}"), cursor))
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.remove_collapsed_paste_at_cursor() {
            return;
        }
        if self.cursor > 0 {
            self.leave_history();
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
            if self
                .collapsed_paste_start
                .is_some_and(|start| self.cursor < start)
            {
                self.collapsed_paste_start = self.collapsed_paste_start.map(|start| start - 1);
                self.collapsed_paste_end = self.collapsed_paste_end.map(|end| end - 1);
            }
        }
    }

    pub fn delete(&mut self) {
        if self.remove_collapsed_paste_after_cursor() {
            return;
        }
        self.move_to_collapsed_paste_end();
        if self.cursor < self.buffer.len() {
            self.leave_history();
            self.buffer.remove(self.cursor);
            if self
                .collapsed_paste_start
                .is_some_and(|start| self.cursor < start)
            {
                self.collapsed_paste_start = self.collapsed_paste_start.map(|start| start - 1);
                self.collapsed_paste_end = self.collapsed_paste_end.map(|end| end - 1);
            }
        }
    }

    pub fn move_left(&mut self) {
        if self
            .collapsed_paste_end
            .is_some_and(|end| self.cursor == end)
        {
            self.cursor = self.collapsed_paste_start.unwrap_or(self.cursor);
        } else {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    pub fn move_right(&mut self) {
        if self
            .collapsed_paste_start
            .is_some_and(|start| self.cursor == start)
        {
            self.cursor = self.collapsed_paste_end.unwrap_or(self.cursor);
        } else {
            self.cursor = (self.cursor + 1).min(self.buffer.len());
        }
    }

    pub fn move_home(&mut self) {
        if let Some(end) = self.collapsed_paste_end {
            self.cursor = end;
            return;
        }
        while self.cursor > 0 && self.buffer[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub fn move_end(&mut self) {
        while self.cursor < self.buffer.len() && self.buffer[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    /// Moves within physical input rows. Returns false at the first row so the
    /// caller can fall back to history navigation.
    pub fn move_up(&mut self) -> bool {
        if let Some(end) = self.collapsed_paste_end {
            if self.cursor <= end {
                self.cursor = end;
                return true;
            }
        }
        let line_start = self.buffer[..self.cursor]
            .iter()
            .rposition(|&ch| ch == '\n')
            .map_or(0, |index| index + 1);
        if line_start == 0 {
            return false;
        }
        let previous_end = line_start - 1;
        let previous_start = self.buffer[..previous_end]
            .iter()
            .rposition(|&ch| ch == '\n')
            .map_or(0, |index| index + 1);
        let column = self.cursor - line_start;
        self.cursor = (previous_start + column.min(previous_end - previous_start))
            .max(self.collapsed_paste_end.unwrap_or(0));
        true
    }

    /// Moves within physical input rows. Returns false at the last row so the
    /// caller can fall back to history navigation.
    pub fn move_down(&mut self) -> bool {
        let line_start = self.buffer[..self.cursor]
            .iter()
            .rposition(|&ch| ch == '\n')
            .map_or(0, |index| index + 1);
        let line_end = self.buffer[self.cursor..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(self.buffer.len(), |index| self.cursor + index);
        if line_end == self.buffer.len() {
            return self.collapsed_paste_end.is_some();
        }
        let next_start = line_end + 1;
        let next_end = self.buffer[next_start..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(self.buffer.len(), |index| next_start + index);
        let column = self.cursor - line_start;
        self.cursor = next_start + column.min(next_end - next_start);
        true
    }

    pub fn move_word_left(&mut self) {
        while self.cursor > 0 && self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        self.cursor = self.cursor.max(self.collapsed_paste_end.unwrap_or(0));
    }

    pub fn move_word_right(&mut self) {
        while self.cursor < self.buffer.len() && self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < self.buffer.len() && !self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.draft.clear();
        self.collapsed_paste_lines = None;
        self.collapsed_paste_start = None;
        self.collapsed_paste_end = None;
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.history_index = None;
        self.draft.clear();
        self.replace(text.into());
    }

    pub fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        self.leave_history();
        let start = self.raw_index_for_text_index(range.start);
        let end = self.raw_index_for_text_index(range.end).max(start);
        self.buffer.splice(start..end, text.chars());
        self.cursor = start + text.chars().count();
    }

    /// Removes a span of display characters — what a drag over the composer
    /// highlighted. Reports whether anything was removed. A span that cuts into
    /// a collapsed paste leaves the rest of it expanded: what is left is no
    /// longer the block that was pasted.
    pub fn delete_display_range(&mut self, range: std::ops::Range<usize>) -> bool {
        let start = range.start.min(self.buffer.len());
        let end = range.end.min(self.buffer.len());
        if start >= end {
            return false;
        }
        self.leave_history();
        self.buffer.drain(start..end);
        self.cursor = start;
        if let (Some(paste_start), Some(paste_end)) =
            (self.collapsed_paste_start, self.collapsed_paste_end)
        {
            if start < paste_end && end > paste_start {
                self.collapsed_paste_lines = None;
                self.collapsed_paste_start = None;
                self.collapsed_paste_end = None;
            } else if end <= paste_start {
                let removed = end - start;
                self.collapsed_paste_start = Some(paste_start - removed);
                self.collapsed_paste_end = Some(paste_end - removed);
            }
        }
        true
    }

    pub fn delete_word_left(&mut self) {
        if self.remove_collapsed_paste_at_cursor() {
            return;
        }
        if self.cursor > 0
            && self
            .buffer
            .get(self.cursor - 1)
            .is_some_and(|&ch| ch == ATTACHMENT_PLACEHOLDER)
        {
            self.leave_history();
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
            return;
        }
        if self.cursor == 0
            || self
                .collapsed_paste_end
                .is_some_and(|end| self.cursor <= end)
        {
            return;
        }
        self.leave_history();
        let end = self.cursor;
        self.move_word_left_for_delete();
        self.kill_buffer = self.buffer[self.cursor..end].iter().collect();
        self.buffer.drain(self.cursor..end);
    }

    fn move_word_left_for_delete(&mut self) {
        while self.cursor > 0 && self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        if self.cursor > 0 && matches!(self.buffer[self.cursor - 1], '/' | '\\') {
            self.cursor -= 1;
        }
        while self.cursor > 0
            && !self.buffer[self.cursor - 1].is_whitespace()
            && !matches!(self.buffer[self.cursor - 1], '/' | '\\')
        {
            self.cursor -= 1;
        }
        self.cursor = self.cursor.max(self.collapsed_paste_end.unwrap_or(0));
    }

    pub fn delete_word_right(&mut self) {
        if self.remove_collapsed_paste_after_cursor() {
            return;
        }
        if self
            .buffer
            .get(self.cursor)
            .is_some_and(|&ch| ch == ATTACHMENT_PLACEHOLDER)
        {
            self.leave_history();
            self.buffer.remove(self.cursor);
            if self
                .collapsed_paste_start
                .is_some_and(|start| self.cursor < start)
            {
                self.collapsed_paste_start = self.collapsed_paste_start.map(|start| start - 1);
                self.collapsed_paste_end = self.collapsed_paste_end.map(|end| end - 1);
            }
            return;
        }
        self.move_to_collapsed_paste_end();
        if self.cursor == self.buffer.len() {
            return;
        }

        self.leave_history();
        let start = self.cursor;
        let limit = self.collapsed_paste_start.unwrap_or(self.buffer.len());
        while self.cursor < limit && self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < limit && !self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < limit && self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        let end = self.cursor;
        if start == end {
            return;
        }
        self.kill_buffer = self.buffer[start..end].iter().collect();
        self.buffer.drain(start..end);
        if self.collapsed_paste_start.is_some_and(|paste_start| start < paste_start) {
            let removed = end - start;
            self.collapsed_paste_start = self.collapsed_paste_start.map(|value| value - removed);
            self.collapsed_paste_end = self.collapsed_paste_end.map(|value| value - removed);
        }
        self.cursor = start;
    }

    pub fn delete_to_line_end(&mut self) {
        self.move_to_collapsed_paste_end();
        self.leave_history();
        let mut end = self.cursor;
        while end < self.buffer.len() && self.buffer[end] != '\n' {
            end += 1;
        }
        if end == self.cursor && end < self.buffer.len() {
            end += 1;
        }
        if end == self.cursor {
            return;
        }
        self.kill_buffer = self.buffer[self.cursor..end].iter().collect();
        self.buffer.drain(self.cursor..end);
    }

    pub fn delete_to_line_start(&mut self) {
        if self.cursor == 0
            || self
                .collapsed_paste_end
                .is_some_and(|end| self.cursor <= end)
        {
            return;
        }
        self.leave_history();
        let mut start = self.cursor;
        while start > 0 && self.buffer[start - 1] != '\n' {
            start -= 1;
        }
        if start == self.cursor && start > 0 {
            start -= 1;
        }
        self.kill_buffer = self.buffer[start..self.cursor].iter().collect();
        self.buffer.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let killed = self.kill_buffer.clone();
        self.insert_str(&killed);
    }

    pub fn take_for_submit(&mut self) -> Option<String> {
        let text = self.text();
        if text.trim().is_empty() {
            self.clear();
            return None;
        }

        let is_slash_command = text.starts_with('/') && !text.contains('\n');
        if !is_slash_command && self.history.last().is_none_or(|last| last != &text) {
            self.history.push(text.clone());
            if self.history.len() > 100 {
                self.history.remove(0);
            }
        }
        self.clear();
        Some(text)
    }

    /// Where the recalled entry sits, newest first, as `(position, total)`.
    /// `None` unless the composer is currently showing history.
    pub fn history_position(&self) -> Option<(usize, usize)> {
        self.history_index
            .map(|index| (index + 1, self.history.len()))
    }

    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }

        let next = match self.history_index {
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_index = Some(next);
        self.replace(self.history[next].clone());
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_index = Some(next);
            self.replace(self.history[next].clone());
        } else {
            self.history_index = None;
            self.replace(self.draft.clone());
        }
    }

    fn replace(&mut self, text: String) {
        self.collapsed_paste_lines = None;
        self.collapsed_paste_start = None;
        self.collapsed_paste_end = None;
        self.buffer = text.chars().collect();
        self.cursor = self.buffer.len();
    }

    fn leave_history(&mut self) {
        if self.history_index.take().is_some() {
            self.draft.clear();
        }
    }

    fn raw_index_for_text_index(&self, text_index: usize) -> usize {
        let mut visible = 0;
        for (index, &ch) in self.buffer.iter().enumerate() {
            if ch == ATTACHMENT_PLACEHOLDER {
                continue;
            }
            if visible == text_index {
                return index;
            }
            visible += 1;
        }
        self.buffer.len()
    }

    fn move_to_collapsed_paste_end(&mut self) {
        if let (Some(start), Some(end)) =
            (self.collapsed_paste_start, self.collapsed_paste_end)
        {
            if self.cursor > start && self.cursor < end {
                self.cursor = end;
            }
        }
    }

    fn expand_collapsed_paste_if_same(&mut self, text: &str) -> bool {
        let (Some(start), Some(end)) = (self.collapsed_paste_start, self.collapsed_paste_end)
        else {
            return false;
        };
        // A block pasted with CRLF comes back from the terminal as bare
        // newlines, so the carriage returns cannot decide whether this is the
        // same paste arriving a second time.
        if self.buffer[start..end]
            .iter()
            .copied()
            .filter(|&ch| ch != '\r')
            .eq(text.chars().filter(|&ch| ch != '\r'))
        {
            self.collapsed_paste_lines = None;
            self.collapsed_paste_start = None;
            self.collapsed_paste_end = None;
            return true;
        }
        false
    }

    fn remove_collapsed_paste_at_cursor(&mut self) -> bool {
        let (Some(start), Some(end)) = (self.collapsed_paste_start, self.collapsed_paste_end)
        else {
            return false;
        };
        if self.cursor != end {
            return false;
        }
        self.leave_history();
        self.buffer.drain(start..end);
        self.cursor = start;
        self.collapsed_paste_lines = None;
        self.collapsed_paste_start = None;
        self.collapsed_paste_end = None;
        true
    }

    fn remove_collapsed_paste_after_cursor(&mut self) -> bool {
        let (Some(start), Some(end)) = (self.collapsed_paste_start, self.collapsed_paste_end)
        else {
            return false;
        };
        if self.cursor != start {
            return false;
        }
        self.leave_history();
        self.buffer.drain(start..end);
        self.cursor = start;
        self.collapsed_paste_lines = None;
        self.collapsed_paste_start = None;
        self.collapsed_paste_end = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::Editor;

    #[test]
    fn delete_word_left_can_be_yanked_back() {
        let mut editor = Editor::default();
        editor.set_text("alpha beta");

        editor.delete_word_left();
        assert_eq!(editor.text(), "alpha ");

        editor.yank();
        assert_eq!(editor.text(), "alpha beta");
    }

    #[test]
    fn delete_word_left_removes_path_segments_one_at_a_time() {
        let mut editor = Editor::default();
        editor.set_text("C:/Source/devezcode");

        editor.delete_word_left();
        assert_eq!(editor.text(), "C:/Source/");

        editor.delete_word_left();
        assert_eq!(editor.text(), "C:/");

        editor.delete_word_left();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn delete_word_right_removes_the_next_word_and_spacing() {
        let mut editor = Editor::default();
        editor.set_text("alpha beta gamma");
        editor.move_home();
        editor.move_right();
        editor.move_right();
        editor.move_right();
        editor.move_right();
        editor.move_right();

        editor.delete_word_right();

        assert_eq!(editor.text(), "alphagamma");
        editor.yank();
        assert_eq!(editor.text(), "alpha beta gamma");
    }

    #[test]
    fn history_recall_reports_its_position_newest_first() {
        let mut editor = Editor::default();
        for prompt in ["first", "second", "third"] {
            editor.set_text(prompt);
            editor.take_for_submit();
        }
        editor.set_text("a draft");

        assert_eq!(editor.history_position(), None, "not browsing yet");

        editor.history_previous();
        assert_eq!(editor.text(), "third");
        assert_eq!(editor.history_position(), Some((3, 3)));

        editor.history_previous();
        editor.history_previous();
        assert_eq!(editor.text(), "first");
        assert_eq!(editor.history_position(), Some((1, 3)));

        // Walking back past the newest entry restores the draft, not a position.
        for _ in 0..3 {
            editor.history_next();
        }
        assert_eq!(editor.text(), "a draft");
        assert_eq!(editor.history_position(), None);

        editor.history_previous();
        editor.insert('!');
        assert_eq!(
            editor.history_position(),
            None,
            "editing leaves history behind"
        );
    }

    #[test]
    fn slash_commands_are_not_added_to_prompt_history() {
        let mut editor = Editor::default();
        for text in ["/help", "a real prompt", "/status"] {
            editor.set_text(text);
            editor.take_for_submit();
        }

        editor.history_previous();

        assert_eq!(editor.text(), "a real prompt");
        assert_eq!(editor.history_position(), Some((1, 1)));
    }

    #[test]
    fn line_kill_commands_preserve_multiline_boundaries() {
        let mut editor = Editor::default();
        editor.set_text("alpha\nbeta");
        editor.move_home();

        editor.delete_to_line_start();
        assert_eq!(editor.text(), "alphabeta");
        editor.yank();
        assert_eq!(editor.text(), "alpha\nbeta");

        editor.set_text("alpha\nbeta");
        editor.move_home();
        editor.delete_to_line_end();
        assert_eq!(editor.text(), "alpha\n");

        editor.yank();
        assert_eq!(editor.text(), "alpha\nbeta");
    }

    #[test]
    fn replace_range_preserves_the_surrounding_multiline_draft() {
        let mut editor = Editor::default();
        editor.set_text("open @mai\nthen continue");

        editor.replace_range(5..9, "src/main.rs");

        assert_eq!(editor.text(), "open src/main.rs\nthen continue");
        assert_eq!(editor.cursor(), 16);
    }

    #[test]
    fn up_and_down_stay_within_multiline_input_rows() {
        let mut editor = Editor::default();
        editor.set_text("first\nsecond\nthird");
        editor.move_home();

        assert!(editor.move_up());
        assert_eq!(editor.cursor(), 6, "up moves from the last row to the middle row");
        assert!(editor.move_up());
        assert_eq!(editor.cursor(), 0, "up moves from the middle row to the first row");
        assert!(!editor.move_up(), "the first row leaves up for history navigation");

        assert!(editor.move_down());
        assert_eq!(editor.cursor(), 6);
        assert!(editor.move_down());
        assert_eq!(editor.cursor(), 13);
        assert!(!editor.move_down(), "the last row leaves down for history navigation");
    }

    #[test]
    fn a_large_paste_stays_collapsed_while_the_user_edits_its_tail() {
        let mut editor = Editor::default();
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");

        assert_eq!(editor.paste_summary_lines(), Some(6));
        assert_eq!(editor.text(), "one\ntwo\nthree\nfour\nfive\nsix");

        editor.insert('!');
        assert_eq!(editor.paste_summary_lines(), Some(6));
        assert_eq!(editor.text(), "one\ntwo\nthree\nfour\nfive\nsix!");
    }

    #[test]
    fn expanding_a_collapsed_paste_keeps_the_text_and_drops_the_summary() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix";
        let mut editor = Editor::default();
        editor.insert_paste_str(text);

        assert!(editor.expand_collapsed_paste());

        assert_eq!(editor.paste_summary_lines(), None);
        assert_eq!(editor.collapsed_paste_display(), None);
        assert_eq!(editor.text(), text);
        assert!(!editor.expand_collapsed_paste(), "nothing left to expand");
    }

    #[test]
    fn a_short_burst_after_a_paste_keeps_the_block_collapsed() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix";
        let mut editor = Editor::default();
        editor.insert_paste_str(text);
        let range = editor.collapsed_paste_range().expect("collapsed");

        // Windows reports a fast typing run as a paste; it is not the block.
        editor.insert_paste_str(" more");

        assert_eq!(editor.paste_summary_lines(), Some(6));
        assert_eq!(editor.collapsed_paste_range(), Some(range));
        assert_eq!(editor.text(), format!("{text} more"));
    }

    #[test]
    fn a_short_burst_before_a_paste_shifts_the_collapsed_block() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix";
        let mut editor = Editor::default();
        editor.insert_paste_str(text);
        editor.move_home();
        editor.move_left();

        editor.insert_paste_str("ab");

        assert_eq!(editor.paste_summary_lines(), Some(6));
        assert_eq!(editor.collapsed_paste_range(), Some(2..2 + text.chars().count()));
        assert_eq!(editor.text(), format!("ab{text}"));
    }

    #[test]
    fn only_pasting_the_same_block_again_expands_it() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix";
        let mut editor = Editor::default();
        editor.insert_paste_str(text);

        editor.insert(' ');
        editor.move_left();
        editor.move_right();
        assert_eq!(editor.paste_summary_lines(), Some(6));
        assert_eq!(editor.text(), format!("{text} "));

        editor.insert_paste_str(text);
        assert_eq!(editor.paste_summary_lines(), None);
        assert_eq!(
            editor.text(),
            format!("{text} "),
            "the second paste only expands"
        );
    }

    #[test]
    fn backspace_at_a_collapsed_paste_removes_the_whole_paste_once() {
        let mut editor = Editor::default();
        editor.set_text("before ");
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");

        editor.backspace();

        assert_eq!(editor.text(), "before ");
        assert_eq!(editor.paste_summary_lines(), None);
        assert_eq!(editor.cursor(), "before ".chars().count());
    }

    #[test]
    fn deleting_a_display_range_leaves_the_cursor_where_it_started() {
        let mut editor = Editor::default();
        editor.set_text("alpha beta gamma");

        assert!(editor.delete_display_range(6..11));

        assert_eq!(editor.text(), "alpha gamma");
        assert_eq!(editor.cursor(), 6);
        assert!(!editor.delete_display_range(4..4), "an empty range is a no-op");
    }

    #[test]
    fn deleting_a_display_range_over_a_collapsed_paste_expands_what_is_left() {
        let mut editor = Editor::default();
        editor.set_text("before ");
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");
        let paste = editor.collapsed_paste_range().expect("collapsed");

        // Half the block: what remains is no longer the paste that arrived.
        assert!(editor.delete_display_range(paste.start..paste.start + 4));

        assert_eq!(editor.text(), "before two\nthree\nfour\nfive\nsix");
        assert_eq!(editor.paste_summary_lines(), None);
    }

    #[test]
    fn deleting_a_display_range_before_a_collapsed_paste_keeps_it_collapsed() {
        let mut editor = Editor::default();
        editor.set_text("before ");
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");

        assert!(editor.delete_display_range(0..7));

        assert_eq!(editor.paste_summary_lines(), Some(6));
        assert_eq!(editor.collapsed_paste_range(), Some(0..27));
    }

    #[test]
    fn word_delete_removes_a_collapsed_paste_as_one_item() {
        let mut editor = Editor::default();
        editor.set_text("before ");
        editor.insert_paste_str("one\ntwo\nthree\nfour\nfive\nsix");

        editor.delete_word_left();

        assert_eq!(editor.text(), "before ");
        assert_eq!(editor.paste_summary_lines(), None);
    }
}
