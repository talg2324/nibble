//! Find (Cmd+F): one box, two scopes, Tab switches between them.
//!
//! - Local: the file in the editor (every match tinted, the current one selected; Enter /
//!   Shift+Enter go to the next / previous), or the agents chat that has focus (the turn with the
//!   match outlined and scrolled to).
//! - Project: nibd searches the whole project (`Request::Search`: file names fuzzily, and lines
//!   containing the text; hidden folders, gitignored and binary files skipped), so it works the
//!   same over SSH. Enter opens the chosen file or line.
//!
//! Matching is smart case: case-insensitive unless the query has a capital.

use super::{async_sleep, App, Focus, Message, Pending, Tab};
use crate::control::Call;
use crate::editor_view::Mark;
use iced::widget::{button, column, container, rich_text, row, space, span, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;
use nib_proto::{Location, Request, Response, RpcError, SearchResults};
use serde_json::{json, Value};
use std::time::Duration;

/// Typing pauses this long before the project is searched.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Most lines the daemon sends for one query.
const MAX_LINES: u32 = 500;
/// File-name matches listed above the lines.
const FILES_SHOWN: usize = 8;
/// Matches looked for in one file.
const MAX_LOCAL: usize = 10_000;
/// Rows of the project results shown at once.
const ROWS: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// The file or chat with focus.
    #[default]
    Local,
    /// Every file in the project, on the daemon.
    Project,
}

/// What a local search looks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A tab, by `Tab::id`.
    Tab(u64),
    /// An agents chat, by its key.
    Chat(u64),
}

#[derive(Debug, Clone)]
pub enum Msg {
    /// Typing paused: search the project for query number `seq`, if it is still the latest.
    Due(u64),
    /// A project result was clicked.
    Pick(usize),
    /// A scope's label was clicked.
    Scope(Scope),
    Pasted(Option<String>),
}

/// One local match: a char range in the tab, or a byte range in a chat turn's text.
#[derive(Debug, Clone, Copy)]
struct Hit {
    turn: usize,
    from: usize,
    to: usize,
}

#[derive(Default)]
pub struct State {
    /// The box is showing: it has the keyboard.
    pub open: bool,
    scope: Scope,
    query: String,
    target: Option<Target>,
    hits: Vec<Hit>,
    /// The tab's `Document::version` the hits were found in.
    version: u64,
    current: usize,
    /// Where the cursor was when the box opened: the first match is the one from there on.
    origin: usize,
    /// The project's answer for the current query (`None`: none yet).
    results: Option<SearchResults>,
    /// In the project list (file names first, then lines).
    selected: usize,
    /// Bumped on every query change; older answers are dropped.
    seq: u64,
    /// Asked, not answered yet.
    searching: bool,
    /// A control request waiting for the project's answer.
    call: Option<Call>,
}

/// A row of the project list.
enum Item<'a> {
    File(&'a str),
    Line(&'a Location),
}

impl State {
    fn items(&self) -> Vec<Item<'_>> {
        let Some(r) = &self.results else { return Vec::new() };
        let files = r.files.iter().take(FILES_SHOWN).map(|f| Item::File(f));
        files.chain(r.lines.iter().map(Item::Line)).collect()
    }
}

/// Byte ranges of `needle` in `hay`, smart case (ASCII folding keeps byte offsets).
fn find_all(hay: &str, needle: &str, max: usize) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let exact = needle.chars().any(char::is_uppercase);
    let (hay, needle) = if exact { (hay.to_string(), needle.to_string()) } else { (hay.to_ascii_lowercase(), needle.to_ascii_lowercase()) };
    hay.match_indices(&needle).take(max).map(|(i, m)| (i, i + m.len())).collect()
}

impl App {
    /// Cmd+F: open the box on what has focus (a file, a chat), or on the project when nothing
    /// there can be searched.
    pub(super) fn search_start(&mut self) -> Task<Message> {
        if self.search.open {
            return Task::none();
        }
        self.code.picker = None;
        self.code.rename = None;
        let target = self.search_target();
        let s = &mut self.search;
        s.open = true;
        s.target = target;
        s.scope = if target.is_some() { Scope::Local } else { Scope::Project };
        s.origin = 0;
        // A selection on one line is what to look for.
        if let (Some(Target::Tab(_)), Some(t)) = (target, self.tabs.get(self.active)) {
            let r = t.doc.selection.primary();
            let picked = t.doc.text.slice(r.from()..r.to()).to_string();
            if !picked.is_empty() && !picked.contains('\n') {
                self.search.query = picked;
            }
            self.search.origin = r.from();
        }
        self.search_changed()
    }

    fn search_target(&self) -> Option<Target> {
        if self.focus == Focus::Ai && self.ai.shown() {
            return Some(Target::Chat(self.ai_transcript().0));
        }
        let t = self.tab().filter(|t| self.focus == Focus::Main && t.media.is_none())?;
        Some(Target::Tab(t.id))
    }

    fn search_close(&mut self) {
        let s = &mut self.search;
        s.open = false;
        s.hits.clear();
        s.searching = false;
        if let Some(c) = s.call.take() {
            c.err("search closed");
        }
    }

    /// The query or the scope changed.
    fn search_changed(&mut self) -> Task<Message> {
        self.search.seq += 1;
        self.search.selected = 0;
        match self.search.scope {
            Scope::Local => {
                self.search_local();
                self.search_reveal()
            }
            Scope::Project => {
                self.search.results = None;
                if self.search.query.trim().is_empty() {
                    self.search.searching = false;
                    return Task::none();
                }
                self.search.searching = true;
                let seq = self.search.seq;
                Task::perform(async_sleep(DEBOUNCE), move |_| Message::Search(Msg::Due(seq)))
            }
        }
    }

    /// Find the query in the local target, and pick the first match from where the box opened.
    fn search_local(&mut self) {
        let q = self.search.query.clone();
        let mut hits = Vec::new();
        match self.search.target {
            Some(Target::Tab(id)) => {
                if let Some(t) = self.tabs.iter().find(|t| t.id == id) {
                    let text = t.doc.text.to_string();
                    let c = |b| t.doc.text.byte_to_char(b);
                    hits = find_all(&text, &q, MAX_LOCAL).into_iter().map(|(a, b)| Hit { turn: 0, from: c(a), to: c(b) }).collect();
                    self.search.version = t.doc.version();
                }
            }
            Some(Target::Chat(key)) => {
                let (k, turns) = self.ai_transcript();
                if k == key {
                    for (turn, s) in turns.iter().enumerate() {
                        let left = MAX_LOCAL.saturating_sub(hits.len());
                        hits.extend(find_all(s, &q, left).into_iter().map(|(from, to)| Hit { turn, from, to }));
                    }
                }
            }
            None => {}
        }
        let origin = self.search.origin;
        self.search.current = match self.search.target {
            Some(Target::Tab(_)) => hits.iter().position(|h| h.from >= origin).unwrap_or(0),
            // A chat is read from the end: the latest match first.
            _ => hits.len().saturating_sub(1),
        };
        self.search.hits = hits;
    }

    /// Show the current local match: selected in the editor, or scrolled to in the chat.
    fn search_reveal(&mut self) -> Task<Message> {
        let Some(h) = self.search.hits.get(self.search.current).copied() else { return Task::none() };
        match self.search.target {
            Some(Target::Tab(id)) => {
                let Some(i) = self.tabs.iter().position(|t| t.id == id) else { return Task::none() };
                self.active = i;
                let doc = &mut self.tabs[i].doc;
                doc.set_cursor(h.from, false);
                doc.set_cursor(h.to, true);
                self.follow_cursor();
                Task::none()
            }
            Some(Target::Chat(_)) => self.ai_scroll_to_turn(h.turn),
            None => Task::none(),
        }
    }

    /// Next (`step` 1) or previous (-1) local match, around the ends.
    fn search_step(&mut self, step: isize) -> Task<Message> {
        let n = self.search.hits.len();
        if n == 0 {
            return Task::none();
        }
        self.search.current = (self.search.current as isize + step).rem_euclid(n as isize) as usize;
        self.search_reveal()
    }

    fn search_toggle_scope(&mut self) -> Task<Message> {
        let to = match self.search.scope {
            Scope::Local => Scope::Project,
            Scope::Project => Scope::Local,
        };
        self.search_set_scope(to)
    }

    fn search_set_scope(&mut self, to: Scope) -> Task<Message> {
        if self.search.scope == to {
            return Task::none();
        }
        self.search.scope = to;
        self.search_changed()
    }

    /// Ask the daemon about the current query now.
    fn search_project_now(&mut self) -> Task<Message> {
        let seq = self.search.seq;
        self.search.searching = true;
        let query = self.search.query.clone();
        self.request(Request::Search { query, max: MAX_LINES }, Pending::Search { seq })
    }

    pub(super) fn on_search_results(&mut self, seq: u64, result: Result<Response, RpcError>) -> Task<Message> {
        if seq != self.search.seq || !self.search.open || self.search.scope != Scope::Project {
            return Task::none();
        }
        self.search.searching = false;
        match result {
            Ok(Response::Search(r)) => {
                self.search.results = Some(r);
                self.search.selected = 0;
                if let Some(c) = self.search.call.take() {
                    c.ok(self.search_json());
                }
                Task::none()
            }
            Ok(other) => self.flash_warn(format!("unexpected answer: {other:?}")),
            Err(e) => {
                if let Some(c) = self.search.call.take() {
                    c.err(&e.msg);
                }
                self.flash_warn(format!("search: {}", e.msg))
            }
        }
    }

    /// Open project result `i`: a file, or a line in one.
    fn search_open(&mut self, i: usize) -> Task<Message> {
        let root = self.root.trim_end_matches('/').to_string();
        let task = match self.search.items().get(i) {
            Some(Item::File(f)) => {
                let path = format!("{root}/{f}");
                self.open_path(path, true)
            }
            Some(Item::Line(l)) => {
                let l = (*l).clone();
                self.jump(&l)
            }
            None => return Task::none(),
        };
        self.search_close();
        self.ai.front = false;
        self.focus = Focus::Main;
        task
    }

    pub(super) fn on_search_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Due(seq) if seq == self.search.seq && self.search.open && self.search.scope == Scope::Project => {
                self.search_project_now()
            }
            Msg::Due(_) => Task::none(),
            Msg::Pick(i) => self.search_open(i),
            Msg::Scope(s) => self.search_set_scope(s),
            Msg::Pasted(Some(t)) if self.search.open => {
                // One line: the box doesn't hold more.
                let line = t.lines().next().unwrap_or("").to_string();
                self.search.query.push_str(&line);
                self.search_changed()
            }
            Msg::Pasted(_) => Task::none(),
        }
    }

    /// Keys while the box is open: it has them all, except shortcuts, which close it and run.
    pub(super) fn search_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let cmd = chord.ctrl || chord.logo;
        let project = self.search.scope == Scope::Project;
        let last = self.search.items().len().saturating_sub(1);
        match chord.key {
            Key::Esc => self.search_close(),
            Key::Tab if !cmd => return self.search_toggle_scope(),
            Key::Enter if project => return self.search_open(self.search.selected),
            Key::Enter => return self.search_step(if chord.shift { -1 } else { 1 }),
            Key::Up if !project => return self.search_step(-1),
            Key::Down if !project => return self.search_step(1),
            Key::Up => self.search.selected = self.search.selected.saturating_sub(1),
            Key::Down => self.search.selected = (self.search.selected + 1).min(last),
            Key::PageUp if project => self.search.selected = self.search.selected.saturating_sub(ROWS),
            Key::PageDown if project => self.search.selected = (self.search.selected + ROWS).min(last),
            Key::Backspace => {
                if chord.alt || cmd {
                    self.search.query.clear();
                } else {
                    self.search.query.pop();
                }
                return self.search_changed();
            }
            _ if cmd => match self.keymap.lookup(&chord) {
                Some(Command::Paste) => return iced::clipboard::read().map(|s| Message::Search(Msg::Pasted(s))),
                Some(Command::SelectAll) => {}
                Some(Command::Find) => return self.search_toggle_scope(),
                Some(c) => {
                    self.search_close();
                    return self.exec(c);
                }
                None => {}
            },
            _ => {
                let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                if !typed.is_empty() {
                    self.search.query.push_str(&typed);
                    return self.search_changed();
                }
            }
        }
        Task::none()
    }

    /// The matches to tint in `tab` (the current one is the selection).
    pub(super) fn search_marks(&self, tab: &Tab) -> Vec<Mark> {
        let s = &self.search;
        if !s.open || s.scope != Scope::Local || s.target != Some(Target::Tab(tab.id)) || s.version != tab.doc.version() {
            return Vec::new();
        }
        let color = Color { a: 0.28, ..self.theme.warn };
        s.hits.iter().map(|h| Mark { from: h.from, to: h.to, color, rows: false }).collect()
    }

    /// Turn `turn` of chat `chat`, outlined when the current match is in it.
    pub(super) fn search_outline<'a>(&self, chat: u64, turn: usize, el: Element<'a, Message>) -> Element<'a, Message> {
        let s = &self.search;
        let here = s.open && s.scope == Scope::Local && s.target == Some(Target::Chat(chat)) && s.hits.get(s.current).is_some_and(|h| h.turn == turn);
        if !here {
            return el;
        }
        let warn = self.theme.warn;
        container(el)
            .padding(2)
            .style(move |_| container::Style { border: Border { color: warn, width: 1.0, radius: 4.0.into() }, ..Default::default() })
            .into()
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    pub(super) fn search_view(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let s = &self.search;
        let (accent, dim, fg, divider) = (th.accent, th.dim, th.foreground, th.divider);
        let local_name = match s.target {
            Some(Target::Tab(id)) => {
                let name = self.tabs.iter().find(|t| t.id == id).and_then(|t| t.doc.path.as_ref()).and_then(|p| p.file_name());
                name.map_or("this file".to_string(), |n| n.to_string_lossy().into_owned())
            }
            Some(Target::Chat(_)) => "this chat".to_string(),
            None => "nothing here".to_string(),
        };
        let scope = |label: String, sc: Scope| {
            let on = s.scope == sc;
            button(text(label).size(11).color(if on { fg } else { dim })).on_press(Message::Search(Msg::Scope(sc))).padding([1, 6]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: if on { accent } else { divider }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            })
        };
        let count = match s.scope {
            _ if s.query.is_empty() => String::new(),
            Scope::Local if s.target.is_none() => "open a file or a chat to search it".into(),
            Scope::Local if s.hits.is_empty() => "no matches".into(),
            Scope::Local => format!("{} of {}", s.current + 1, s.hits.len()),
            Scope::Project if s.searching && s.results.is_none() => "searching…".into(),
            Scope::Project => match &s.results {
                Some(r) => {
                    let more = if r.truncated { "+" } else { "" };
                    format!("{} files · {}{more} lines", r.files.len(), r.lines.len())
                }
                None => String::new(),
            },
        };
        let header = row![text("Find").size(11).color(dim), scope(local_name, Scope::Local), scope("project".into(), Scope::Project), space::horizontal(), text(count).size(11).color(dim)]
            .spacing(8)
            .align_y(Alignment::Center)
            .padding([2, 10]);

        // The caret is a glyph inside the text, as in the rename box.
        let typed: [iced::widget::text::Span<'_, (), Font>; 2] = [span(s.query.as_str()).color(fg), span("▏").color(accent)];
        let field = container(rich_text(typed).font(self.face.font).size(self.face.size).width(Length::Fill))
            .padding([5, 8])
            .width(Length::Fill)
            .style(move |_| container::Style { border: Border { color: accent, width: 1.0, radius: 4.0.into() }, ..Default::default() });
        let field = container(field).padding([0, 10]).width(Length::Fixed(720.0));

        let mut body = column![header, field].spacing(6);
        if s.scope == Scope::Project {
            body = body.push(self.search_list());
            body = body.push(row![self.hint("↑↓", "choose"), self.hint("↵", "open"), self.hint("tab", "this file / chat"), self.hint("esc", "close")].spacing(18).padding([4, 10]));
        } else {
            body = body.push(row![self.hint("↵", "next"), self.hint("⇧↵", "previous"), self.hint("tab", "whole project"), self.hint("esc", "close")].spacing(18).padding([4, 10]));
        }
        body.into()
    }

    fn search_list(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let s = &self.search;
        let items = s.items();
        let n = items.len();
        let start = s.selected.saturating_sub(ROWS / 2).min(n.saturating_sub(ROWS));
        let root = self.root.trim_end_matches('/');
        let mut rows = column![];
        for (i, item) in items.iter().enumerate().skip(start).take(ROWS) {
            let line = match item {
                Item::File(f) => row![
                    text(f.to_string()).size(self.face.size.min(13.0)).font(self.face.font).color(th.foreground),
                    space::horizontal(),
                    text("file").size(11).color(th.dim),
                ],
                Item::Line(l) => {
                    let rel = l.path.strip_prefix(root).map_or(l.path.as_str(), |r| r.trim_start_matches('/'));
                    let code: String = l.line_text.trim().chars().take(140).collect();
                    row![
                        text(format!("{rel}:{}", l.start.line + 1)).size(12).font(Font::MONOSPACE).color(th.dim),
                        text(code).size(self.face.size.min(13.0)).font(self.face.font).color(th.foreground),
                    ]
                }
            };
            let bg = if i == s.selected { th.selection } else { Color::TRANSPARENT };
            rows = rows.push(
                button(line.spacing(14).align_y(Alignment::Center))
                    .on_press(Message::Search(Msg::Pick(i)))
                    .width(Length::Fill)
                    .padding([3, 10])
                    .style(move |_, _| button::Style { background: Some(bg.into()), ..Default::default() }),
            );
        }
        rows.into()
    }

    // -----------------------------------------------------------------------------------------
    // control socket
    // -----------------------------------------------------------------------------------------

    /// What the box shows, for `state` and the `search` request.
    pub(super) fn search_json(&self) -> Value {
        let s = &self.search;
        let scope = match s.scope {
            Scope::Local => "local",
            Scope::Project => "project",
        };
        let target = match s.target {
            Some(Target::Tab(id)) => json!({ "tab": self.tabs.iter().find(|t| t.id == id).map(|t| &t.doc.path) }),
            Some(Target::Chat(key)) => json!({ "chat": key }),
            None => Value::Null,
        };
        let root = self.root.trim_end_matches('/');
        let results = s.results.as_ref().map(|r| {
            let lines: Vec<Value> = r
                .lines
                .iter()
                .map(|l| {
                    let rel = l.path.strip_prefix(root).map_or(l.path.as_str(), |r| r.trim_start_matches('/'));
                    json!({ "path": rel, "line": l.start.line + 1, "col": l.start.col + 1, "text": l.line_text })
                })
                .collect();
            json!({ "files": r.files, "lines": lines, "truncated": r.truncated })
        });
        json!({
            "open": s.open,
            "scope": scope,
            "query": s.query,
            "target": target,
            "matches": s.hits.len(),
            "current": (!s.hits.is_empty()).then_some(s.current + 1),
            "searching": s.searching,
            "selected": s.selected,
            "results": results,
        })
    }

    /// `search`: open the box with a query (`scope` local / project), or step through it
    /// (`do`: next, prev, open with `index`, close). A project search answers once the daemon has.
    pub(super) fn on_search_ctl(&mut self, call: Call) -> Task<Message> {
        let task = match call.str_arg("do") {
            Some("next") => self.search_step(1),
            Some("prev") => self.search_step(-1),
            Some("close") => {
                self.search_close();
                Task::none()
            }
            Some("open") => {
                let i = call.req.get("index").and_then(Value::as_u64).map_or(self.search.selected, |i| i as usize);
                if i >= self.search.items().len() {
                    call.err("no such result (state.search.results: file names first, then lines)");
                    return Task::none();
                }
                self.search_open(i)
            }
            Some(other) => {
                call.err(format!("search: unknown do {other:?} (next, prev, open, close)"));
                return Task::none();
            }
            None => {
                let start = self.search_start();
                let want = match call.str_arg("scope") {
                    Some("project" | "repo") => Scope::Project,
                    _ if self.search.target.is_none() => Scope::Project,
                    _ => Scope::Local,
                };
                self.search.scope = want;
                if let Some(q) = call.str_arg("query") {
                    self.search.query = q.to_string();
                }
                let changed = self.search_changed();
                if want == Scope::Project && !self.search.query.trim().is_empty() {
                    if let Some(old) = self.search.call.replace(call) {
                        old.err("replaced by a newer search");
                    }
                    // No pause: the query is whole.
                    return Task::batch([start, self.search_project_now()]);
                }
                Task::batch([start, changed])
            }
        };
        call.ok(self.search_json());
        task
    }
}
