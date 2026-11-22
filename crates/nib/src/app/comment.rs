//! Comments on lines of a text: a plan an agent asks to have approved, a diff (nibdiff). The
//! model and the draft being written know nothing of Iced, so a custom widget (the diff grid)
//! can use them as they are; `view` draws them with stock widgets where those fit.
//!
//! Lines are 1-based and both ends are included, counted in the text the comment is on. The
//! field names are the ones the review store uses (`start_line`, `end_line`, `anchor`, …).

use serde_json::{json, Value};

/// Who wrote what the user writes.
pub const USER: &str = "user";

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub id: String,
    pub body: String,
    pub author: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub id: String,
    pub start_line: u32,
    pub end_line: u32,
    /// The text of those lines when it was written: what it was about, should they move.
    pub anchor: String,
    pub body: String,
    pub author: String,
    pub resolved: bool,
    /// Its lines changed and their text wasn't found again: `start_line` is a guess.
    pub drifted: bool,
    pub replies: Vec<Reply>,
}

impl Comment {
    pub fn lines(&self) -> String {
        lines_label(self.start_line, self.end_line)
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "start_line": self.start_line,
            "end_line": self.end_line,
            "anchor": self.anchor,
            "body": self.body,
            "author": self.author,
            "resolved": self.resolved,
            "drifted": self.drifted,
            "replies": self.replies.iter().map(|r| json!({ "id": r.id, "body": r.body, "author": r.author })).collect::<Vec<_>>(),
        })
    }
}

/// `line 4`, `lines 4–6`.
pub fn lines_label(start: u32, end: u32) -> String {
    if start == end { format!("line {start}") } else { format!("lines {start}–{end}") }
}

/// A comment being written. Its range grows from the lines it was opened on (`from`) to take in
/// others, whichever side of them they are.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub start_line: u32,
    pub end_line: u32,
    from: (u32, u32),
    pub body: String,
    /// The comment it edits (out of the list while it is open; back as it was on cancel).
    editing: Option<Comment>,
}

impl Draft {
    pub fn new(start_line: u32, end_line: u32) -> Self {
        Draft { start_line, end_line, from: (start_line, end_line), body: String::new(), editing: None }
    }

    /// Stretch the range from where it was opened to take in `start..=end`, or shrink it back
    /// towards there.
    pub fn extend(&mut self, start: u32, end: u32) {
        self.start_line = self.from.0.min(start);
        self.end_line = self.from.1.max(end);
    }

    pub fn lines(&self) -> String {
        lines_label(self.start_line, self.end_line)
    }

    pub fn blank(&self) -> bool {
        self.body.trim().is_empty()
    }

    pub fn insert(&mut self, s: &str) {
        self.body.push_str(&s.replace("\r\n", "\n"));
    }

    pub fn backspace(&mut self) {
        self.body.pop();
    }

    /// Take back the last word (and the spaces after it).
    pub fn backspace_word(&mut self) {
        let kept = self.body.trim_end().len();
        self.body.truncate(kept);
        let cut = self.body.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        self.body.truncate(cut);
    }

    /// Take back the line being written.
    pub fn backspace_line(&mut self) {
        let cut = self.body.trim_end_matches('\n').rfind('\n').map_or(0, |i| i + 1);
        self.body.truncate(cut);
    }

    pub fn json(&self) -> Value {
        json!({ "start_line": self.start_line, "end_line": self.end_line, "body": self.body, "editing": self.editing.as_ref().map(|c| c.id.as_str()) })
    }
}

/// The comments on one text, and the one being written.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comments {
    list: Vec<Comment>,
    pub draft: Option<Draft>,
    next: u32,
}

impl Comments {
    pub fn list(&self) -> &[Comment] {
        &self.list
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Those whose last line is `line`: they show under it.
    pub fn ending_at(&self, line: u32) -> impl Iterator<Item = &Comment> {
        self.list.iter().filter(move |c| c.end_line == line)
    }

    /// Some comment (or the draft) is on `line`.
    pub fn covers(&self, line: u32) -> bool {
        self.list.iter().any(|c| (c.start_line..=c.end_line).contains(&line))
    }

    /// Start writing one on `start..=end`. One being written already is kept (and stretched
    /// to take these lines in), so a click elsewhere never loses words.
    pub fn open(&mut self, start: u32, end: u32) {
        match &mut self.draft {
            Some(d) => d.extend(start, end),
            None => self.draft = Some(Draft::new(start, end)),
        }
    }

    /// File the draft. `anchor`: the text of its lines. A blank one is dropped, and an edited
    /// comment emptied this way is removed. The id of what was filed, if anything.
    pub fn save(&mut self, anchor: impl FnOnce(u32, u32) -> String) -> Option<String> {
        let d = self.draft.take()?;
        if d.blank() {
            return None;
        }
        let anchor = anchor(d.start_line, d.end_line);
        let c = match d.editing {
            Some(mut c) => {
                c.start_line = d.start_line;
                c.end_line = d.end_line;
                c.anchor = anchor;
                c.body = d.body.trim().to_string();
                c.drifted = false;
                c
            }
            None => {
                self.next += 1;
                Comment {
                    id: format!("c_{}", self.next),
                    start_line: d.start_line,
                    end_line: d.end_line,
                    anchor,
                    body: d.body.trim().to_string(),
                    author: USER.into(),
                    resolved: false,
                    drifted: false,
                    replies: vec![],
                }
            }
        };
        let id = c.id.clone();
        self.insert_sorted(c);
        Some(id)
    }

    /// Throw the draft away; a comment that was being edited goes back as it was.
    pub fn cancel(&mut self) {
        if let Some(c) = self.draft.take().and_then(|d| d.editing) {
            self.insert_sorted(c);
        }
    }

    /// Open a comment again as the draft, to change it. Whatever was being written is filed
    /// first if it can be.
    pub fn edit(&mut self, id: &str, anchor: impl FnOnce(u32, u32) -> String) {
        if self.draft.is_some() {
            self.save(anchor);
        }
        let Some(i) = self.list.iter().position(|c| c.id == id) else { return };
        let c = self.list.remove(i);
        self.draft = Some(Draft { start_line: c.start_line, end_line: c.end_line, from: (c.start_line, c.end_line), body: c.body.clone(), editing: Some(c) });
    }

    pub fn remove(&mut self, id: &str) {
        self.list.retain(|c| c.id != id);
    }

    /// These are the filed ones now (they are kept elsewhere: nibdiff's review). The one being
    /// written stays.
    pub fn set(&mut self, list: Vec<Comment>) {
        self.list.clear();
        for c in list {
            self.insert_sorted(c);
        }
    }

    /// In the order of their lines, so they read top to bottom.
    fn insert_sorted(&mut self, c: Comment) {
        let at = self.list.iter().position(|o| (o.start_line, o.end_line) > (c.start_line, c.end_line)).unwrap_or(self.list.len());
        self.list.insert(at, c);
    }

    pub fn json(&self) -> Value {
        json!({ "comments": self.list.iter().map(Comment::json).collect::<Vec<_>>(), "draft": self.draft.as_ref().map(Draft::json) })
    }
}

// ---------------------------------------------------------------------------------------------
// views: stock widgets
// ---------------------------------------------------------------------------------------------

pub mod view {
    use super::{Comment, Draft};
    use iced::widget::{button, column, container, row, space, text};
    use iced::{Alignment, Border, Color, Element, Length};

    /// The colours and type comments are drawn in.
    #[derive(Debug, Clone, Copy)]
    pub struct Look {
        pub fg: Color,
        pub dim: Color,
        pub accent: Color,
        /// A resolved comment's outline.
        pub divider: Color,
        pub size: f32,
    }

    /// A little text button, outlined while hovered.
    fn link<'a, M: Clone + 'a>(label: &'a str, color: Color, look: Look, msg: M) -> Element<'a, M> {
        let dim = look.dim;
        button(text(label).size(look.size - 2.0).color(color))
            .on_press(msg)
            .padding([0, 5])
            .style(move |_: &iced::Theme, status: button::Status| button::Style {
                border: Border { color: if matches!(status, button::Status::Hovered) { dim } else { Color::TRANSPARENT }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            })
            .into()
    }

    /// A card in the comments' style: lifted a little off what it sits on, outlined in `edge`.
    fn card<'a, M: 'a>(body: Element<'a, M>, edge: Color, look: Look) -> Element<'a, M> {
        let lift = Color { a: 0.05, ..look.fg };
        container(body)
            .padding([7, 11])
            .width(Length::Fill)
            .style(move |_| container::Style { background: Some(lift.into()), border: Border { color: edge, width: 1.0, radius: 6.0.into() }, ..Default::default() })
            .into()
    }

    /// Who wrote it, as the reader sees it.
    fn who(author: &str) -> &str {
        if author == super::USER { "you" } else { author }
    }

    /// A filed comment and its replies. `edit` / `remove`: `None` where it can't be changed.
    pub fn thread<'a, M: Clone + 'a>(c: &'a Comment, look: Look, edit: Option<M>, remove: Option<M>) -> Element<'a, M> {
        let mut head = row![text(who(&c.author)).size(look.size - 2.0).color(look.accent), text(c.lines()).size(look.size - 2.0).color(look.dim)].spacing(8).align_y(Alignment::Center);
        if c.drifted {
            head = head.push(text("moved").size(look.size - 2.0).color(look.dim));
        }
        head = head.push(space::horizontal());
        if let Some(m) = edit {
            head = head.push(link("edit", look.dim, look, m));
        }
        if let Some(m) = remove {
            head = head.push(link("✕", look.dim, look, m));
        }
        let mut body = column![head, text(c.body.as_str()).size(look.size).color(look.fg)].spacing(3);
        for r in &c.replies {
            body = body.push(column![text(who(&r.author)).size(look.size - 2.0).color(look.accent), text(r.body.as_str()).size(look.size).color(look.fg)].spacing(2).padding([4, 0]));
        }
        card(body.into(), if c.resolved { look.divider } else { Color { a: 0.45, ..look.accent } }, look)
    }

    /// The comment being written: its words so far with a caret, and how to file or drop it.
    pub fn draft<'a, M: Clone + 'a>(d: &'a Draft, look: Look, save: M, cancel: M) -> Element<'a, M> {
        let head = row![text("your comment").size(look.size - 2.0).color(look.accent), text(d.lines()).size(look.size - 2.0).color(look.dim)].spacing(8);
        let words: Element<'a, M> = if d.body.is_empty() {
            row![text("▏").size(look.size).color(look.accent), text("Write a comment…").size(look.size).color(look.dim)].into()
        } else {
            text(format!("{}▏", d.body)).size(look.size).color(look.fg).into()
        };
        let keys = row![
            link("comment · ↵", look.accent, look, save),
            link("cancel · esc", look.dim, look, cancel),
            text("⇧↵ new line · ⇧↑↓ more rows").size(look.size - 2.0).color(look.dim),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        card(column![head, words, keys].spacing(5).into(), look.accent, look)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(a: u32, b: u32) -> String {
        format!("{a}-{b}")
    }

    #[test]
    fn a_draft_grows_both_ways_from_where_it_opened() {
        let mut d = Draft::new(5, 6);
        d.extend(9, 9);
        assert_eq!((d.start_line, d.end_line), (5, 9));
        d.extend(2, 3);
        assert_eq!((d.start_line, d.end_line), (2, 6), "back above: the far end returns");
    }

    #[test]
    fn filing_editing_and_cancelling() {
        let mut c = Comments::default();
        c.open(4, 4);
        assert_eq!(c.save(anchor), None, "a blank draft files nothing");
        c.open(7, 8);
        c.draft.as_mut().unwrap().insert("too vague ");
        c.open(2, 2);
        assert_eq!(c.draft.as_ref().map(|d| (d.start_line, d.end_line)), Some((2, 8)), "a click elsewhere stretches, never loses words");
        let id = c.save(anchor).unwrap();
        assert_eq!((c.list()[0].body.as_str(), c.list()[0].anchor.as_str()), ("too vague", "2-8"));
        c.open(1, 1);
        c.draft.as_mut().unwrap().insert("first");
        c.save(anchor);
        assert_eq!(c.list()[0].body, "first", "kept in the order of their lines");
        c.edit(&id, anchor);
        assert_eq!(c.len(), 1);
        c.draft.as_mut().unwrap().insert("!");
        c.cancel();
        assert_eq!(c.list()[1].body, "too vague", "cancel puts it back as it was");
        c.edit(&id, anchor);
        c.draft.as_mut().unwrap().body.clear();
        c.save(anchor);
        assert_eq!(c.len(), 1, "emptied while edited: removed");
    }

    #[test]
    fn taking_words_back() {
        let mut d = Draft::new(1, 1);
        d.insert("one two  ");
        d.backspace_word();
        assert_eq!(d.body, "one ");
        d.insert("\nthree four");
        d.backspace_line();
        assert_eq!(d.body, "one \n");
    }
}
