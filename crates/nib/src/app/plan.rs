//! A plan an agent hands over (Claude Code's `ExitPlanMode`), shown as rendered Markdown that
//! can be commented on: split into blocks (a heading, a paragraph, one item of a list, a code
//! block), each drawn on its own with a gutter to comment from, the comments under the lines
//! they are on. Used by the agents tab to approve plans, and by nibdiff beside a diff.
//!
//! Comments are on the plan's source lines (`comment`), so what goes back to the agent quotes
//! the exact text it wrote.

use super::code::MarkdownViewer;
use super::comment::{self, Comment, Comments};
use super::Message;
use iced::advanced::widget::{operate, operation, Id as WidgetId, Operation};
use iced::widget::{column, container, markdown, mouse_area, row, text};
use iced::{Border, Color, Element, Length, Padding, Rectangle, Task, Vector};
use pulldown_cmark::{Event, Options, Parser, Tag};
use serde_json::{json, Value};
use std::ops::Range;

/// One row of the plan: drawn, picked and commented on by itself.
pub struct Row {
    /// 1-based, both included.
    pub start_line: u32,
    pub end_line: u32,
    /// How deep in lists it is (0: not a list item).
    depth: usize,
    md: markdown::Content,
}

/// What became of a plan the user was asked to approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Approved,
    /// Sent back with comments or a reply, for the agent to revise.
    Revise,
    /// Refused outright (only from outside: `nib ctl ai deny`).
    Declined,
}

impl Outcome {
    pub fn word(self) -> &'static str {
        match self {
            Outcome::Approved => "approved",
            Outcome::Revise => "sent back for a revision",
            Outcome::Declined => "declined",
        }
    }
}

pub struct Plan {
    pub text: String,
    pub rows: Vec<Row>,
    pub comments: Comments,
    /// The rows picked (↑ / ↓ move, ⇧ takes in more, a click picks one): where the pick
    /// started, and the row the cursor is on.
    pub sel: Option<(usize, usize)>,
    /// Where the arrows start from when nothing is picked: the row of the last comment filed
    /// or dropped (so ↵ is free to send the plan, and ↓ goes on from there).
    home: Option<usize>,
    /// The row the comment being written was opened from (if it has no rows of its own).
    back: Option<usize>,
    /// Drawn whole; otherwise just its title (a plan that has been answered).
    pub open: bool,
    pub outcome: Option<Outcome>,
}

/// What the plan's view publishes.
#[derive(Debug, Clone)]
pub enum PlanMsg {
    /// A row was clicked: picked, or (picked already) commented on.
    Row(usize),
    Save,
    Cancel,
    Remove(String),
    /// Show it whole / just its title.
    Fold,
}

impl Plan {
    pub fn new(text: &str) -> Self {
        let rows = split(text).into_iter().map(|p| Row { start_line: p.start_line, end_line: p.end_line, depth: p.depth, md: markdown::Content::parse(&p.source(text)) }).collect();
        Plan { text: text.to_string(), rows, comments: Comments::default(), sel: None, home: None, back: None, open: true, outcome: None }
    }

    /// Its first heading, or its first line: what it is called.
    pub fn title(&self) -> &str {
        fn line(l: &str) -> &str {
            l.trim().trim_start_matches('#').trim()
        }
        let t = self.text.lines().find(|l| l.trim_start().starts_with('#')).or_else(|| self.text.lines().find(|l| !l.trim().is_empty())).map_or("", line);
        // Shown after the word "Plan" already.
        t.strip_prefix("Plan:").map_or(t, str::trim)
    }

    /// Its title as plain text (no `code` or **bold** marks).
    pub fn title_text(&self) -> String {
        self.title().chars().filter(|c| !matches!(c, '`' | '*' | '_')).collect()
    }

    /// The lines rows `a..=b` span (either order).
    fn span(&self, a: usize, b: usize) -> Option<(u32, u32)> {
        let (a, b) = (a.min(b), a.max(b));
        Some((self.rows.get(a)?.start_line, self.rows.get(b)?.end_line))
    }

    /// The row a line is on.
    fn row_of(&self, line: u32) -> Option<usize> {
        self.rows.iter().position(|r| (r.start_line..=r.end_line).contains(&line))
    }

    pub fn update(&mut self, msg: PlanMsg) {
        match msg {
            PlanMsg::Row(i) => self.click(i),
            PlanMsg::Save => self.save(),
            PlanMsg::Cancel => self.cancel(),
            PlanMsg::Remove(id) => self.comments.remove(&id),
            PlanMsg::Fold => self.open = !self.open,
        }
    }

    /// A click on a row picks it; a click on the row picked opens its comment. One being
    /// written is filed first (dropped if it has no words).
    fn click(&mut self, i: usize) {
        if self.comments.draft.is_some() {
            self.save();
        }
        if self.sel == Some((i, i)) {
            self.enter();
        } else if i < self.rows.len() {
            self.sel = Some((i, i));
        }
    }

    /// File the comment being written (a blank one is dropped); the arrows go on from its
    /// last row.
    pub fn save(&mut self) {
        self.settle();
        let text = &self.text;
        self.comments.save(|s, e| lines_of(text, s, e));
    }

    /// Drop the comment being written; the arrows go on from its last row.
    pub fn cancel(&mut self) {
        self.settle();
        self.comments.cancel();
    }

    /// Where the arrows go on from once the comment being written is put away: its last row, so
    /// ↓ leaves it (and ↵ there doesn't open it again by surprise).
    fn settle(&mut self) {
        let end = self.comments.draft.as_ref().and_then(|d| self.row_of(d.end_line));
        self.home = end.or(self.back.take());
        self.back = None;
    }

    /// ↑ (`-1`) / ↓: move the cursor a row; `more` (⇧): take that row into the pick, or, while
    /// a comment is being written, into the comment.
    pub fn step(&mut self, by: isize, more: bool) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let at = |i: usize| (i as isize + by).clamp(0, n as isize - 1) as usize;
        if let Some(d) = &self.comments.draft {
            if more {
                let edge = if by < 0 { d.start_line } else { d.end_line };
                let Some((s, e)) = self.row_of(edge).and_then(|i| self.span(at(i), at(i))) else { return };
                self.comments.draft.as_mut().unwrap().extend(s, e);
            }
            return;
        }
        self.sel = Some(match self.sel {
            None => {
                let first = match self.home {
                    Some(h) => at(h.min(n - 1)),
                    None if by < 0 => n - 1,
                    None => 0,
                };
                (first, first)
            }
            Some((from, to)) if more => (from, at(to)),
            Some((_, to)) => (at(to), at(to)),
        });
    }

    /// ↵ on the rows picked: open a comment on them, or the one already on that row to change
    /// it. `false`: nothing is picked.
    pub fn enter(&mut self) -> bool {
        let Some((a, b)) = self.sel else { return false };
        let Some((s, e)) = self.span(a, b) else { return false };
        let Some(id) = self.comments.list().iter().find(|c| a == b && c.start_line <= s && e <= c.end_line).map(|c| c.id.clone()) else { return self.write() };
        let text = &self.text;
        self.comments.edit(&id, |s, e| lines_of(text, s, e));
        self.back = Some(b);
        self.sel = None;
        true
    }

    /// Open a new comment on the rows picked, whatever is on them already (nibdiff, where a
    /// filed comment is answered, not changed). `false`: nothing is picked.
    pub fn write(&mut self) -> bool {
        let Some((a, b)) = self.sel else { return false };
        let Some((s, e)) = self.span(a, b) else { return false };
        self.comments.open(s, e);
        self.back = Some(b);
        self.sel = None;
        true
    }

    /// The lines of the rows picked.
    pub fn picked(&self) -> Option<(u32, u32)> {
        self.sel.and_then(|(a, b)| self.span(a, b))
    }

    /// Take the comment being written, to file it elsewhere (nibdiff: in the agent's review):
    /// its lines, their text, its words. `None`: it had no words (it is dropped), or there was
    /// none.
    pub fn take(&mut self) -> Option<(u32, u32, String, String)> {
        self.comments.draft.as_ref()?;
        self.settle();
        let d = self.comments.draft.take().filter(|d| !d.blank())?;
        Some((d.start_line, d.end_line, lines_of(&self.text, d.start_line, d.end_line), d.body.trim().to_string()))
    }

    /// Show `list` as the filed comments (nibdiff: those of the agent's review that are on the
    /// plan), each on whole rows, so that it has a row to show under whatever lines it names.
    pub fn show(&mut self, mut list: Vec<Comment>) {
        let row = |line: u32| self.rows.iter().rev().find(|r| r.start_line <= line).or(self.rows.first());
        for c in &mut list {
            c.start_line = row(c.start_line).map_or(c.start_line, |r| r.start_line);
            c.end_line = row(c.end_line).map_or(c.end_line, |r| r.end_line);
        }
        self.comments.set(list);
    }

    /// Something would go back to the agent besides a yes: comments, or a draft with words.
    pub fn has_feedback(&self) -> bool {
        !self.comments.is_empty() || self.comments.draft.as_ref().is_some_and(|d| !d.blank())
    }

    /// What the agent is told when the plan goes back with the user's comments and `reply`
    /// (either may be missing). It says what to do with them, so the agent needs no skill.
    pub fn feedback(&self, reply: &str) -> String {
        let mut out = String::from("The user reviewed your plan and is sending it back for a revision. Don't start the work yet.\n");
        let reply = reply.trim();
        if !self.comments.is_empty() {
            out.push_str("\nTheir comments, each under the lines of your plan it is about:\n");
            for (n, c) in self.comments.list().iter().enumerate() {
                out.push_str(&format!("\n{}. On {}:\n", n + 1, c.lines()));
                const MOST: usize = 12;
                let quoted: Vec<&str> = c.anchor.lines().collect();
                for l in quoted.iter().take(MOST) {
                    out.push_str(&if l.trim().is_empty() { ">\n".to_string() } else { format!("> {l}\n") });
                }
                if quoted.len() > MOST {
                    out.push_str(&format!("> … ({} more lines)\n", quoted.len() - MOST));
                }
                out.push_str(&c.body);
                out.push('\n');
            }
        }
        if !reply.is_empty() {
            out.push_str(&format!("\nWhat they say about the plan as a whole:\n{reply}\n"));
        }
        out.push_str("\nRevise the plan so it answers every comment");
        if !reply.is_empty() {
            out.push_str(" and what they said");
        }
        out.push_str(". If they ask you to drop it, do so and say what you would do instead. If a comment is a question, answer it in the plan or in your reply. Then hand in the whole revised plan again (ExitPlanMode), not only the parts that changed.");
        out
    }

    pub fn json(&self) -> Value {
        let mut v = self.comments.json();
        v["title"] = json!(self.title());
        v["outcome"] = json!(self.outcome.map(Outcome::word));
        v["rows"] = json!(self.rows.iter().map(|r| json!([r.start_line, r.end_line])).collect::<Vec<_>>());
        v["selected"] = json!(self.picked());
        v
    }
}

fn lines_of(text: &str, start: u32, end: u32) -> String {
    text.lines().skip(start.saturating_sub(1) as usize).take((end + 1).saturating_sub(start) as usize).collect::<Vec<_>>().join("\n")
}

/// Where a row is in the plan's source.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub range: Range<usize>,
    pub start_line: u32,
    pub end_line: u32,
    /// How deep in lists (0: not a list item).
    pub depth: usize,
}

impl Part {
    /// Its Markdown on its own: a list item's later lines lose the indent it has in the plan.
    fn source(&self, md: &str) -> String {
        let col = self.range.start - md[..self.range.start].rfind('\n').map_or(0, |i| i + 1);
        let mut lines = md[self.range.clone()].lines();
        let mut out = lines.next().unwrap_or_default().to_string();
        for l in lines {
            let strip = l.len() - l.trim_start_matches(' ').len();
            out.push('\n');
            out.push_str(&l[strip.min(col)..]);
        }
        out
    }
}

/// The plan's rows: each top-level block (heading, paragraph, code block, table, quote), and
/// each item of a list at any depth (without the items nested in it, which are rows of their
/// own).
pub fn split(md: &str) -> Vec<Part> {
    enum Frame {
        List,
        /// A list item, and its row.
        Item(usize),
        Other,
    }
    let line_at = |byte: usize| md[..byte].matches('\n').count() as u32 + 1;
    // Leave out the blank lines that end some of them.
    let part = |r: Range<usize>, depth: usize| {
        let end = r.start + md[r.clone()].trim_end().len();
        (end > r.start).then(|| Part { range: r.start..end, start_line: line_at(r.start), end_line: line_at(end - 1), depth })
    };
    let mut out: Vec<Part> = vec![];
    let mut stack: Vec<Frame> = vec![];
    for (event, r) in Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                let in_lists = stack.iter().all(|f| !matches!(f, Frame::Other));
                let frame = match (&tag, stack.last()) {
                    (Tag::List(_), None) => Frame::List,
                    (_, None) => {
                        out.extend(part(r, 0));
                        Frame::Other
                    }
                    (Tag::Item, Some(Frame::List)) if in_lists => {
                        let depth = stack.iter().filter(|f| matches!(f, Frame::List)).count();
                        match part(r, depth) {
                            Some(p) => {
                                out.push(p);
                                Frame::Item(out.len() - 1)
                            }
                            None => Frame::Other,
                        }
                    }
                    // A list inside an item: the item's row ends where it starts.
                    (Tag::List(_), Some(Frame::Item(i))) => {
                        let i = *i;
                        let start = out[i].range.start;
                        if r.start > start && r.start < out[i].range.end {
                            if let Some(p) = part(start..r.start, out[i].depth) {
                                out[i] = p;
                            }
                        }
                        Frame::List
                    }
                    _ => Frame::Other,
                };
                stack.push(frame);
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Rule | Event::Html(_) if stack.is_empty() => out.extend(part(r, 0)),
            _ => {}
        }
    }
    out
}

/// How the plan is drawn.
pub struct Look {
    pub comment: comment::view::Look,
    pub markdown: markdown::Settings,
    /// For code blocks (prose is in `markdown`'s faces).
    pub mono: iced::Font,
    /// Rows picked, or in the comment being written.
    pub lit: Color,
}

impl Look {
    /// In the theme's colours: prose in the UI face, code in `mono`, at `size` (≤ 14 reads best).
    pub fn of(th: &crate::theme::Theme, mono: iced::Font, size: f32) -> Look {
        let mut style = markdown::Style::from_palette(iced::Theme::Dark.palette());
        style.link_color = th.accent;
        style.inline_code_color = th.scope("string");
        style.inline_code_font = mono;
        style.code_block_font = mono;
        Look {
            comment: comment::view::Look { fg: th.foreground, dim: th.dim, accent: th.accent, divider: th.divider, size: size - 1.0 },
            markdown: markdown::Settings::with_text_size(size, style),
            mono,
            lit: Color { a: 0.35, ..th.selection },
        }
    }
}

/// The plan, row by row, with its comments under the lines they are on. `live`: it can be
/// picked from and commented on (otherwise the comments show as they were sent). `on`: wraps
/// what it publishes.
pub fn view<'a>(plan: &'a Plan, look: &Look, live: bool, on: impl Fn(PlanMsg) -> Message + Copy + 'a) -> Element<'a, Message> {
    let c = look.comment;
    let viewer = MarkdownViewer { font: look.mono, size: c.size + 1.0, color: c.fg, math: None, images: None };
    let draft = plan.comments.draft.as_ref().filter(|_| live);
    let sel = plan.sel.filter(|_| live).map(|(a, b)| (a.min(b), a.max(b)));
    let mut col = column![];
    let mut last_item = false;
    for (i, r) in plan.rows.iter().enumerate() {
        let in_draft = draft.is_some_and(|d| d.start_line <= r.start_line && r.end_line <= d.end_line);
        let picked = sel.is_some_and(|(a, z)| (a..=z).contains(&i));
        let commented = plan.comments.covers(r.start_line);
        // The gutter: a bar on the rows picked, a thinner one where comments are.
        let (mark, color) = match () {
            _ if picked || in_draft => ("▌", c.accent),
            _ if commented => ("▏", Color { a: 0.7, ..c.accent }),
            _ => ("", c.dim),
        };
        let gutter = container(text(mark).size(c.size + 3.0).color(color)).width(Length::Fixed(14.0));
        let body = container(markdown::view_with(r.md.items(), look.markdown, &viewer)).padding(Padding { left: r.depth.saturating_sub(1) as f32 * 22.0, ..Padding::ZERO }).width(Length::Fill);
        let tint = (picked || in_draft).then_some(look.lit);
        // Items of one list sit close together; anything else gets room above it.
        let item = r.depth > 0;
        let above = if i == 0 { 0.0 } else if item && last_item { 1.0 } else { c.size * 0.6 };
        last_item = item;
        let block = container(row![gutter, body].spacing(4))
            .padding([1, 0])
            .width(Length::Fill)
            .style(move |_| container::Style { background: tint.map(Into::into), border: Border { radius: 3.0.into(), ..Default::default() }, ..Default::default() });
        let block: Element<'a, Message> = if live { mouse_area(block).on_press(on(PlanMsg::Row(i))).into() } else { block.into() };
        let block = container(block).padding(Padding { top: above, ..Padding::ZERO });
        // Named while live, so the keys can scroll it into view (`reveal`).
        col = col.push(if live { block.id(row_id(i)) } else { block });
        // What is said about it, under its last line.
        let indent = |e: Element<'a, Message>| -> Element<'a, Message> { container(e).padding(Padding { left: 18.0 + r.depth.saturating_sub(1) as f32 * 22.0, top: 4.0, bottom: 4.0, right: 0.0 }).into() };
        for cm in plan.comments.ending_at(r.end_line) {
            let remove = live.then(|| on(PlanMsg::Remove(cm.id.clone())));
            col = col.push(indent(comment::view::thread(cm, c, None, remove)));
            last_item = false;
        }
        if let Some(d) = draft.filter(|d| d.end_line == r.end_line) {
            col = col.push(container(indent(comment::view::draft(d, c, on(PlanMsg::Save), on(PlanMsg::Cancel)))).id(DRAFT_ID));
            last_item = false;
        }
    }
    col.into()
}

/// The name of a live plan's row `i` on screen.
fn row_id(i: usize) -> WidgetId {
    WidgetId::from(format!("plan-row-{i}"))
}

/// The name of the comment being written on a live plan.
const DRAFT_ID: &str = "plan-draft";

/// Scroll `scroll` (the scrollable the live plan is drawn in; `bottom`: anchored to its bottom)
/// just far enough that the comment being written, or else the row the cursor is on, is in
/// view. Nothing moves if it is in view already.
pub fn reveal(plan: &Plan, scroll: &'static str, bottom: bool) -> Task<Message> {
    let target = match (&plan.comments.draft, plan.sel) {
        (Some(_), _) => WidgetId::from(DRAFT_ID),
        (None, Some((_, at))) => row_id(at),
        (None, None) => return Task::none(),
    };
    operate(Measure { scroll: WidgetId::from(scroll), target, bottom, view: None, found: None })
}

/// Room left above or below what is revealed.
const MARGIN: f32 = 24.0;

/// First pass of `reveal`: where the scrollable is scrolled to, and where the target is in it.
struct Measure {
    scroll: WidgetId,
    target: WidgetId,
    bottom: bool,
    /// The scrollable's viewport and content, and how far down it is scrolled.
    view: Option<(Rectangle, Rectangle, f32)>,
    found: Option<Rectangle>,
}

impl<T: 'static> Operation<T> for Measure {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&WidgetId>, bounds: Rectangle) {
        if id == Some(&self.target) {
            self.found = Some(bounds);
        }
    }

    fn scrollable(&mut self, id: Option<&WidgetId>, bounds: Rectangle, content: Rectangle, translation: Vector, _state: &mut dyn operation::Scrollable) {
        if id == Some(&self.scroll) {
            self.view = Some((bounds, content, translation.y));
        }
    }

    fn finish(&self) -> operation::Outcome<T> {
        let (Some((view, content, at)), Some(t)) = (self.view, self.found) else { return operation::Outcome::None };
        let Some(y) = scrolled_to(view.height, content.height, at, t.y - content.y, t.height) else { return operation::Outcome::None };
        // Anchored to the bottom, offsets count up from the end.
        let y = if self.bottom { (content.height - view.height).max(0.0) - y } else { y };
        operation::Outcome::Chain(Box::new(ScrollTo { scroll: self.scroll.clone(), y }))
    }
}

/// How far down (from the top) to scroll a viewport `view` high over content `content` high,
/// scrolled to `at` now, so that what spans `top..top + height` in it is in view with a margin:
/// its top first, when it is higher than the viewport. `None`: it is in view already.
fn scrolled_to(view: f32, content: f32, at: f32, top: f32, height: f32) -> Option<f32> {
    let (top, end) = (top - MARGIN, top + height + MARGIN);
    let y = if top < at {
        top
    } else if end > at + view {
        (end - view).min(top)
    } else {
        return None;
    };
    Some(y.clamp(0.0, (content - view).max(0.0)))
}

/// Second pass of `reveal`: scroll there.
struct ScrollTo {
    scroll: WidgetId,
    y: f32,
}

impl<T> Operation<T> for ScrollTo {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        operate(self);
    }

    fn scrollable(&mut self, id: Option<&WidgetId>, _bounds: Rectangle, _content: Rectangle, _translation: Vector, state: &mut dyn operation::Scrollable) {
        if id == Some(&self.scroll) {
            state.scroll_to(operation::scrollable::AbsoluteOffset { x: None, y: Some(self.y) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "# Cache the fetches\n\nWhy: it is slow.\nVery slow.\n\n1. Add a cache\n   with a size cap\n   - keyed by URL\n2. Use it in `fetch()`\n\n```rust\nlet x = 1;\n```\n";

    #[test]
    fn reveal_scrolls_only_as_far_as_the_row_needs() {
        // A 100-high view over 1000, at the top: a row in view stays put.
        assert_eq!(scrolled_to(100.0, 1000.0, 0.0, 30.0, 20.0), None);
        // One below the bottom: scrolled until it (and the margin) shows at the bottom.
        assert_eq!(scrolled_to(100.0, 1000.0, 0.0, 150.0, 20.0), Some(150.0 + 20.0 + MARGIN - 100.0));
        // One above the top: scrolled until it shows at the top.
        assert_eq!(scrolled_to(100.0, 1000.0, 500.0, 400.0, 20.0), Some(400.0 - MARGIN));
        // Taller than the view: its top shows.
        assert_eq!(scrolled_to(100.0, 1000.0, 0.0, 300.0, 400.0), Some(300.0 - MARGIN));
        // Never past either end.
        assert_eq!(scrolled_to(100.0, 1000.0, 500.0, 5.0, 20.0), Some(0.0));
        assert_eq!(scrolled_to(100.0, 1000.0, 0.0, 990.0, 10.0), Some(900.0));
    }

    #[test]
    fn rows_are_blocks_and_every_list_item() {
        let parts = split(PLAN);
        let lines: Vec<(u32, u32, usize)> = parts.iter().map(|p| (p.start_line, p.end_line, p.depth)).collect();
        assert_eq!(lines, [(1, 1, 0), (3, 4, 0), (6, 7, 1), (8, 8, 2), (9, 9, 1), (11, 13, 0)], "a nested item is a row of its own");
        assert_eq!(parts[2].source(PLAN), "1. Add a cache\n   with a size cap", "without its nested list");
        assert_eq!(parts[3].source(PLAN), "- keyed by URL", "a nested item loses the indent it has in the plan");
        let deep = "- a\n  - b\n    more of b\n";
        assert_eq!(split(deep)[1].source(deep), "- b\n  more of b", "its later lines keep only their indent past the marker");
        assert_eq!(Plan::new(PLAN).title(), "Cache the fetches");
    }

    #[test]
    fn arrows_pick_rows_and_enter_toggles_a_comment() {
        let mut p = Plan::new(PLAN);
        assert!(!p.enter(), "nothing picked: ↵ is not a comment");
        p.step(1, false);
        p.step(1, false);
        p.step(1, true);
        assert_eq!(p.sel, Some((1, 2)), "⇧↓ takes the next row in");
        assert!(p.enter());
        let d = p.comments.draft.as_mut().unwrap();
        assert_eq!((d.start_line, d.end_line), (3, 7));
        d.insert("why not memoize?");
        p.step(1, true);
        assert_eq!(p.comments.draft.as_ref().map(|d| d.end_line), Some(8), "⇧↓ while writing stretches the comment");
        p.save();
        assert_eq!(p.sel, None, "filed: ↵ is free to send the plan");
        p.step(1, false);
        assert_eq!(p.sel, Some((4, 4)), "↓ leaves the comment: it covers rows 1-3 now");
        let f = p.feedback("Looks fine otherwise.");
        assert!(f.contains("On lines 3–8:\n> Why: it is slow.\n> Very slow.\n>\n> 1. Add a cache"), "{f}");
        assert!(f.contains("why not memoize?") && f.contains("Looks fine otherwise.") && f.contains("ExitPlanMode"));
        // ↵ on a commented row opens that comment again; emptied, it is gone.
        p.step(-1, false);
        p.step(-1, false);
        p.enter();
        assert_eq!(p.comments.draft.as_ref().map(|d| d.body.as_str()), Some("why not memoize?"));
        p.comments.draft.as_mut().unwrap().body.clear();
        p.save();
        assert!(p.comments.is_empty());
        // A click picks; a second click on it opens a comment.
        p.update(PlanMsg::Row(4));
        assert_eq!(p.sel, Some((4, 4)));
        p.update(PlanMsg::Row(4));
        assert!(p.comments.draft.is_some());
    }

    #[test]
    fn comments_kept_elsewhere_are_taken_out_and_shown_on_whole_rows() {
        let mut p = Plan::new(PLAN);
        assert_eq!(p.take(), None);
        p.update(PlanMsg::Row(1));
        assert_eq!(p.picked(), Some((3, 4)));
        assert!(p.write());
        assert_eq!(p.take(), None, "no words: dropped");
        p.step(0, false);
        assert_eq!(p.sel, Some((1, 1)), "the row it was on is picked again");
        p.write();
        p.comments.draft.as_mut().unwrap().insert(" how slow? ");
        assert_eq!(p.take(), Some((3, 4, "Why: it is slow.\nVery slow.".to_string(), "how slow?".to_string())));
        assert!(p.comments.is_empty() && p.comments.draft.is_none(), "filed by whoever took it");
        // Line 3 alone, and a line between two rows: each shows under a whole row.
        let c = |id: &str, a: u32, b: u32| Comment { id: id.into(), start_line: a, end_line: b, anchor: String::new(), body: "x".into(), author: "user".into(), resolved: false, drifted: false, replies: vec![] };
        p.show(vec![c("c2", 10, 10), c("c1", 3, 3)]);
        let shown: Vec<(&str, u32, u32)> = p.comments.list().iter().map(|c| (c.id.as_str(), c.start_line, c.end_line)).collect();
        assert_eq!(shown, [("c1", 3, 4), ("c2", 9, 9)]);
        // On a commented row a new comment is written: the one there is not opened.
        p.update(PlanMsg::Row(1));
        p.write();
        assert_eq!((p.comments.len(), p.comments.draft.as_ref().map(|d| d.body.as_str())), (2, Some("")));
    }
}
