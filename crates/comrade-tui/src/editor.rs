//! Cursor-based, multi-line prompt editing for the message input.
//!
//! The editor stores the buffer as a plain `String` plus a byte cursor
//! (always on a char boundary) and an optional selection anchor. All motion
//! and deletion is expressed in byte offsets so the rest of the crate can map
//! the cursor to a screen cell without re-parsing text.

use unicode_width::UnicodeWidthChar;

/// A cursor-based text buffer with optional selection.
#[derive(Debug, Default)]
pub struct Editor {
    buf: String,
    /// Byte offset of the cursor; always on a char boundary, `0..=len`.
    cur: usize,
    /// Selection anchor (byte offset). `Some(a)` with `a != cur` means the
    /// range `min(a, cur)..max(a, cur)` is selected.
    anchor: Option<usize>,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The full buffer contents.
    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Cursor position as a byte offset.
    pub fn cursor(&self) -> usize {
        self.cur
    }

    /// Take the buffer contents (clearing the editor), used on submit.
    pub fn take_text(&mut self) -> String {
        let out = std::mem::take(&mut self.buf);
        self.cur = 0;
        self.anchor = None;
        out
    }

    /// The currently selected byte range `(start, end)`, if any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.cur {
            return None;
        }
        Some(if a < self.cur {
            (a, self.cur)
        } else {
            (self.cur, a)
        })
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(a, b)| &self.buf[a..b])
    }

    fn move_cursor_to(&mut self, target: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cur);
            }
        } else {
            self.anchor = None;
        }
        self.cur = target;
        // A collapsed selection is no selection.
        if self.anchor == Some(self.cur) {
            self.anchor = None;
        }
    }

    /// Remove the selection (if any) and move the cursor to its start.
    fn take_selection(&mut self) -> Option<(usize, usize)> {
        let (a, b) = self.selection()?;
        self.buf.replace_range(a..b, "");
        self.cur = a;
        self.anchor = None;
        Some((a, b))
    }

    /// Insert a character at the cursor, replacing any selection.
    pub fn insert(&mut self, ch: char) {
        let at = match self.take_selection() {
            Some((a, _)) => a,
            None => self.cur,
        };
        self.buf.insert(at, ch);
        self.cur = at + ch.len_utf8();
    }

    /// Insert `text` at the cursor, replacing any selection. Paste.
    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let at = match self.take_selection() {
            Some((a, _)) => a,
            None => self.cur,
        };
        self.buf.insert_str(at, text);
        self.cur = at + text.len();
    }

    /// Delete the selection and return the removed text; `None` when there is
    /// no selection. The cursor lands where the selection started. Cut.
    pub fn cut_selection(&mut self) -> Option<String> {
        let (a, b) = self.selection()?;
        let out = self.buf[a..b].to_string();
        self.buf.replace_range(a..b, "");
        self.cur = a;
        self.anchor = None;
        Some(out)
    }

    /// Emacs-style kill-line: cut the active selection, otherwise cut from
    /// the cursor to the end of the line. At the end of a line (cursor right
    /// before a `\n`) the newline itself is cut, joining the next line; at
    /// the end of the buffer nothing is cut. Returns the killed text, or
    /// `None` when there was nothing to kill.
    pub fn kill_line(&mut self) -> Option<String> {
        if let Some(out) = self.cut_selection() {
            return Some(out);
        }
        let cur = self.cur;
        match self.buf[cur..].find('\n') {
            Some(0) => {
                // Cursor sits at the end of a line: cut the newline itself.
                self.buf.remove(cur);
                self.anchor = None;
                Some("\n".to_string())
            }
            Some(rel) => {
                let end = cur + rel;
                let out = self.buf[cur..end].to_string();
                self.buf.replace_range(cur..end, "");
                self.anchor = None;
                Some(out)
            }
            None => {
                if cur == self.buf.len() {
                    return None;
                }
                let out = self.buf[cur..].to_string();
                self.buf.truncate(cur);
                self.anchor = None;
                Some(out)
            }
        }
    }

    /// Delete the character before the cursor (or the whole selection).
    pub fn backspace(&mut self) {
        if self.selection().is_some() {
            self.take_selection();
            return;
        }
        if self.cur == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.buf, self.cur);
        self.buf.replace_range(prev..self.cur, "");
        self.cur = prev;
    }

    /// Delete the character at (after) the cursor (or the whole selection).
    pub fn delete(&mut self) {
        if self.selection().is_some() {
            self.take_selection();
            return;
        }
        if self.cur >= self.buf.len() {
            return;
        }
        let next = next_char_boundary(&self.buf, self.cur);
        self.buf.replace_range(self.cur..next, "");
    }

    /// Delete the whole word before the cursor (or the whole selection).
    pub fn backspace_word(&mut self) {
        if self.selection().is_some() {
            self.take_selection();
            return;
        }
        if self.cur == 0 {
            return;
        }
        let start = prev_word_start(&self.buf, self.cur);
        self.buf.replace_range(start..self.cur, "");
        self.cur = start;
    }

    /// Delete the whole word after the cursor (or the whole selection).
    pub fn delete_word(&mut self) {
        if self.selection().is_some() {
            self.take_selection();
            return;
        }
        if self.cur >= self.buf.len() {
            return;
        }
        let end = next_word_end(&self.buf, self.cur);
        self.buf.replace_range(self.cur..end, "");
    }

    pub fn move_left(&mut self, extend: bool) {
        let t = prev_char_boundary(&self.buf, self.cur);
        self.move_cursor_to(t, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        let t = next_char_boundary(&self.buf, self.cur);
        self.move_cursor_to(t, extend);
    }

    pub fn move_word_left(&mut self, extend: bool) {
        let t = prev_word_start(&self.buf, self.cur);
        self.move_cursor_to(t, extend);
    }

    pub fn move_word_right(&mut self, extend: bool) {
        let t = next_word_end(&self.buf, self.cur);
        self.move_cursor_to(t, extend);
    }

    /// Move to the start of the current line (just after the last newline).
    pub fn move_home(&mut self, extend: bool) {
        let start = line_start(&self.buf, self.cur);
        self.move_cursor_to(start, extend);
    }

    /// Move to the end of the current line (before the next newline).
    pub fn move_end(&mut self, extend: bool) {
        let end = line_end(&self.buf, self.cur);
        self.move_cursor_to(end, extend);
    }
}

// ---------------------------------------------------------------------------
// byte / word helpers (pure)
// ---------------------------------------------------------------------------

/// Byte offset of the char boundary just before `i` (or 0).
pub fn prev_char_boundary(s: &str, i: usize) -> usize {
    let bytes = s.as_bytes();
    debug_assert!(i <= bytes.len() && s.is_char_boundary(i));
    if i == 0 {
        return 0;
    }
    let mut j = i - 1;
    while j > 0 && (bytes[j] & 0xC0) == 0x80 {
        j -= 1;
    }
    j
}

/// Byte offset of the char boundary just after `i` (or `len`).
pub fn next_char_boundary(s: &str, i: usize) -> usize {
    debug_assert!(i <= s.len() && s.is_char_boundary(i));
    if i >= s.len() {
        return s.len();
    }
    i + s[i..].chars().next().unwrap().len_utf8()
}

/// Byte offset of the start of the word ending at `i` (skipping whitespace
/// between `i` and the word).
pub fn prev_word_start(s: &str, i: usize) -> usize {
    let mut j = i;
    // Skip trailing whitespace backwards.
    while j > 0 {
        let p = prev_char_boundary(s, j);
        if !char_at(s, p).is_whitespace() {
            break;
        }
        j = p;
    }
    // Skip the word backwards.
    while j > 0 {
        let p = prev_char_boundary(s, j);
        if char_at(s, p).is_whitespace() {
            break;
        }
        j = p;
    }
    j
}

/// Byte offset just past the word starting at `i` (skipping whitespace
/// between `i` and the word).
pub fn next_word_end(s: &str, i: usize) -> usize {
    let mut j = i;
    // Skip leading whitespace forwards.
    while j < s.len() {
        let n = next_char_boundary(s, j);
        if !char_at(s, j).is_whitespace() {
            break;
        }
        j = n;
    }
    // Skip the word forwards.
    while j < s.len() {
        let n = next_char_boundary(s, j);
        if char_at(s, j).is_whitespace() {
            break;
        }
        j = n;
    }
    j
}

/// Byte offset of the start of the logical line containing `i`.
pub fn line_start(s: &str, i: usize) -> usize {
    match s[..i].rfind('\n') {
        Some(nl) => nl + 1,
        None => 0,
    }
}

/// Byte offset just past the logical line containing `i` (before `\n`).
pub fn line_end(s: &str, i: usize) -> usize {
    match s[i..].find('\n') {
        Some(nl) => i + nl,
        None => s.len(),
    }
}

fn char_at(s: &str, i: usize) -> char {
    s[i..].chars().next().unwrap_or('\0')
}

// ---------------------------------------------------------------------------
// display layout (wrap + cursor mapping)
// ---------------------------------------------------------------------------

/// One visual row of the wrapped buffer: byte range `start..end` (newlines
/// excluded) plus its display width in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRow {
    pub start: usize,
    pub end: usize,
    pub width: usize,
}

/// Hard-wrap `text` into visual rows of at most `width` display cells.
/// Newlines force a row break; an empty buffer yields a single empty row.
/// When a line breaks, whitespace sitting right at the break point is
/// absorbed instead of starting the next row.
pub fn wrap_rows(text: &str, width: usize) -> Vec<LayoutRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut w = 0usize;
    let mut pos = 0usize;
    while pos < text.len() {
        let ch = text[pos..].chars().next().unwrap();
        let next = pos + ch.len_utf8();
        if ch == '\n' {
            rows.push(LayoutRow {
                start: row_start,
                end: pos,
                width: w,
            });
            row_start = next;
            w = 0;
            pos = next;
            continue;
        }
        let cw = char_width(ch);
        if cw > 0 && w > 0 && w + cw > width {
            // Break before `ch`; swallow any whitespace that would otherwise
            // begin the new row (the break happens *at* that whitespace).
            rows.push(LayoutRow {
                start: row_start,
                end: pos,
                width: w,
            });
            let mut np = pos;
            while np < text.len() {
                let c = text[np..].chars().next().unwrap();
                if c == '\n' || !c.is_whitespace() {
                    break;
                }
                np += c.len_utf8();
            }
            row_start = np;
            w = 0;
            pos = np;
            continue;
        }
        w += cw;
        pos = next;
    }
    rows.push(LayoutRow {
        start: row_start,
        end: text.len(),
        width: w,
    });
    rows
}

/// Display width (in terminal cells) of a single character.
pub fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Display width of `text` in terminal cells.
pub fn slice_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Index into `rows` of the visual row holding the cursor byte offset.
pub fn cursor_row(rows: &[LayoutRow], cur: usize) -> usize {
    rows.iter()
        .position(|r| r.start <= cur && cur <= r.end)
        .unwrap_or(rows.len().saturating_sub(1))
}

/// Column (in cells, excluding any gutter) of the cursor inside its row.
pub fn cursor_col(text: &str, row: LayoutRow, cur: usize) -> usize {
    let cur = cur.clamp(row.start, row.end);
    slice_width(&text[row.start..cur])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(e: &Editor) -> String {
        e.text().to_string()
    }

    #[test]
    fn insert_and_move_by_char() {
        let mut e = Editor::new();
        e.insert('a');
        e.insert('b');
        assert_eq!(text(&e), "ab");
        assert_eq!(e.cursor(), 2);
        e.move_left(false);
        assert_eq!(e.cursor(), 1);
        e.insert('X');
        assert_eq!(text(&e), "aXb");
        assert_eq!(e.cursor(), 2);
        e.move_left(false);
        e.move_left(false);
        assert_eq!(e.cursor(), 0);
        e.move_left(false);
        assert_eq!(e.cursor(), 0); // clamped
    }

    #[test]
    fn move_right_from_end_clamps() {
        let mut e = Editor::new();
        e.insert('a');
        e.move_right(false);
        assert_eq!(e.cursor(), 1);
    }

    #[test]
    fn word_motion() {
        let mut e = Editor::new();
        for ch in "hello brave world".chars() {
            e.insert(ch);
        }
        assert_eq!(e.cursor(), 17);
        e.move_word_left(false);
        assert_eq!(e.cursor(), 12); // before "world"
        e.move_word_left(false);
        assert_eq!(e.cursor(), 6); // before "brave"
        e.move_word_left(false);
        assert_eq!(e.cursor(), 0);
        e.move_word_right(false);
        assert_eq!(e.cursor(), 5); // after "hello"
        e.move_word_right(false);
        assert_eq!(e.cursor(), 11); // after "brave"
    }

    #[test]
    fn word_motion_on_whitespace_runs_and_edges() {
        let mut e = Editor::new();
        for ch in "  one   ".chars() {
            e.insert(ch);
        }
        e.move_word_left(false);
        assert_eq!(e.cursor(), 2); // start of "one" (after leading spaces)
        e.move_word_left(false);
        assert_eq!(e.cursor(), 0);
        let mut e = Editor::new();
        for ch in "  one   two  ".chars() {
            e.insert(ch);
        }
        e.move_home(false);
        e.move_word_right(false);
        assert_eq!(e.cursor(), 5); // end of "one" (spaces skipped)
        e.move_word_right(false);
        assert_eq!(e.cursor(), 11); // end of "two"
    }

    #[test]
    fn backspace_and_word_backspace() {
        let mut e = Editor::new();
        for ch in "delete me".chars() {
            e.insert(ch);
        }
        e.backspace();
        assert_eq!(text(&e), "delete m");
        e.backspace_word();
        assert_eq!(text(&e), "delete ");
        assert_eq!(e.cursor(), 7);
        e.backspace_word();
        assert_eq!(text(&e), "");
        e.backspace(); // empty: no-op
        assert_eq!(text(&e), "");
    }

    #[test]
    fn delete_and_word_delete() {
        let mut e = Editor::new();
        for ch in "one two".chars() {
            e.insert(ch);
        }
        e.move_home(false);
        e.delete_word();
        assert_eq!(text(&e), " two");
        e.move_home(false);
        e.delete();
        assert_eq!(text(&e), "two");
    }

    #[test]
    fn shift_selection_extend_and_collapse() {
        let mut e = Editor::new();
        for ch in "abcdef".chars() {
            e.insert(ch);
        }
        e.move_left(true); // f
        e.move_left(true); // ef
        e.move_left(true); // def
        assert_eq!(e.selection(), Some((3, 6)));
        assert_eq!(e.selected_text(), Some("def"));
        // moving without shift collapses the selection and moves the cursor
        e.move_left(false);
        assert_eq!(e.selection(), None);
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn selection_edit_replaces_range() {
        let mut e = Editor::new();
        for ch in "hello world".chars() {
            e.insert(ch);
        }
        e.move_word_left(true); // select "world"
        assert_eq!(e.selected_text(), Some("world"));
        e.insert('X');
        assert_eq!(text(&e), "hello X");
        assert_eq!(e.cursor(), 7);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut e = Editor::new();
        for ch in "hello world".chars() {
            e.insert(ch);
        }
        e.move_word_left(true);
        e.backspace();
        assert_eq!(text(&e), "hello ");
        assert_eq!(e.cursor(), 6);
    }

    #[test]
    fn insert_str_replaces_selection_like_paste() {
        let mut e = Editor::new();
        for ch in "hello world".chars() {
            e.insert(ch);
        }
        e.move_word_left(true); // select "world"
        e.insert_str("there");
        assert_eq!(text(&e), "hello there");
        assert_eq!(e.cursor(), "hello there".len());
        assert_eq!(e.selection(), None);
        // multiline paste lands at the cursor
        e.move_home(false);
        e.insert_str("say ");
        assert_eq!(text(&e), "say hello there");
        assert_eq!(e.cursor(), "say ".len());
        // empty paste is a no-op
        let before = text(&e);
        e.insert_str("");
        assert_eq!(text(&e), before);
    }

    #[test]
    fn cut_selection_removes_and_returns() {
        let mut e = Editor::new();
        for ch in "hello world".chars() {
            e.insert(ch);
        }
        // no selection: nothing to cut
        assert_eq!(e.cut_selection(), None);
        assert_eq!(text(&e), "hello world");
        e.move_word_left(true); // select "world"
        assert_eq!(e.cut_selection(), Some("world".to_string()));
        assert_eq!(text(&e), "hello ");
        assert_eq!(e.cursor(), 6);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn kill_line_cuts_to_end_of_line() {
        let mut e = Editor::new();
        for ch in "hello world".chars() {
            e.insert(ch);
        }
        e.move_word_left(false); // cursor before "world"
        assert_eq!(e.cursor(), 6);
        assert_eq!(e.kill_line(), Some("world".to_string()));
        assert_eq!(text(&e), "hello ");
        assert_eq!(e.cursor(), 6);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn kill_line_joins_at_end_of_line() {
        let mut e = Editor::new();
        for ch in "ab\ncd".chars() {
            e.insert(ch);
        }
        // cursor after "ab", right before the newline (end of line)
        e.move_home(false); // start of the second line ("cd")
        e.move_left(false); // onto the newline char
        assert_eq!(e.cursor(), 2);
        assert_eq!(e.kill_line(), Some("\n".to_string()));
        assert_eq!(text(&e), "abcd");
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn kill_line_at_end_of_buffer_is_noop() {
        let mut e = Editor::new();
        for ch in "hi".chars() {
            e.insert(ch);
        }
        assert_eq!(e.cursor(), 2);
        assert_eq!(e.kill_line(), None);
        assert_eq!(text(&e), "hi");
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn kill_line_cuts_selection_first() {
        let mut e = Editor::new();
        for ch in "hello world".chars() {
            e.insert(ch);
        }
        e.move_word_left(true); // select "world"
        assert_eq!(e.kill_line(), Some("world".to_string()));
        assert_eq!(text(&e), "hello ");
        assert_eq!(e.cursor(), 6);
    }

    #[test]
    fn home_end_and_multiline() {
        let mut e = Editor::new();
        for ch in "ab\ncd".chars() {
            e.insert(ch);
        }
        assert_eq!(e.cursor(), 5);
        e.move_home(false);
        assert_eq!(e.cursor(), 3);
        e.move_left(false);
        assert_eq!(e.cursor(), 2); // onto the newline char
        e.move_left(false);
        assert_eq!(e.cursor(), 1);
        e.move_home(false);
        assert_eq!(e.cursor(), 0);
        e.move_end(false);
        assert_eq!(e.cursor(), 2);
        // insert a newline mid-line with shift+enter semantics
        e.insert('\n');
        assert_eq!(text(&e), "ab\n\ncd");
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn unicode_cursor_and_backspace() {
        let mut e = Editor::new();
        for ch in "héllo".chars() {
            e.insert(ch);
        }
        assert_eq!(e.cursor(), "héllo".len());
        e.backspace();
        assert_eq!(text(&e), "héll");
        e.move_left(false);
        e.move_left(false);
        e.insert(' ');
        assert_eq!(text(&e), "hé ll");
        assert!(e.text().is_char_boundary(e.cursor()));
    }

    #[test]
    fn take_text_clears() {
        let mut e = Editor::new();
        e.insert('x');
        assert_eq!(e.take_text(), "x");
        assert_eq!(e.text(), "");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn line_helpers_are_char_safe() {
        let s = "ab\ncé";
        assert_eq!(line_start(s, s.len()), 3);
        assert_eq!(line_end(s, s.len()), 6);
        assert_eq!(prev_word_start("a b c", 5), 4);
        assert_eq!(next_word_end("a b c", 2), 3);
    }

    #[test]
    fn wrap_rows_basic() {
        assert_eq!(
            wrap_rows("", 10),
            vec![LayoutRow {
                start: 0,
                end: 0,
                width: 0
            }]
        );
        assert_eq!(
            wrap_rows("hello world", 5),
            vec![
                LayoutRow {
                    start: 0,
                    end: 5,
                    width: 5
                },
                LayoutRow {
                    start: 6,
                    end: 11,
                    width: 5
                },
            ]
        );
    }

    #[test]
    fn wrap_rows_newlines() {
        assert_eq!(
            wrap_rows("ab\ncd", 10),
            vec![
                LayoutRow {
                    start: 0,
                    end: 2,
                    width: 2
                },
                LayoutRow {
                    start: 3,
                    end: 5,
                    width: 2
                },
            ]
        );
        // trailing newline leaves a trailing empty row
        assert_eq!(
            wrap_rows("ab\n", 10),
            vec![
                LayoutRow {
                    start: 0,
                    end: 2,
                    width: 2
                },
                LayoutRow {
                    start: 3,
                    end: 3,
                    width: 0
                },
            ]
        );
    }

    #[test]
    fn wrap_rows_unicode_width() {
        let s = "hélloworld";
        // é is one cell wide, so 5 cells fit "héllo" (6 bytes)
        assert_eq!(
            wrap_rows(s, 5),
            vec![
                LayoutRow {
                    start: 0,
                    end: 6,
                    width: 5
                },
                LayoutRow {
                    start: 6,
                    end: 11,
                    width: 5
                },
            ]
        );
    }

    #[test]
    fn cursor_maps_to_row_and_col() {
        let text = "hello\nworld";
        let rows = wrap_rows(text, 80);
        assert_eq!(cursor_row(&rows, 0), 0);
        assert_eq!(cursor_row(&rows, 5), 0); // end of first line
        assert_eq!(cursor_row(&rows, 6), 1); // start of second line
        assert_eq!(cursor_col(text, rows[1], 11), 5);
        assert_eq!(cursor_col(text, rows[0], 3), 3);

        let wide = "héllo world";
        let rows = wrap_rows(wide, 80);
        assert_eq!(cursor_row(&rows, 6), 0); // é is two bytes but one cell
        assert_eq!(cursor_col(wide, rows[0], 6), 5);
    }
}
