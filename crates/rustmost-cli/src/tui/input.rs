//! A small multi-line text editor for the message box.

#[derive(Debug, Default, Clone)]
pub struct LineEditor {
    chars: Vec<char>,
    cursor: usize,
}

impl LineEditor {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Cursor position as a char index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        let c = if c == '\t' { ' ' } else { c };
        if c == '\r' {
            return;
        }
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n");
        for c in s.chars() {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Start of the current line.
    pub fn home(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    /// End of the current line.
    pub fn end(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    /// Delete from the start of the line to the cursor (Ctrl-U).
    pub fn kill_to_start(&mut self) {
        let end = self.cursor;
        self.home();
        self.chars.drain(self.cursor..end);
    }

    /// Delete from the cursor to the end of the line.
    #[cfg(test)]
    pub fn kill_to_end(&mut self) {
        let start = self.cursor;
        self.end();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Delete the word before the cursor (Ctrl-W).
    pub fn delete_word(&mut self) {
        let end = self.cursor;
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        self.chars.drain(self.cursor..end);
    }

    pub fn word_left(&mut self) {
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_alphanumeric() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && self.chars[self.cursor - 1].is_alphanumeric() {
            self.cursor -= 1;
        }
    }

    pub fn word_right(&mut self) {
        let n = self.chars.len();
        while self.cursor < n && !self.chars[self.cursor].is_alphanumeric() {
            self.cursor += 1;
        }
        while self.cursor < n && self.chars[self.cursor].is_alphanumeric() {
            self.cursor += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(s: &str) -> LineEditor {
        let mut e = LineEditor::default();
        e.insert_str(s);
        e
    }

    #[test]
    fn editing() {
        let mut e = ed("hello world");
        e.delete_word();
        assert_eq!(e.text(), "hello ");
        e.insert_str("there");
        e.home();
        e.insert('>');
        assert_eq!(e.text(), ">hello there");
        e.end();
        e.backspace();
        assert_eq!(e.text(), ">hello ther");
        e.home();
        e.kill_to_end();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn multiline_home_end_and_unicode() {
        let mut e = ed("one\ntwo é");
        e.home();
        assert_eq!(e.cursor(), 4);
        e.kill_to_end();
        assert_eq!(e.text(), "one\n");
        e.insert_str("ünï\r\ncode");
        assert_eq!(e.text(), "one\nünï\ncode");
        e.left();
        e.left();
        e.kill_to_start();
        assert_eq!(e.text(), "one\nünï\nde");
        e.clear();
        assert!(e.is_empty());
    }
}
