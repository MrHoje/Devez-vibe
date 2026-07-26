#[derive(Default)]
pub struct Editor {
    buffer: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    kill_buffer: String,
}

impl Editor {
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn chars(&self) -> &[char] {
        &self.buffer
    }

    pub fn insert(&mut self, ch: char) {
        self.leave_history();
        self.buffer.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        self.leave_history();
        for ch in text.chars() {
            self.buffer.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.leave_history();
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
        }
    }


    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.leave_history();
            self.buffer.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buffer.len());
    }

    pub fn move_home(&mut self) {
        while self.cursor > 0 && self.buffer[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub fn move_end(&mut self) {
        while self.cursor < self.buffer.len() && self.buffer[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    pub fn move_word_left(&mut self) {
        while self.cursor > 0 && self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
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
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.history_index = None;
        self.draft.clear();
        self.replace(text.into());
    }

    pub fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        self.leave_history();
        let start = range.start.min(self.buffer.len());
        let end = range.end.min(self.buffer.len()).max(start);
        self.buffer.splice(start..end, text.chars());
        self.cursor = start + text.chars().count();
    }

    pub fn delete_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.leave_history();
        let end = self.cursor;
        self.move_word_left();
        self.kill_buffer = self.buffer[self.cursor..end].iter().collect();
        self.buffer.drain(self.cursor..end);
    }

    pub fn delete_to_line_end(&mut self) {
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
        if self.cursor == 0 {
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
        self.buffer = text.chars().collect();
        self.cursor = self.buffer.len();
    }

    fn leave_history(&mut self) {
        if self.history_index.take().is_some() {
            self.draft.clear();
        }
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

}
