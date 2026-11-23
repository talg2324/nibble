//! One single-line text box's text and caret, and the keys that edit them, so every small box in
//! the app (a file name, a search, a commit message, an address) types, deletes and moves the
//! same way. What Enter, Esc, Tab and the up/down arrows do stays with each box: this only edits.

use std::ops::Deref;
use std::time::{Duration, Instant};

use iced::widget::span;
use iced::widget::text::Span;
use iced::{Color, Font};
use nib_editor::keymap::{Key, KeyChord};

use super::ai::caret::{next_char, prev_char, word_left, word_right};

/// The caret glyph drawn inside the text, so it follows the characters when the line scrolls.
pub(super) const CARET: &str = "▏";

/// Typing within this long of the last key is one undo step (as in the editor).
const TYPING_GROUP: Duration = Duration::from_millis(1000);
/// The most undo steps a box keeps.
const MOST_STEPS: usize = 200;

/// Undo and redo for a text box: what the text and its caret (a byte offset) were before each
/// change. Typed characters run together into one step, a word at a time, as in the editor.
#[derive(Debug, Clone, Default)]
pub(super) struct History {
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    /// When the last character was typed, while its step is still open to more typing.
    typing: Option<Instant>,
}

impl History {
    /// Before a change to `text` (caret at `caret`). `typed`: the change is that one character
    /// typed, which joins the typing just before it unless it starts a new word.
    pub(super) fn record(&mut self, text: &str, caret: usize, typed: Option<char>) {
        let now = Instant::now();
        let open = self.typing.is_some_and(|t| now.duration_since(t) < TYPING_GROUP);
        let joins = open && typed.is_some_and(|c| !c.is_whitespace());
        self.typing = typed.map(|_| now);
        self.redo.clear();
        if !joins {
            if self.undo.len() == MOST_STEPS {
                self.undo.remove(0);
            }
            self.undo.push((text.to_string(), caret));
        }
    }

    /// The caret moved: the next typing is a step of its own.
    pub(super) fn close(&mut self) {
        self.typing = None;
    }

    /// What to go back to from `text` / `caret`; `None`: nothing to undo.
    pub(super) fn undo(&mut self, text: &str, caret: usize) -> Option<(String, usize)> {
        self.typing = None;
        let back = self.undo.pop()?;
        self.redo.push((text.to_string(), caret));
        Some(back)
    }

    /// What to go forward to again; `None`: nothing undone to redo.
    pub(super) fn redo(&mut self, text: &str, caret: usize) -> Option<(String, usize)> {
        self.typing = None;
        let fwd = self.redo.pop()?;
        self.undo.push((text.to_string(), caret));
        Some(fwd)
    }
}

/// Undo (`Some(false)`) or redo (`Some(true)`) by its usual keys: Cmd+Z / Cmd+Shift+Z on macOS,
/// Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y elsewhere. For boxes that see keys without the keymap.
fn undo_chord(chord: &KeyChord) -> Option<bool> {
    let held = if cfg!(target_os = "macos") { chord.logo && !chord.ctrl } else { chord.ctrl && !chord.logo };
    if !held || chord.alt {
        return None;
    }
    match chord.key {
        Key::Char('z') => Some(chord.shift),
        Key::Char('y') if !chord.shift && !cfg!(target_os = "macos") => Some(true),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct LineInput {
    text: String,
    /// A byte offset into `text`, always on a char boundary.
    caret: usize,
    history: History,
}

/// Two boxes are the same when they show the same text and caret (history aside).
impl PartialEq for LineInput {
    fn eq(&self, other: &Self) -> bool {
        (&self.text, self.caret) == (&other.text, other.caret)
    }
}

impl Eq for LineInput {}

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
        Self { caret: text.len(), text, history: History::default() }
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

    /// Replace the text; the caret goes to its end, and the history starts over.
    pub(super) fn set(&mut self, text: impl Into<String>) {
        *self = Self::from(text.into());
    }

    /// Empty the box (one undo step: Esc on a filled box, or typing over a selected name).
    pub(super) fn clear(&mut self) {
        if !self.text.is_empty() {
            self.history.record(&self.text, self.caret, None);
        }
        self.text.clear();
        self.caret = 0;
    }

    /// `s` pasted at the caret, the caret after it. One line: a trailing line break goes, the
    /// ones inside become spaces, and other control characters are dropped.
    pub(super) fn insert(&mut self, s: &str) -> Edit {
        self.put(s, false)
    }

    /// `s` in at the caret; `typed`: it was typed (one key), so it can join the typing before it.
    fn put(&mut self, s: &str, typed: bool) -> Edit {
        let s = s.trim_end_matches(['\r', '\n']).replace("\r\n", " ");
        let s: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
        if s.is_empty() {
            return Edit::Moved;
        }
        let mut one = s.chars();
        let typed = match (one.next(), one.next()) {
            (Some(c), None) if typed => Some(c),
            _ => None,
        };
        self.history.record(&self.text, self.caret, typed);
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
        self.history.record(&self.text, self.caret, None);
        self.text.replace_range(a..b, "");
        self.caret = a;
        Edit::Changed
    }

    /// One key on the box. Words go with Alt (macOS) or Ctrl (elsewhere, and on macOS too, as
    /// people reach for it there); Cmd goes to the start or end, like a text field on macOS.
    /// Typed text goes in unless Ctrl or Cmd is held (those are shortcuts).
    pub(super) fn key(&mut self, chord: &KeyChord, text: Option<&str>) -> Edit {
        if let Some(redo) = undo_chord(chord) {
            let (s, i) = (self.text.as_str(), self.caret);
            let to = if redo { self.history.redo(s, i) } else { self.history.undo(s, i) };
            let Some((text, caret)) = to else { return Edit::Moved };
            (self.text, self.caret) = (text, caret);
            return Edit::Changed;
        }
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
            self.history.close();
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
        self.put(&typed, true)
    }

    /// The same box with every character shown as `c` (a password), the caret in its place.
    pub(super) fn masked(&self, c: char) -> LineInput {
        let n = self.text[..self.caret].chars().count();
        let text: String = std::iter::repeat_n(c, self.text.chars().count()).collect();
        let caret = n * c.len_utf8();
        LineInput { text, caret, history: History::default() }
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

    fn typed(l: &mut LineInput, s: &str) {
        for c in s.chars() {
            l.key(&KeyChord::parse(if c == ' ' { "space" } else { "x" }).unwrap(), Some(&c.to_string()));
        }
    }

    #[test]
    fn undoes_a_word_at_a_time() {
        let (undo, redo) = if cfg!(target_os = "macos") { ("cmd+z", "cmd+shift+z") } else { ("ctrl+z", "ctrl+shift+z") };
        let mut l = LineInput::default();
        assert_eq!(press(&mut l, undo), Edit::Moved, "nothing to undo: taken, nothing changes");
        typed(&mut l, "git status");
        assert_eq!(press(&mut l, undo), Edit::Changed);
        assert_eq!((l.as_str(), l.caret()), ("git", 3), "the last word and the space before it");
        assert_eq!(press(&mut l, redo), Edit::Changed);
        assert_eq!(l.as_str(), "git status");
        press(&mut l, "alt+backspace");
        assert_eq!(l.as_str(), "git ");
        press(&mut l, undo);
        assert_eq!((l.as_str(), l.caret()), ("git status", 10), "a delete is a step, caret back where it was");
        press(&mut l, "home");
        typed(&mut l, "x");
        assert_eq!(l.as_str(), "xgit status", "a move starts a new step");
        assert_eq!(press(&mut l, redo), Edit::Moved, "a new edit drops what was undone");
        l.insert("pasted");
        l.clear();
        press(&mut l, undo);
        assert_eq!(l.as_str(), "xpastedgit status", "clearing is a step");
        press(&mut l, undo);
        press(&mut l, undo);
        press(&mut l, undo);
        assert_eq!(l.as_str(), "git", "back to the first step");
        press(&mut l, undo);
        assert_eq!(l.as_str(), "");
        l.set("new");
        assert_eq!(press(&mut l, undo), Edit::Moved, "set starts the history over");
    }
}
