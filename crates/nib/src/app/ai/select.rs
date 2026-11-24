//! Text selected with the mouse in the chat, to copy: it can start in one paragraph of one turn
//! and end in another turn. Each piece of text in the feed that can be selected is a block,
//! keyed by its turn and its number in that turn; a selection is from a place in one block
//! (a byte offset) to a place in another. The blocks' text is noted while the feed is drawn, so
//! what is copied is what was shown.

use super::Msg;
use crate::app::Message;
use crate::selectable::{plain, Ev, Selectable};
use iced::advanced::mouse::click::Kind;
use iced::widget::markdown::Uri;
use iced::widget::text::Span;
use iced::{Color, Element, Font, Length};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// A block: its turn, and its number among the turn's blocks (in the order they are shown).
pub(in crate::app) type Key = (u32, u32);

/// Whose feed a selection is in: the chat's key, and the sub-agent thread shown, if one is.
pub(in crate::app) type Owner = (u64, Option<String>);

/// What the mouse did to block text (`Ev` without the link: a link clicked opens it).
#[derive(Debug, Clone, Copy)]
pub(crate) enum SelEv {
    Press(usize, Kind),
    Drag(usize),
    Release,
}

/// Before a block's text when copied after another: a paragraph's break, or a line's.
pub(in crate::app) const PARAGRAPH: &str = "\n\n";
pub(in crate::app) const LINE: &str = "\n";

#[derive(Debug, Default)]
pub(in crate::app) struct FeedSel {
    anchor: Option<(Key, usize)>,
    head: Option<(Key, usize)>,
    /// The button is held since a press on block text.
    pub dragging: bool,
    /// The feed it was made in: shown another, it is not there.
    owner: Option<Owner>,
    /// Each block's text, and what goes before it when copied after another.
    blocks: RefCell<BTreeMap<Key, (String, &'static str)>>,
}

impl FeedSel {
    /// Start to end, in the order shown.
    fn ordered(&self) -> Option<((Key, usize), (Key, usize))> {
        let (a, h) = (self.anchor?, self.head?);
        Some(if a <= h { (a, h) } else { (h, a) })
    }

    /// Something is selected (not only a place clicked).
    pub fn is_some(&self) -> bool {
        self.ordered().is_some_and(|(a, b)| a != b)
    }

    pub fn clear(&mut self) {
        (self.anchor, self.head, self.dragging, self.owner) = (None, None, false, None);
    }

    /// This, if it was made in `owner`'s feed.
    pub fn of(&self, owner: &Owner) -> Option<&Self> {
        (self.owner.as_ref() == Some(owner)).then_some(self)
    }

    /// The blocks are drawn anew: forget what they said.
    pub fn forget_blocks(&self) {
        self.blocks.borrow_mut().clear();
    }

    /// Note block `key`'s text, as drawn.
    pub fn note(&self, key: Key, text: String, before: &'static str) {
        self.blocks.borrow_mut().insert(key, (text, before));
    }

    /// The part of block `key` (`len` bytes long) that is selected.
    pub fn range_in(&self, key: Key, len: usize) -> Option<(usize, usize)> {
        let ((ka, a), (kb, b)) = self.ordered()?;
        if key < ka || key > kb {
            return None;
        }
        let lo = if key == ka { a.min(len) } else { 0 };
        let hi = if key == kb { b.min(len) } else { len };
        (lo < hi).then_some((lo, hi))
    }

    /// What is selected, as text to copy.
    pub fn text(&self) -> Option<String> {
        if !self.is_some() {
            return None;
        }
        let ((ka, _), (kb, _)) = self.ordered()?;
        let blocks = self.blocks.borrow();
        let mut out = String::new();
        let mut turn = None;
        for (key, (text, before)) in blocks.range(ka..=kb) {
            let Some((lo, hi)) = self.range_in(*key, text.len()) else { continue };
            let Some(part) = text.get(lo..hi) else { continue };
            if !out.is_empty() {
                // Between turns a paragraph's break, whatever the block is.
                out.push_str(if turn == Some(key.0) { before } else { PARAGRAPH });
            }
            turn = Some(key.0);
            out.push_str(part);
        }
        (!out.is_empty()).then_some(out)
    }

    /// A mouse event on block `key` of `owner`'s feed.
    pub fn on(&mut self, owner: Owner, key: Key, ev: SelEv) {
        if self.owner.as_ref() != Some(&owner) {
            if !matches!(ev, SelEv::Press(..)) {
                return;
            }
            self.clear();
            self.owner = Some(owner);
        }
        match ev {
            SelEv::Press(at, kind) => {
                let text = self.blocks.borrow().get(&key).map(|(t, _)| t.clone()).unwrap_or_default();
                let at = at.min(text.len());
                let (a, b) = match kind {
                    Kind::Single => (at, at),
                    Kind::Double => word_at(&text, at),
                    Kind::Triple => (0, text.len()),
                };
                (self.anchor, self.head, self.dragging) = (Some((key, a)), Some((key, b)), true);
            }
            SelEv::Drag(at) if self.dragging => self.head = Some((key, at)),
            SelEv::Drag(_) => {}
            SelEv::Release => self.dragging = false,
        }
    }
}

/// One turn's blocks as the feed draws them: numbered in order, noted, and highlighted where
/// the selection is.
pub(in crate::app) struct Blocks<'m> {
    feed: &'m FeedSel,
    /// The selection is in this feed (else nothing here is highlighted).
    shown: bool,
    turn: u32,
    next: Cell<u32>,
    /// Inside a list: items are joined by a line break (but the first is a paragraph's).
    lists: Cell<u32>,
    list_first: Cell<bool>,
    bg: Color,
}

impl<'m> Blocks<'m> {
    pub fn new(feed: &'m FeedSel, shown: bool, turn: usize, bg: Color) -> Self {
        Self { feed, shown, turn: turn as u32, next: Cell::new(0), lists: Cell::new(0), list_first: Cell::new(false), bg }
    }

    fn key(&self) -> Key {
        let n = self.next.get();
        self.next.set(n + 1);
        (self.turn, n)
    }

    fn before(&self, line: bool) -> &'static str {
        if line || (self.lists.get() > 0 && !self.list_first.replace(false)) {
            LINE
        } else {
            PARAGRAPH
        }
    }

    /// The next block, selectable; `line`: copied after the one before with a line break.
    pub fn block<'a>(&self, spans: &[Span<'_, Uri, Font>], line: bool) -> Selectable<'a, Uri, Message> {
        let key = self.key();
        let text = plain(spans);
        let len = text.len();
        self.feed.note(key, text, self.before(line));
        let range = if self.shown { self.feed.range_in(key, len) } else { None };
        Selectable::new(spans, move |ev: Ev<Uri>| match ev {
            Ev::Press(at, kind) => Message::Ai(Msg::Sel(key, SelEv::Press(at, kind))),
            Ev::Drag(at) => Message::Ai(Msg::Sel(key, SelEv::Drag(at))),
            Ev::Release => Message::Ai(Msg::Sel(key, SelEv::Release)),
            Ev::Link(url) => Message::PreviewLink(url),
        })
        .selected(range, self.bg)
        .dragging(self.shown && self.feed.dragging)
    }

    /// A block drawn some other way (with formulas): copied with the rest, not highlighted.
    pub fn unselectable(&self, text: String) {
        let key = self.key();
        self.feed.note(key, text, self.before(false));
    }

    /// A list's items follow (until `list_end`).
    pub fn list_start(&self) {
        if self.lists.get() == 0 {
            self.list_first.set(true);
        }
        self.lists.set(self.lists.get() + 1);
    }

    pub fn list_end(&self) {
        self.lists.set(self.lists.get().saturating_sub(1));
    }
}

/// Plain text as one selectable block of `blocks`, in this size and colour.
pub(in crate::app) fn plain_block<'a>(blocks: &Blocks<'_>, text: &str, size: f32, color: Color, width: Length) -> Element<'a, Message> {
    blocks.block(&[Span::new(text.to_string())], false).size(size).color(color).width(width).into()
}

/// The word around `at` (letters, digits, `_`); off a word, the one character there.
fn word_at(s: &str, at: usize) -> (usize, usize) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    let start = s[..at].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(at, |(i, _)| i);
    let end = at + s[at..].char_indices().find(|(_, c)| !is_word(*c)).map_or(s.len() - at, |(i, _)| i);
    if start < end {
        return (start, end);
    }
    // Not on a word: the character after `at` (or before it, at the very end).
    match s[at..].chars().next() {
        Some(c) => (at, at + c.len_utf8()),
        None => (s[..at].chars().next_back().map_or(at, |c| at - c.len_utf8()), at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply of two paragraphs (turn 1) after the user's message (turn 0), then a list (turn 2).
    fn feed() -> FeedSel {
        let s = FeedSel::default();
        s.note((0, 0), "fix the build".into(), PARAGRAPH);
        s.note((1, 0), "The linker fails.".into(), PARAGRAPH);
        s.note((1, 1), "Run cargo clean first.".into(), PARAGRAPH);
        s.note((2, 0), "one".into(), LINE);
        s.note((2, 1), "two".into(), LINE);
        s
    }

    fn me() -> Owner {
        (1, None)
    }

    fn drag(s: &mut FeedSel, from: (Key, usize), to: (Key, usize)) {
        s.on(me(), from.0, SelEv::Press(from.1, Kind::Single));
        s.on(me(), to.0, SelEv::Drag(to.1));
        s.on(me(), to.0, SelEv::Release);
    }

    #[test]
    fn a_click_selects_nothing() {
        let mut s = feed();
        s.on(me(), (1, 0), SelEv::Press(4, Kind::Single));
        s.on(me(), (1, 0), SelEv::Release);
        assert!(!s.is_some());
        assert_eq!(s.text(), None);
    }

    #[test]
    fn inside_one_block() {
        let mut s = feed();
        drag(&mut s, ((1, 0), 4), ((1, 0), 10));
        assert_eq!(s.text().as_deref(), Some("linker"));
        assert_eq!(s.range_in((1, 0), 17), Some((4, 10)));
        assert_eq!(s.range_in((1, 1), 22), None);
    }

    #[test]
    fn backwards_and_across_blocks_and_turns() {
        let mut s = feed();
        drag(&mut s, ((2, 0), 2), ((0, 0), 8));
        assert_eq!(s.text().as_deref(), Some("build\n\nThe linker fails.\n\nRun cargo clean first.\n\non"));
        assert_eq!(s.range_in((1, 1), 22), Some((0, 22)), "a block in between is selected whole");
        assert_eq!(s.range_in((2, 0), 3), Some((0, 2)));
    }

    #[test]
    fn list_items_join_with_a_line_break() {
        let mut s = feed();
        drag(&mut s, ((2, 0), 0), ((2, 1), 3));
        assert_eq!(s.text().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn a_move_after_the_release_changes_nothing() {
        let mut s = feed();
        drag(&mut s, ((1, 0), 0), ((1, 0), 3));
        s.on(me(), (1, 1), SelEv::Drag(5));
        assert_eq!(s.text().as_deref(), Some("The"));
    }

    #[test]
    fn another_feed_has_none_of_it() {
        let mut s = feed();
        drag(&mut s, ((1, 0), 0), ((1, 0), 3));
        assert!(s.of(&me()).is_some() && s.of(&(1, Some("a1".into()))).is_none() && s.of(&(2, None)).is_none());
        s.on((2, None), (1, 0), SelEv::Drag(9));
        assert_eq!(s.text().as_deref(), Some("The"), "a drag in another feed doesn't move it");
        s.on((2, None), (1, 0), SelEv::Press(0, Kind::Triple));
        assert!(s.of(&me()).is_none(), "a press there starts a new one");
    }

    #[test]
    fn double_click_a_word_triple_the_block() {
        let mut s = feed();
        s.on(me(), (1, 1), SelEv::Press(6, Kind::Double));
        assert_eq!(s.text().as_deref(), Some("cargo"));
        s.on(me(), (1, 1), SelEv::Press(6, Kind::Triple));
        assert_eq!(s.text().as_deref(), Some("Run cargo clean first."));
        assert_eq!(word_at("a , b", 2), (2, 3), "off a word: the character");
        assert_eq!(word_at("ab, c", 2), (0, 2), "just after a word: that word");
        assert_eq!(word_at("héllo wörld", 9), (7, 13));
    }
}
