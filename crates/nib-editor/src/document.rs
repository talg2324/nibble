//! A text buffer: rope + selection + undo history (all helix-core) + optional syntax tree.
//!
//! Selection model: helix-core `Range { anchor, head }` in char indices. A zero-width range is a
//! plain cursor (non-modal editing: the cursor sits *between* characters).

use helix_core::graphemes::{next_grapheme_boundary, prev_grapheme_boundary};
use helix_core::history::{History, State};
use helix_core::{Assoc, Range, Rope, RopeSlice, Selection, Tendril, Transaction};
use nib_syntax::Syntax;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Consecutive typing within this window is merged into one undo step (VSCode-like).
const UNDO_GROUP_IDLE: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Typing a character: may merge with the previous typing step.
    Typing,
    /// Everything else: its own undo step.
    Discrete,
}

pub struct Document {
    pub text: Rope,
    pub selection: Selection,
    /// Path as known to the daemon (absolute on the daemon's machine).
    pub path: Option<PathBuf>,
    /// mtime reported by the daemon when last read/written; used for conflict detection on save.
    pub disk_mtime_ns: Option<u128>,
    pub syntax: Option<Syntax>,
    pub read_only: bool,
    pub tab_width: usize,
    history: History,
    pending: Option<(Transaction, State, Instant)>,
    saved_revision: usize,
    /// Preferred visual column for vertical movement.
    goal_col: Option<usize>,
    /// Bumped on every text change (edits, undo, redo); lets derived views know when to refresh.
    version: u64,
}

impl Document {
    pub fn new(text: Rope, path: Option<PathBuf>) -> Self {
        let mut d = Self::new_plain(text, path);
        if nib_syntax::registry_if_ready().is_some() {
            d.attach_syntax();
        }
        d
    }

    /// Without syntax; call `attach_syntax` later (keeps first paint off the query-compile path).
    pub fn new_plain(text: Rope, path: Option<PathBuf>) -> Self {
        Self {
            text,
            selection: Selection::point(0),
            path,
            disk_mtime_ns: None,
            syntax: None,
            read_only: false,
            tab_width: 4,
            history: History::default(),
            pending: None,
            saved_revision: 0,
            goal_col: None,
            version: 0,
        }
    }

    pub fn from_bytes(data: &[u8], path: Option<PathBuf>) -> Self {
        match std::str::from_utf8(data) {
            Ok(s) => Self::new(Rope::from(s), path),
            Err(_) => {
                let mut d = Self::new(Rope::from(String::from_utf8_lossy(data).as_ref()), path);
                d.syntax = None;
                d.read_only = true; // never write back a lossy decode
                d
            }
        }
    }

    /// Detect the language and parse. Blocks on grammar/query init if not yet warmed.
    pub fn attach_syntax(&mut self) {
        if self.syntax.is_some() {
            return;
        }
        let Some(path) = &self.path else { return };
        if let Some(lang) = nib_syntax::registry().detect(path, self.text.slice(..)) {
            self.syntax = Syntax::new(&self.text, lang).ok();
        }
    }

    pub fn language_name(&self) -> &'static str {
        self.syntax
            .as_ref()
            .map(|s| nib_syntax::registry().name(s.lang))
            .unwrap_or("text")
    }

    /// Changes whenever the text does.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_modified(&self) -> bool {
        self.history.current_revision() != self.saved_revision || self.pending.is_some()
    }

    /// Close the typing group and return the revision being saved. Pass it back to
    /// `mark_saved` when the daemon confirms (edits made while the write is in flight stay dirty).
    pub fn save_point(&mut self) -> usize {
        self.commit_pending();
        self.history.current_revision()
    }

    pub fn mark_saved(&mut self, revision: usize, mtime_ns: u128) {
        self.saved_revision = revision;
        self.disk_mtime_ns = Some(mtime_ns);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.text.len_bytes());
        for chunk in self.text.chunks() {
            out.extend_from_slice(chunk.as_bytes());
        }
        out
    }

    // -----------------------------------------------------------------------------------------
    // transactions & history
    // -----------------------------------------------------------------------------------------

    /// Apply an edit. `tx` must carry the post-edit selection.
    pub fn apply(&mut self, tx: Transaction, kind: EditKind) {
        if self.read_only || tx.changes().is_empty() {
            if let Some(sel) = tx.selection() {
                self.selection = sel.clone();
            }
            return;
        }
        let old_state = State {
            doc: self.text.clone(),
            selection: self.selection.clone(),
        };
        self.apply_raw(&tx);

        let now = Instant::now();
        match (kind, self.pending.take()) {
            (EditKind::Typing, Some((ptx, pstate, t)))
                if now.duration_since(t) < UNDO_GROUP_IDLE =>
            {
                self.pending = Some((ptx.compose(tx), pstate, now));
            }
            (EditKind::Typing, prev) => {
                if let Some((ptx, pstate, _)) = prev {
                    self.history.commit_revision(&ptx, &pstate);
                }
                self.pending = Some((tx, old_state, now));
            }
            (EditKind::Discrete, prev) => {
                if let Some((ptx, pstate, _)) = prev {
                    self.history.commit_revision(&ptx, &pstate);
                }
                self.history.commit_revision(&tx, &old_state);
            }
        }
    }

    fn apply_raw(&mut self, tx: &Transaction) {
        let old = self.text.clone(); // O(1): ropes are persistent
        tx.apply(&mut self.text);
        self.version += 1;
        if let Some(sel) = tx.selection() {
            // Not `ensure_invariants`: that widens points to 1 char (helix block cursor), which
            // with our bar cursor makes the next keystroke overwrite the following character.
            let text = self.text.slice(..);
            self.selection = sel.clone().transform(|r| r.grapheme_aligned(text));
        } else {
            self.selection = self.selection.clone().map(tx.changes());
        }
        if let Some(syn) = &mut self.syntax {
            if syn.update(&old, &self.text, tx.changes()).is_err() {
                self.syntax = Syntax::new(&self.text, syn.lang).ok();
            }
        }
    }

    /// Close the current typing group (call on cursor moves, saves, focus changes...).
    pub fn commit_pending(&mut self) {
        if let Some((tx, state, _)) = self.pending.take() {
            self.history.commit_revision(&tx, &state);
        }
    }

    pub fn undo(&mut self) -> bool {
        self.commit_pending();
        if let Some(tx) = self.history.undo().cloned() {
            self.apply_raw(&tx);
            self.goal_col = None;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        self.commit_pending();
        if let Some(tx) = self.history.redo().cloned() {
            self.apply_raw(&tx);
            self.goal_col = None;
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------------------------
    // editing
    // -----------------------------------------------------------------------------------------

    /// Replace every selection with `s`; cursors land after the inserted text.
    pub fn insert(&mut self, s: &str, kind: EditKind) {
        let s: Tendril = s.into();
        let tx = Transaction::change_by_selection(&self.text, &self.selection, |r| {
            (r.from(), r.to(), Some(s.clone()))
        });
        let sel = self.selection.clone().transform(|r| {
            let p = tx.changes().map_pos(r.to(), Assoc::After);
            Range::point(p)
        });
        self.apply(tx.with_selection(sel), kind);
        self.goal_col = None;
    }

    pub fn type_char(&mut self, c: char) {
        // Word boundaries close the typing group so undo steps are word-sized.
        if c.is_whitespace() {
            self.commit_pending();
        }
        let mut buf = [0u8; 4];
        self.insert(c.encode_utf8(&mut buf), EditKind::Typing);
    }

    pub fn newline(&mut self) {
        self.commit_pending();
        let text = self.text.slice(..);
        let is_py = self.language_name() == "python";
        let tab = self.tab_width;
        let tx = Transaction::change_by_selection(&self.text, &self.selection, |r| {
            let line = text.char_to_line(r.from());
            let line_start = text.line_to_char(line);
            let before: String = text.slice(line_start..r.from()).chars().collect();
            let mut indent: String = before
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            if is_py && before.trim_end().ends_with(':') {
                indent.push_str(&" ".repeat(tab));
            }
            (r.from(), r.to(), Some(format!("\n{indent}").into()))
        });
        let sel = self
            .selection
            .clone()
            .transform(|r| Range::point(tx.changes().map_pos(r.to(), Assoc::After)));
        self.apply(tx.with_selection(sel), EditKind::Discrete);
        self.goal_col = None;
    }

    pub fn delete_backward(&mut self) {
        self.delete_dir(false, false);
    }
    pub fn delete_forward(&mut self) {
        self.delete_dir(true, false);
    }
    pub fn delete_word_backward(&mut self) {
        self.delete_dir(false, true);
    }

    fn delete_dir(&mut self, forward: bool, word: bool) {
        let text = self.text.slice(..);
        let tab = self.tab_width;
        let tx = Transaction::delete_by_selection(&self.text, &self.selection, |r| {
            if !r.is_empty() {
                return (r.from(), r.to());
            }
            let pos = r.head;
            if forward {
                (pos, next_grapheme_boundary(text, pos))
            } else if word {
                (word_left(text, pos), pos)
            } else {
                // Backspace in leading whitespace removes back to the previous tab stop.
                let line_start = text.line_to_char(text.char_to_line(pos));
                let col = pos - line_start;
                let leading = text.slice(line_start..pos).chars().all(|c| c == ' ');
                if leading && col > 0 {
                    let n = match col % tab {
                        0 => tab,
                        m => m,
                    };
                    (pos - n.min(col), pos)
                } else {
                    (prev_grapheme_boundary(text, pos), pos)
                }
            }
        });
        let sel = self
            .selection
            .clone()
            .transform(|r| Range::point(tx.changes().map_pos(r.from(), Assoc::Before)));
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
        self.goal_col = None;
    }

    pub fn indent(&mut self) {
        let multi_line = self.selection.ranges().iter().any(|r| {
            let (a, b) = r.line_range(self.text.slice(..));
            a != b
        });
        if !multi_line {
            let pos = self.selection.primary().head;
            let col = pos - self.text.line_to_char(self.text.char_to_line(pos));
            let n = self.tab_width - col % self.tab_width;
            self.commit_pending();
            self.insert(&" ".repeat(n), EditKind::Discrete);
            return;
        }
        let pad: Tendril = " ".repeat(self.tab_width).into();
        let lines = self.selected_lines();
        let tx = Transaction::change(
            &self.text,
            lines.iter().map(|&l| {
                let p = self.text.line_to_char(l);
                (p, p, Some(pad.clone()))
            }),
        );
        let sel = self.selection.clone().map(tx.changes());
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
    }

    pub fn dedent(&mut self) {
        let lines = self.selected_lines();
        let tab = self.tab_width;
        let tx = Transaction::change(
            &self.text,
            lines.iter().filter_map(|&l| {
                let p = self.text.line_to_char(l);
                let n = self
                    .text
                    .line(l)
                    .chars()
                    .take(tab)
                    .take_while(|c| *c == ' ')
                    .count();
                (n > 0).then_some((p, p + n, None))
            }),
        );
        let sel = self.selection.clone().map(tx.changes());
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
    }

    pub fn toggle_comment(&mut self) {
        let token = match self.language_name() {
            "python" | "bash" | "yaml" | "toml" => "#",
            "rust" | "go" => "//",
            _ => return,
        };
        let lines = self.selected_lines();
        let text = self.text.slice(..);
        let non_blank: Vec<usize> = lines
            .iter()
            .copied()
            .filter(|&l| text.line(l).chars().any(|c| !c.is_whitespace()))
            .collect();
        let indent_of = |l: usize| {
            text.line(l)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .count()
        };
        let all_commented = !non_blank.is_empty()
            && non_blank.iter().all(|&l| {
                text.line(l)
                    .chars()
                    .skip(indent_of(l))
                    .collect::<String>()
                    .starts_with(token)
            });
        let min_indent = non_blank.iter().map(|&l| indent_of(l)).min().unwrap_or(0);
        let tx = Transaction::change(
            &self.text,
            non_blank.iter().map(|&l| {
                let start = text.line_to_char(l);
                if all_commented {
                    let p = start + indent_of(l);
                    let after: String = text.slice(p..).chars().take(token.len() + 1).collect();
                    let n = if after == format!("{token} ") {
                        token.len() + 1
                    } else {
                        token.len()
                    };
                    (p, p + n, None)
                } else {
                    let p = start + min_indent;
                    (p, p, Some(format!("{token} ").into()))
                }
            }),
        );
        let sel = self.selection.clone().map(tx.changes());
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
    }

    fn selected_lines(&self) -> Vec<usize> {
        let text = self.text.slice(..);
        let mut lines = Vec::new();
        for r in self.selection.ranges() {
            let (a, mut b) = r.line_range(text);
            // A selection ending at column 0 of a line doesn't include that line.
            if b > a && r.to() == text.line_to_char(b) {
                b -= 1;
            }
            lines.extend(a..=b);
        }
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// Text of the selections (joined by newlines), or whole lines if all selections are empty.
    pub fn copy_text(&self) -> (String, bool) {
        let text = self.text.slice(..);
        if self.selection.ranges().iter().all(|r| r.is_empty()) {
            let mut out = String::new();
            for l in self.selected_lines() {
                out.extend(text.line(l).chars());
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            (out, true)
        } else {
            let parts: Vec<String> = self
                .selection
                .fragments(text)
                .map(|f| f.into_owned())
                .collect();
            (parts.join("\n"), false)
        }
    }

    pub fn delete_lines(&mut self) {
        let lines = self.selected_lines();
        let text = self.text.slice(..);
        let tx = Transaction::change(
            &self.text,
            lines.iter().map(|&l| {
                let s = text.line_to_char(l);
                let e = if l + 1 < text.len_lines() {
                    text.line_to_char(l + 1)
                } else {
                    text.len_chars()
                };
                (s, e, None)
            }),
        );
        let sel = self
            .selection
            .clone()
            .transform(|r| Range::point(tx.changes().map_pos(r.head, Assoc::Before)));
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
    }

    pub fn cut(&mut self) -> String {
        let (s, lines) = self.copy_text();
        if lines {
            self.delete_lines();
        } else {
            self.commit_pending();
            self.insert("", EditKind::Discrete);
        }
        s
    }

    pub fn paste(&mut self, s: &str) {
        self.commit_pending();
        let s = s.replace("\r\n", "\n");
        self.insert(&s, EditKind::Discrete);
    }

    pub fn duplicate_line(&mut self) {
        let lines = self.selected_lines();
        let text = self.text.slice(..);
        let tx = Transaction::change(
            &self.text,
            lines.iter().map(|&l| {
                let mut s: String = text.line(l).chars().collect();
                if !s.ends_with('\n') {
                    s.insert(0, '\n');
                }
                let p = if l + 1 < text.len_lines() || text.line(l).chars().last() == Some('\n') {
                    text.line_to_char(l + 1)
                } else {
                    text.len_chars()
                };
                (p, p, Some(s.into()))
            }),
        );
        // Cursors follow the copy (VSCode "copy line down").
        let block: usize = lines
            .iter()
            .map(|&l| text.line(l).len_chars())
            .sum::<usize>()
            + usize::from(
                lines
                    .last()
                    .is_some_and(|&l| !text.line(l).chars().last().is_some_and(|c| c == '\n')),
            );
        let shift = |pos: usize| pos + block;
        let sel = self
            .selection
            .clone()
            .transform(|r| Range::new(shift(r.anchor), shift(r.head)));
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
    }

    // -----------------------------------------------------------------------------------------
    // movement
    // -----------------------------------------------------------------------------------------

    fn move_each(
        &mut self,
        extend: bool,
        keep_goal: bool,
        f: impl Fn(RopeSlice, Range, Option<usize>) -> usize,
    ) {
        self.commit_pending();
        let text = self.text.slice(..);
        let goal = self.goal_col;
        self.selection = self.selection.clone().transform(|r| {
            let head = f(text, r, goal);
            if extend {
                Range::new(r.anchor, head)
            } else {
                Range::point(head)
            }
        });
        if !keep_goal {
            self.goal_col = None;
        }
    }

    pub fn move_horizontal(&mut self, right: bool, extend: bool) {
        self.move_each(extend, false, |t, r, _| {
            if !extend && !r.is_empty() {
                return if right { r.to() } else { r.from() };
            }
            if right {
                next_grapheme_boundary(t, r.head)
            } else {
                prev_grapheme_boundary(t, r.head)
            }
        });
    }

    pub fn move_word(&mut self, right: bool, extend: bool) {
        self.move_each(extend, false, |t, r, _| {
            if right {
                word_right(t, r.head)
            } else {
                word_left(t, r.head)
            }
        });
    }

    pub fn move_vertical(&mut self, lines: isize, extend: bool) {
        let text = self.text.slice(..);
        let tab = self.tab_width;
        let goal = self
            .goal_col
            .unwrap_or_else(|| visual_col(text, self.selection.primary().head, tab));
        self.goal_col = Some(goal);
        self.move_each(extend, true, |t, r, g| {
            let line = t.char_to_line(r.head) as isize;
            let last = t.len_lines().saturating_sub(1) as isize;
            let target = line + lines;
            if target < 0 {
                return 0;
            }
            if target > last {
                return t.len_chars();
            }
            let goal = g.unwrap_or_else(|| visual_col(t, r.head, tab));
            char_at_visual_col(t, target as usize, goal, tab)
        });
    }

    pub fn move_line_start(&mut self, extend: bool) {
        self.move_each(extend, false, |t, r, _| {
            let ls = t.line_to_char(t.char_to_line(r.head));
            let first = ls
                + t.line(t.char_to_line(r.head))
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .count();
            if r.head == first {
                ls
            } else {
                first
            }
        });
    }

    pub fn move_line_end(&mut self, extend: bool) {
        self.move_each(extend, false, |t, r, _| line_end(t, t.char_to_line(r.head)));
    }

    pub fn move_doc(&mut self, end: bool, extend: bool) {
        self.move_each(extend, false, |t, _, _| if end { t.len_chars() } else { 0 });
    }

    pub fn select_all(&mut self) {
        self.commit_pending();
        self.selection = Selection::single(0, self.text.len_chars());
    }

    pub fn collapse_selection(&mut self) {
        self.selection = self.selection.clone().transform(|r| Range::point(r.head));
        self.selection = Selection::point(self.selection.primary().head);
    }

    /// (line, visual column) of the primary cursor, 0-based.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let t = self.text.slice(..);
        let h = self.selection.primary().head;
        (t.char_to_line(h), visual_col(t, h, self.tab_width))
    }

    /// Char index of (line, char column), clamped to the line's end and the text.
    pub fn char_at(&self, line: usize, col: usize) -> usize {
        let t = self.text.slice(..);
        if line >= t.len_lines() {
            return t.len_chars();
        }
        (t.line_to_char(line) + col).min(line_end(t, line))
    }

    /// (line, char column) of a char index; the inverse of `char_at`.
    pub fn line_col_of(&self, pos: usize) -> (usize, usize) {
        let t = self.text.slice(..);
        let pos = pos.min(t.len_chars());
        let line = t.char_to_line(pos);
        (line, pos - t.line_to_char(line))
    }

    /// The identifier touching `pos` (letters, digits, `_`), as a char range.
    pub fn word_at(&self, pos: usize) -> Option<(usize, usize)> {
        let t = self.text.slice(..);
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut a = pos.min(t.len_chars());
        while a > 0 && is_word(t.char(a - 1)) {
            a -= 1;
        }
        let mut b = pos.min(t.len_chars());
        while b < t.len_chars() && is_word(t.char(b)) {
            b += 1;
        }
        (a < b).then_some((a, b))
    }

    /// Text-only "all occurrences": the primary selection's text, or the whole word under the
    /// cursor, everywhere in the buffer (whole words only for the latter).
    pub fn text_occurrences(&self) -> Vec<(usize, usize)> {
        let t = self.text.slice(..);
        let p = self.selection.primary();
        let (needle, whole_word) = if p.is_empty() {
            match self.word_at(p.head) {
                Some((a, b)) => (t.slice(a..b).to_string(), true),
                None => return Vec::new(),
            }
        } else {
            (t.slice(p.from()..p.to()).to_string(), false)
        };
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let hay = self.text.to_string();
        let n = needle.chars().count();
        let mut out = Vec::new();
        let mut char_idx = 0;
        let mut last_byte = 0;
        for (byte, _) in hay.match_indices(&needle) {
            char_idx += hay[last_byte..byte].chars().count();
            last_byte = byte;
            let (a, b) = (char_idx, char_idx + n);
            let bounded = !whole_word
                || ((a == 0 || !is_word(t.char(a - 1))) && (b >= t.len_chars() || !is_word(t.char(b))));
            if bounded {
                out.push((a, b));
            }
        }
        out
    }

    /// Select each range (multi-cursor). The primary is the range containing the cursor, else
    /// the first. Ranges must be sorted and non-overlapping; empty input changes nothing.
    pub fn select_ranges(&mut self, ranges: &[(usize, usize)]) {
        if ranges.is_empty() {
            return;
        }
        self.commit_pending();
        let head = self.selection.primary().head;
        let n = self.text.len_chars();
        let primary = ranges.iter().position(|&(a, b)| a <= head && head <= b).unwrap_or(0);
        let rs = ranges.iter().map(|&(a, b)| Range::new(a.min(n), b.min(n))).collect();
        self.selection = Selection::new(rs, primary);
        self.goal_col = None;
    }

    /// Replace char ranges `(from, to, text)` in one undo step (a rename's edits). Ranges must
    /// not overlap; cursors move with the text around them.
    pub fn apply_edits(&mut self, mut edits: Vec<(usize, usize, String)>) {
        if edits.is_empty() {
            return;
        }
        edits.sort_by_key(|e| e.0);
        let n = self.text.len_chars();
        let tx = Transaction::change(
            &self.text,
            edits.into_iter().map(|(a, b, s)| (a.min(n), b.min(n).max(a.min(n)), Some(s.into()))),
        );
        let sel = self.selection.clone().map(tx.changes());
        self.commit_pending();
        self.apply(tx.with_selection(sel), EditKind::Discrete);
    }

    pub fn set_cursor(&mut self, pos: usize, extend: bool) {
        self.commit_pending();
        let pos = pos.min(self.text.len_chars());
        let anchor = if extend {
            self.selection.primary().anchor
        } else {
            pos
        };
        self.selection = Selection::single(anchor, pos);
        self.goal_col = None;
    }
}

pub fn line_end(t: RopeSlice, line: usize) -> usize {
    helix_core::line_ending::line_end_char_index(&t, line)
}

pub fn visual_col(t: RopeSlice, pos: usize, tab: usize) -> usize {
    let ls = t.line_to_char(t.char_to_line(pos));
    let mut col = 0;
    for c in t.slice(ls..pos).chars() {
        col += if c == '\t' {
            tab - col % tab
        } else {
            char_width(c)
        };
    }
    col
}

pub fn char_at_visual_col(t: RopeSlice, line: usize, goal: usize, tab: usize) -> usize {
    let ls = t.line_to_char(line);
    let le = line_end(t, line);
    let mut col = 0;
    let mut pos = ls;
    for c in t.slice(ls..le).chars() {
        let w = if c == '\t' {
            tab - col % tab
        } else {
            char_width(c)
        };
        if col + w > goal {
            break;
        }
        col += w;
        pos += 1;
    }
    pos
}

pub fn char_width(c: char) -> usize {
    helix_core::unicode::width::UnicodeWidthChar::width(c).unwrap_or(0)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
    Newline,
}

fn class(c: char) -> Class {
    if c == '\n' || c == '\r' {
        Class::Newline
    } else if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// VSCode-style: skip whitespace, then a run of one class.
pub fn word_right(t: RopeSlice, mut pos: usize) -> usize {
    let n = t.len_chars();
    if pos < n && class(t.char(pos)) == Class::Newline {
        return pos + 1;
    }
    while pos < n && class(t.char(pos)) == Class::Space {
        pos += 1;
    }
    if pos < n {
        let k = class(t.char(pos));
        if k == Class::Newline {
            return pos;
        }
        while pos < n && class(t.char(pos)) == k {
            pos += 1;
        }
    }
    pos
}

pub fn word_left(t: RopeSlice, mut pos: usize) -> usize {
    if pos > 0 && class(t.char(pos - 1)) == Class::Newline {
        return pos - 1;
    }
    while pos > 0 && class(t.char(pos - 1)) == Class::Space {
        pos -= 1;
    }
    if pos > 0 {
        let k = class(t.char(pos - 1));
        if k == Class::Newline {
            return pos;
        }
        while pos > 0 && class(t.char(pos - 1)) == k {
            pos -= 1;
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Document {
        let mut d = Document::new(Rope::from(s), Some("t.py".into()));
        d.attach_syntax();
        d
    }

    #[test]
    fn typing_groups_into_one_undo_per_word() {
        let mut d = doc("");
        for c in "foo bar".chars() {
            d.type_char(c);
        }
        assert_eq!(d.text, "foo bar");
        assert!(d.undo());
        assert_eq!(d.text, "foo");
        assert!(d.undo());
        assert_eq!(d.text, "");
        assert!(d.redo());
        assert!(d.redo());
        assert_eq!(d.text, "foo bar");
        assert_eq!(d.selection.primary().head, 7);
    }

    #[test]
    fn typing_mid_line_inserts() {
        let mut d = doc("echo hi\n");
        d.set_cursor(0, false);
        for c in "xy".chars() {
            d.type_char(c);
        }
        assert_eq!(d.text, "xyecho hi\n");
        assert_eq!(d.selection.primary().head, 2);
    }

    #[test]
    fn newline_python_indent() {
        let mut d = doc("def f():");
        d.move_doc(true, false);
        d.newline();
        d.type_char('x');
        assert_eq!(d.text, "def f():\n    x");
    }

    #[test]
    fn backspace_tab_stop() {
        let mut d = doc("        x");
        d.set_cursor(8, false);
        d.delete_backward();
        assert_eq!(d.text, "    x");
    }

    #[test]
    fn selection_replace_and_modified() {
        let mut d = doc("hello world");
        assert!(!d.is_modified());
        d.set_cursor(0, false);
        d.move_word(true, true);
        d.type_char('H');
        assert_eq!(d.text, "H world");
        assert!(d.is_modified());
        let rev = d.save_point();
        d.mark_saved(rev, 1);
        assert!(!d.is_modified());
        d.undo();
        assert!(d.is_modified());
    }

    #[test]
    fn vertical_goal_column() {
        let mut d = doc("abcdef\nab\nabcdef");
        d.set_cursor(5, false);
        d.move_vertical(1, false);
        assert_eq!(d.cursor_line_col(), (1, 2));
        d.move_vertical(1, false);
        assert_eq!(d.cursor_line_col(), (2, 5));
    }

    #[test]
    fn comment_toggle() {
        let mut d = doc("a\n  b\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.text, "# a\n#   b\n");
        d.toggle_comment();
        assert_eq!(d.text, "a\n  b\n");
    }

    #[test]
    fn syntax_survives_edits_and_undo() {
        let mut d = doc("x = 1\n");
        assert_eq!(d.language_name(), "python");
        d.move_doc(true, false);
        for c in "def g(): pass".chars() {
            d.type_char(c);
        }
        d.undo();
        d.undo();
        let n = d.text.len_bytes();
        let incr = d.syntax.as_ref().unwrap().spans(&d.text, 0..n);
        let fresh = Syntax::new(&d.text, d.syntax.as_ref().unwrap().lang)
            .unwrap()
            .spans(&d.text, 0..n);
        assert_eq!(incr, fresh);
    }

    #[test]
    fn change_all_occurrences_edits_every_whole_word() {
        let mut d = doc("foo = 1\nfoobar = foo + foo_x\nprint(foo)\n");
        d.set_cursor(d.char_at(1, 10), false); // inside the second line's `foo`
        let occ = d.text_occurrences();
        assert_eq!(occ.len(), 3, "{occ:?}");
        d.select_ranges(&occ);
        assert_eq!(d.selection.primary().from(), d.char_at(1, 9));
        for c in "bar".chars() {
            d.type_char(c);
        }
        assert_eq!(d.text, "bar = 1\nfoobar = bar + foo_x\nprint(bar)\n");
        d.undo();
        assert_eq!(d.text, "foo = 1\nfoobar = foo + foo_x\nprint(foo)\n");
    }

    #[test]
    fn edits_apply_in_one_undo_step() {
        let mut d = doc("é = 1\ny = é\n");
        d.set_cursor(d.text.len_chars(), false);
        d.apply_edits(vec![(d.char_at(1, 4), d.char_at(1, 5), "abc".into()), (0, 1, "abc".into())]);
        assert_eq!(d.text, "abc = 1\ny = abc\n");
        assert_eq!(d.selection.primary().head, d.text.len_chars());
        assert_eq!(d.line_col_of(d.char_at(1, 99)), (1, 7));
        d.undo();
        assert_eq!(d.text, "é = 1\ny = é\n");
    }
}
