//! One single-line text box's text and caret, and the keys that edit them, so every small box in
//! the app (a file name, a search, a commit message, an address) types, deletes and moves the
//! same way. What Enter, Esc, Tab and the up/down arrows do stays with each box: this only edits.

use std::ops::Deref;

use iced::widget::span;
use iced::widget::text::Span;
use iced::{Color, Font};
use nib_editor::keymap::{Key, KeyChord};

use super::ai::caret::{next_char, prev_char, word_left, word_right};

/// The caret glyph drawn inside the text, so it follows the characters when the line scrolls.
pub(super) const CARET: &str = "▏";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct LineInput {
    text: String,
    /// A byte offset into `text`, always on a char boundary.
    caret: usize,
}

/// What a key did to the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Edit {
    /// Not an editing key: the box's owner decides what it means.
    Ignored,
    /// Taken (the caret may have moved), the text is the same.
    Moved,
    /// The text changed.
    Changed,
}

impl Edit {
    pub(super) fn changed(self) -> bool {
        self == Edit::Changed
    }

    pub(super) fn handled(self) -> bool {
        self != Edit::Ignored
    }
}

impl Deref for LineInput {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl From<String> for LineInput {
    fn from(text: String) -> Self {
        Self { caret: text.len(), text }
    }
}

impl From<&str> for LineInput {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

impl std::fmt::Display for LineInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl LineInput {
    pub(super) fn as_str(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(super) fn caret(&self) -> usize {
        self.caret
    }

    /// Replace the text; the caret goes to its end.
    pub(super) fn set(&mut self, text: impl Into<String>) {
        *self = Self::from(text.into());
    }

    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// `s` typed or pasted at the caret, the caret after it. One line: a trailing line break
    /// goes, the ones inside become spaces, and other control characters are dropped.
    pub(super) fn insert(&mut self, s: &str) -> Edit {
        let s = s.trim_end_matches(['\r', '\n']).replace("\r\n", " ");
        let s: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
        if s.is_empty() {
            return Edit::Moved;
        }
        self.text.insert_str(self.caret, &s);
        self.caret += s.len();
        Edit::Changed
    }

    /// Take out what's between the caret and `to`, either side of it.
    fn delete_to(&mut self, to: usize) -> Edit {
        let (a, b) = (self.caret.min(to), self.caret.max(to));
        if a == b {
            return Edit::Moved;
        }
        self.text.replace_range(a..b, "");
        self.caret = a;
        Edit::Changed
    }

    /// One key on the box. Words go with Alt (macOS) or Ctrl (elsewhere, and on macOS too, as
    /// people reach for it there); Cmd goes to the start or end, like a text field on macOS.
    /// Typed text goes in unless Ctrl or Cmd is held (those are shortcuts).
    pub(super) fn key(&mut self, chord: &KeyChord, text: Option<&str>) -> Edit {
        let (s, i) = (self.text.as_str(), self.caret);
        let word = chord.alt || chord.ctrl;
        let to = match chord.key {
            Key::Left if chord.logo => Some(0),
            Key::Left if word => Some(word_left(s, i)),
            Key::Left => Some(prev_char(s, i)),
            Key::Right if chord.logo => Some(s.len()),
            Key::Right if word => Some(word_right(s, i)),
            Key::Right => Some(next_char(s, i)),
            Key::Home => Some(0),
            Key::End => Some(s.len()),
            _ => None,
        };
        if let Some(to) = to {
            self.caret = to;
            return Edit::Moved;
        }
        let del = match chord.key {
            Key::Backspace if chord.logo => Some(0),
            Key::Backspace if word => Some(word_left(s, i)),
            Key::Backspace => Some(prev_char(s, i)),
            Key::Delete if chord.logo => Some(s.len()),
            Key::Delete if word => Some(word_right(s, i)),
            Key::Delete => Some(next_char(s, i)),
            _ => None,
        };
        if let Some(to) = del {
            return self.delete_to(to);
        }
        if chord.ctrl || chord.logo {
            return Edit::Ignored;
        }
        let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
        if typed.is_empty() {
            return Edit::Ignored;
        }
        self.insert(&typed)
    }

    /// The same box with every character shown as `c` (a password), the caret in its place.
    pub(super) fn masked(&self, c: char) -> LineInput {
        let n = self.text[..self.caret].chars().count();
        let text: String = std::iter::repeat_n(c, self.text.chars().count()).collect();
        let caret = n * c.len_utf8();
        LineInput { text, caret }
    }

    /// The text with the caret glyph where the caret is: before it, the caret, after it. Owned,
    /// so a view can keep them past the borrow of the box (the text is one short line).
    pub(super) fn spans(&self, fg: Color, caret: Color) -> [Span<'static, (), Font>; 3] {
        let (a, b) = self.text.split_at(self.caret);
        [span(a.to_string()).color(fg), span(CARET).color(caret), span(b.to_string()).color(fg)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(l: &mut LineInput, s: &str) -> Edit {
        l.key(&KeyChord::parse(s).unwrap(), None)
    }

    #[test]
    fn edits_at_the_caret() {
        let mut l = LineInput::from("héllo wörld");
        assert_eq!(l.caret(), l.len(), "a new box starts at the end");
        assert_eq!(press(&mut l, "alt+backspace"), Edit::Changed);
        assert_eq!(l.as_str(), "héllo ", "a word, not the whole box");
        press(&mut l, "home");
        press(&mut l, "right");
        assert_eq!(l.caret(), 1);
        press(&mut l, "right");
        assert_eq!(l.caret(), 3, "é is two bytes");
        assert_eq!(l.key(&KeyChord::parse("x").unwrap(), Some("X")), Edit::Changed);
        assert_eq!(l.as_str(), "héXllo ");
        press(&mut l, "backspace");
        press(&mut l, "delete");
        assert_eq!((l.as_str(), l.caret()), ("hélo ", 3));
        assert_eq!(press(&mut l, "ctrl+delete"), Edit::Changed);
        assert_eq!(l.as_str(), "hé ");
        assert_eq!(l.key(&KeyChord::parse("ctrl+v").unwrap(), Some("v")), Edit::Ignored, "a shortcut, not typing");
        assert_eq!(press(&mut l, "up"), Edit::Ignored);
        press(&mut l, "end");
        assert_eq!(press(&mut l, "delete"), Edit::Moved, "nothing after the caret");
        l.insert("one\ntwo\n");
        assert_eq!(l.as_str(), "hé one two", "pasted lines become one");
        let [a, _, b] = l.spans(Color::WHITE, Color::WHITE);
        assert_eq!((a.text.as_ref(), b.text.as_ref()), ("hé one two", ""));
    }
}
