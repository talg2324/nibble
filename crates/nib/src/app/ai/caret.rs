//! The caret in the message being typed: where typing, pasting and deleting happen, how the
//! arrow / word / line / start-end keys move it, and the selection Shift with those keys makes. Positions are byte offsets into the text, always
//! on a char boundary. Lines are the text's own (Shift+Enter), not where it happens to wrap.

use super::Chat;

impl Chat {
    /// Where the caret is in `input`. Kept as a distance from the end, so text put on the end
    /// (a paste, a completion) leaves a caret that was at the end still at the end.
    pub(super) fn caret(&self) -> usize {
        self.from_back(self.back)
    }

    fn from_back(&self, back: usize) -> usize {
        let mut i = self.input.len() - back.min(self.input.len());
        while !self.input.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    /// What's selected, start to end; `None` when nothing is.
    pub(super) fn selection(&self) -> Option<(usize, usize)> {
        let (a, i) = (self.from_back(self.anchor?), self.caret());
        (a != i).then(|| (a.min(i), a.max(i)))
    }

    pub(super) fn selected(&self) -> Option<&str> {
        self.selection().map(|(a, b)| &self.input[a..b])
    }

    /// The caret to `i`; `extend`: the selection grows (or starts) with it, else it goes.
    pub(super) fn move_to(&mut self, i: usize, extend: bool) {
        self.history.close();
        if !extend {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(self.back);
        }
        self.set_caret(i);
    }

    pub(super) fn select_all(&mut self) {
        self.anchor = Some(self.input.len());
        self.back = 0;
    }

    /// Take out what's selected, if anything is (one undo step).
    pub(super) fn delete_selection(&mut self) -> bool {
        if self.selection().is_none() {
            return false;
        }
        self.history.record(&self.input, self.caret(), None);
        self.take_selection()
    }

    fn take_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else { return false };
        self.input.replace_range(a..b, "");
        self.anchor = None;
        self.set_caret(a);
        true
    }

    pub(super) fn set_caret(&mut self, i: usize) {
        self.back = self.input.len() - i.min(self.input.len());
    }

    /// `s` pasted (or put in) at the caret, in place of what's selected; the caret ends up
    /// after it. One undo step.
    pub(super) fn insert(&mut self, s: &str) {
        self.put(s, false);
    }

    /// `s` typed: one character joins the typing before it in one undo step, a word at a time.
    pub(super) fn type_text(&mut self, s: &str) {
        self.put(s, true);
    }

    fn put(&mut self, s: &str, typed: bool) {
        if s.is_empty() {
            return;
        }
        let mut one = s.chars();
        let typed = match (one.next(), one.next()) {
            (Some(c), None) if typed && self.selection().is_none() => Some(c),
            _ => None,
        };
        self.history.record(&self.input, self.caret(), typed);
        self.take_selection();
        self.anchor = None;
        let i = self.caret();
        self.input.insert_str(i, s);
        self.set_caret(i + s.len());
    }

    /// Take out what's between the caret and `to` (either side of it), or what's selected.
    pub(super) fn delete_to(&mut self, to: usize) {
        if self.delete_selection() {
            return;
        }
        self.anchor = None;
        let i = self.caret();
        let (a, b) = (i.min(to), i.max(to));
        if a == b {
            return;
        }
        self.history.record(&self.input, i, None);
        self.input.replace_range(a..b, "");
        self.set_caret(a);
    }

    /// The message gone (sent, or taken as an answer): an empty box with a history of its own.
    pub(super) fn clear_input(&mut self) {
        self.input.clear();
        self.back = 0;
        self.anchor = None;
        self.history = Default::default();
    }

    /// Back one step (Cmd+Z); nothing to undo leaves the box as it is.
    pub(super) fn undo(&mut self) {
        if let Some((text, caret)) = self.history.undo(&self.input, self.caret()) {
            self.restore(text, caret);
        }
    }

    /// Forward again one step undone (Cmd+Shift+Z).
    pub(super) fn redo(&mut self) {
        if let Some((text, caret)) = self.history.redo(&self.input, self.caret()) {
            self.restore(text, caret);
        }
    }

    fn restore(&mut self, text: String, caret: usize) {
        self.input = text;
        self.anchor = None;
        self.set_caret(caret);
    }
}

pub(in crate::app) fn prev_char(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map_or(0, |(j, _)| j)
}

pub(in crate::app) fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i, |c| i + c.len_utf8())
}

/// The start of the word before `i` (spaces before it skipped), like Alt+Left in a text field.
pub(in crate::app) fn word_left(s: &str, i: usize) -> usize {
    let t = s[..i].trim_end();
    t.rfind(char::is_whitespace).map_or(0, |j| j + t[j..].chars().next().map_or(1, char::len_utf8))
}

/// The end of the word after `i`.
pub(in crate::app) fn word_right(s: &str, i: usize) -> usize {
    let rest = &s[i..];
    let skip = rest.len() - rest.trim_start().len();
    let t = &rest[skip..];
    i + skip + t.find(char::is_whitespace).unwrap_or(t.len())
}

pub(super) fn line_start(s: &str, i: usize) -> usize {
    s[..i].rfind('\n').map_or(0, |j| j + 1)
}

pub(super) fn line_end(s: &str, i: usize) -> usize {
    s[i..].find('\n').map_or(s.len(), |j| i + j)
}

/// `n` chars into the line starting at `start`, or its end if it's shorter.
fn column_at(s: &str, start: usize, n: usize) -> usize {
    let end = line_end(s, start);
    s[start..end].char_indices().nth(n).map_or(end, |(j, _)| start + j)
}

/// Same column on the line above; on the first line, the start of the text.
pub(super) fn up(s: &str, i: usize) -> usize {
    let start = line_start(s, i);
    if start == 0 {
        return 0;
    }
    let col = s[start..i].chars().count();
    column_at(s, line_start(s, start - 1), col)
}

/// Same column on the line below; on the last line, the end of the text.
pub(super) fn down(s: &str, i: usize) -> usize {
    let end = line_end(s, i);
    if end == s.len() {
        return s.len();
    }
    let col = s[line_start(s, i)..i].chars().count();
    column_at(s, end + 1, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves() {
        let s = "héllo wörld\nab\nlonger line";
        let i = s.find("wörld").unwrap();
        assert_eq!(word_left(s, i), 0);
        assert_eq!(word_left(s, i + 3), i);
        assert_eq!(word_right(s, 0), "héllo".len());
        assert_eq!(word_right(s, "héllo".len()), s.find('\n').unwrap());
        assert_eq!(prev_char(s, 3), 1, "é is two bytes");
        assert_eq!(next_char(s, 1), 3);
        // Down from column 8 lands on the end of the short line, then column 2 of the next.
        let ab = s.find("ab").unwrap();
        assert_eq!(down(s, 8), ab + 2);
        assert_eq!(down(s, ab + 2), s.find("longer").unwrap() + 2);
        assert_eq!(down(s, s.len() - 2), s.len());
        assert_eq!(up(s, ab + 1), 1);
        assert_eq!(up(s, 3), 0, "first line: to the start");
        assert_eq!(line_start(s, ab + 1), ab);
        assert_eq!(line_end(s, ab), ab + 2);
    }

    #[test]
    fn edits_at_the_caret() {
        let mut c = Chat { input: "held".into(), ..Default::default() };
        c.set_caret(2);
        c.insert("ll");
        assert_eq!((c.input.as_str(), c.caret()), ("hellld", 4));
        c.delete_to(prev_char(&c.input, 4));
        assert_eq!((c.input.as_str(), c.caret()), ("helld", 3));
        c.set_caret(c.input.len());
        c.input.push_str(" more");
        assert_eq!(c.caret(), c.input.len(), "a caret at the end stays there when text goes on the end");
        c.delete_to(word_left(&c.input, c.caret()));
        assert_eq!(c.input, "helld ");
        c.input.clear();
        assert_eq!(c.caret(), 0);
    }

    #[test]
    fn selects() {
        let mut c = Chat { input: "one two three".into(), ..Default::default() };
        c.move_to(4, false);
        c.move_to(word_right(&c.input, c.caret()), true);
        assert_eq!(c.selected(), Some("two"));
        c.move_to(word_right(&c.input, c.caret()), true);
        assert_eq!(c.selected(), Some("two three"), "the selection grows from where it started");
        c.move_to(2, true);
        assert_eq!(c.selected(), Some("e "), "and goes back past its start");
        c.insert("X");
        assert_eq!((c.input.as_str(), c.caret(), c.selection()), ("onXtwo three", 3, None));
        c.select_all();
        assert_eq!(c.selected(), Some("onXtwo three"));
        c.delete_to(prev_char(&c.input, c.caret()));
        assert_eq!((c.input.as_str(), c.selection()), ("", None));
        c.insert("abc");
        assert_eq!(c.selection(), None, "a selection doesn't come back after the text is replaced");
    }

    #[test]
    fn undoes_and_redoes() {
        let mut c = Chat::default();
        c.undo();
        assert_eq!(c.input, "", "nothing to undo");
        for ch in "fix the bug".chars() {
            c.type_text(&ch.to_string());
        }
        c.undo();
        assert_eq!((c.input.as_str(), c.caret()), ("fix the", 7), "a word at a time");
        c.undo();
        c.undo();
        assert_eq!(c.input, "");
        c.redo();
        c.redo();
        assert_eq!(c.input, "fix the");
        c.type_text("!");
        c.redo();
        assert_eq!(c.input, "fix the!", "a new edit drops what was undone");
        c.move_to(4, false);
        c.move_to(7, true);
        c.delete_selection();
        assert_eq!(c.input, "fix !");
        c.insert("pasted\ntext");
        c.undo();
        assert_eq!(c.input, "fix !");
        c.undo();
        assert_eq!((c.input.as_str(), c.caret(), c.selection()), ("fix the!", 7, None), "the cut comes back");
        c.delete_to(word_left(&c.input, c.caret()));
        c.undo();
        assert_eq!(c.input, "fix the!");
        c.clear_input();
        c.undo();
        assert_eq!(c.input, "", "a sent message doesn't come back");
    }
}
