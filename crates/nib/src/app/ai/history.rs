//! The history tab (D-049): this project's past conversations with agents.
//!
//! The daemon keeps them (it writes each one down as it happens, where the assistant's own
//! session lives). Here they are listed, the latest first; typing searches what was asked, said,
//! run and changed. A conversation can be reopened as an agent and continued (the assistant
//! resumes its own session), replayed in Follow (its steps walk the editor through the files as
//! they are now), or deleted, on the second click.
//!
//! Tab switches it to the mailbox: every message the project's agents sent each other, the latest
//! first, searched the same way.

use super::{end_steps, step_copy, Chat, Msg, Step, StepEnd, Turn, Who};
use crate::app::{async_sleep, App, Focus, Message, Pending};
use crate::{links, math};
use iced::widget::text::Span;
use iced::widget::{button, column, container, markdown, rich_text, row, scrollable, space, span, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Padding, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_proto::{AiAgent, AiLetter, AiPast, AiPastEntry, Request, Response, RpcError};
use serde_json::{json, Value};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BAD: Color = Color::from_rgb8(0xe0, 0x6c, 0x75);
/// How long a replay rests between steps, once the last one has been shown.
const REPLAY_STEP: Duration = Duration::from_millis(300);
/// How often a replay looks whether the step is still being shown.
const REPLAY_WAIT: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(super) struct History {
    /// The history tab exists.
    pub open: bool,
    /// It, not the agents, is what the agents area shows.
    pub front: bool,
    query: String,
    items: Vec<AiPast>,
    sel: usize,
    /// The conversation whose delete was pressed once: the next press deletes it.
    confirm: Option<String>,
    /// The list was asked for and hasn't come.
    loading: bool,
    /// It shows the mailbox's messages, not the conversations.
    mail: bool,
    letters: Vec<AiLetter>,
}

impl History {
    pub(super) fn pick(&mut self, i: usize) {
        if i < self.items.len() {
            self.sel = i;
            self.confirm = None;
        }
    }
}

/// "3 min ago", "5 h ago", "2 days ago".
fn ago(unix: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match (now - unix).max(0) {
        s if s < 90 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s if s < 2 * 86_400 => "yesterday".into(),
        s => format!("{} days ago", s / 86_400),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// What the agent said, as a turn (parsed like a streamed reply is).
fn said(text: &str) -> Turn {
    let (md, formulas) = if math::maybe(text) {
        let p = math::prepare(text);
        (markdown::Content::parse(&links::autolink(&p.md)), p.formulas)
    } else {
        (markdown::Content::parse(&links::autolink(text)), Default::default())
    };
    Turn { who: Who::Assistant, text: text.to_string(), md, math: formulas, step: None, images: Default::default() }
}

fn plain(who: Who, text: String) -> Turn {
    Turn { who, text, md: markdown::Content::new(), math: Default::default(), step: None, images: Default::default() }
}

/// A past conversation as a chat: everything that happened in it, nothing running.
fn revive(key: u64, past: AiPast, entries: Vec<AiPastEntry>, root: &str) -> Chat {
    let mut turns: Vec<Turn> = Vec::new();
    for e in entries {
        match e {
            AiPastEntry::User(t) => turns.push(plain(Who::User, t)),
            AiPastEntry::Text(t) => turns.push(said(&t)),
            AiPastEntry::Note(t) => turns.push(plain(Who::Note, t)),
            AiPastEntry::Mail(m) => turns.push(plain(Who::Mail, if m.incoming { format!("from {}: {}", m.from, m.text) } else { format!("to {}: {}", m.to, m.text) })),
            AiPastEntry::Step(s) => turns.push(Turn { step: Some(Step { id: s.id, parent: s.parent, acts: super::act::of(&s.kind, root), kind: s.kind, started: Instant::now(), done: None }), ..plain(Who::Step, String::new()) }),
            AiPastEntry::StepDone { done, seconds } => {
                if let Some(step) = turns.iter_mut().rev().filter_map(|t| t.step.as_mut()).find(|s| s.id == done.id) {
                    step.done = Some(StepEnd { ok: done.ok, output: done.output, line: done.line, took: Duration::from_secs_f32(seconds.max(0.0)) });
                }
            }
        }
    }
    let mut chat = Chat { key, agent: Some(past.agent), chosen: past.model, name: past.name, past: Some(past.id), turns, ..Default::default() };
    // A step it never finished (the conversation was cut off) is over.
    end_steps(&mut chat);
    chat
}

impl App {
    pub(in crate::app) fn ai_history_front(&self) -> bool {
        self.ai.history.open && self.ai.history.front
    }

    /// Ask the daemon for the list again (the search changed, or something was deleted).
    fn ai_history_refresh(&mut self) -> Task<Message> {
        let query = self.ai.history.query.clone();
        let t = if self.ai.history.mail {
            self.request(Request::AiMailHistory { query }, Pending::AiMailHistory)
        } else {
            self.request(Request::AiHistory { query }, Pending::AiHistory)
        };
        self.ai.history.loading = self.pending.values().any(|p| matches!(p, Pending::AiHistory | Pending::AiMailHistory));
        t
    }

    /// Show the mailbox's messages (`on`) or the conversations; the search stays.
    pub(super) fn ai_history_show_mail(&mut self, on: bool) -> Task<Message> {
        let h = &mut self.ai.history;
        if h.mail == on {
            return Task::none();
        }
        h.mail = on;
        h.confirm = None;
        self.ai_history_refresh()
    }

    pub(in crate::app) fn on_ai_mail_history(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        if self.pending.values().any(|p| matches!(p, Pending::AiMailHistory)) {
            return Task::none();
        }
        let h = &mut self.ai.history;
        h.loading = false;
        match result {
            Ok(Response::AiMailHistory(letters)) => {
                h.letters = letters;
                Task::none()
            }
            Ok(_) => Task::none(),
            Err(e) => self.flash_warn(format!("mailbox history: {}", e.msg)),
        }
    }

    /// Show the history tab, in front.
    pub(in crate::app) fn ai_history_open(&mut self) -> Task<Message> {
        self.ai.open = true;
        self.ai.front = true;
        self.ai.history.open = true;
        self.ai.history.front = true;
        self.ai.history.confirm = None;
        self.ai.picking = false;
        self.focus = Focus::Ai;
        self.ai_history_refresh()
    }

    pub(super) fn ai_history_close(&mut self) {
        self.ai.history.open = false;
        self.ai.history.front = false;
    }

    pub(in crate::app) fn on_ai_history(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        // An older answer while a newer question is out: wait for that one.
        if self.pending.values().any(|p| matches!(p, Pending::AiHistory)) {
            return Task::none();
        }
        let h = &mut self.ai.history;
        h.loading = self.pending.values().any(|p| matches!(p, Pending::AiMailHistory));
        match result {
            Ok(Response::AiHistory(items)) => {
                h.items = items;
                h.sel = h.sel.min(h.items.len().saturating_sub(1));
                Task::none()
            }
            Ok(_) => Task::none(),
            Err(e) => self.flash_warn(format!("history: {}", e.msg)),
        }
    }

    /// Reopen conversation `i` of the list as an agent (and replay it, if asked).
    pub(super) fn ai_history_reopen(&mut self, i: usize, replay: bool) -> Task<Message> {
        let Some(id) = self.ai.history.items.get(i).map(|p| p.id.clone()) else { return Task::none() };
        // Already open: go to it.
        if let Some(at) = self.ai.chats.iter().position(|c| c.past.as_deref() == Some(id.as_str())) {
            self.ai.history.front = false;
            self.ai_select(at);
            return if replay { self.ai_replay_start() } else { Task::none() };
        }
        self.request(Request::AiHistoryLoad { id }, Pending::AiHistoryLoad { replay })
    }

    pub(in crate::app) fn on_ai_past(&mut self, replay: bool, result: Result<Response, RpcError>) -> Task<Message> {
        let (past, entries) = match result {
            Ok(Response::AiPastLoaded { past, entries }) => (past, entries),
            Ok(_) => return Task::none(),
            Err(e) => return Task::batch([self.flash_warn(format!("history: {}", e.msg)), self.ai_history_refresh()]),
        };
        let key = self.ai.next_key;
        self.ai.next_key += 1;
        let agent = past.agent;
        let chat = self.ai.seeded(revive(key, past, entries, &self.root));
        // A blank agent makes way for it.
        match self.ai.chats.iter().position(Chat::blank) {
            Some(i) => self.ai.chats[i] = chat,
            None => self.ai.chats.push(chat),
        }
        let at = self.ai.chats.iter().position(|c| c.key == key).unwrap_or(0);
        self.ai.history.front = false;
        self.ai.front = true;
        self.ai_select(at);
        // What the `/` menu offers it, as a fresh chat with this assistant would ask.
        let skills = self.request(Request::AiSkills { agent }, Pending::AiSkills { agent });
        let replay = if replay { self.ai_replay_start() } else { Task::none() };
        Task::batch([skills, replay, self.ai_scroll_to_end()])
    }

    /// Delete conversation `i`: the first press asks, the second deletes.
    pub(super) fn ai_history_delete(&mut self, i: usize) -> Task<Message> {
        let Some(id) = self.ai.history.items.get(i).map(|p| p.id.clone()) else { return Task::none() };
        self.ai.history.sel = i;
        if self.ai.history.confirm.as_deref() != Some(id.as_str()) {
            self.ai.history.confirm = Some(id);
            return Task::none();
        }
        self.ai.history.confirm = None;
        self.ai.history.items.remove(i);
        self.ai.history.sel = self.ai.history.sel.min(self.ai.history.items.len().saturating_sub(1));
        // An agent reopened from it starts afresh from now on.
        for c in self.ai.chats.iter_mut().filter(|c| c.past.as_deref() == Some(id.as_str())) {
            c.past = None;
        }
        self.request(Request::AiHistoryDelete { id }, Pending::AiHistoryDelete)
    }

    pub(in crate::app) fn on_ai_history_deleted(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        match result {
            Ok(_) => self.ai_history_refresh(),
            Err(e) => Task::batch([self.flash_warn(e.msg), self.ai_history_refresh()]),
        }
    }

    /// The selected conversation's delete (the kill shortcut, in this tab).
    pub(super) fn ai_history_delete_selected(&mut self) -> Task<Message> {
        if self.ai.history.mail {
            return Task::none();
        }
        self.ai_history_delete(self.ai.history.sel)
    }

    /// Keys in the history tab: typing searches, arrows pick, Enter reopens.
    pub(super) fn ai_history_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let n = self.ai.history.items.len();
        let h = &mut self.ai.history;
        match chord.key {
            Key::Esc if h.confirm.is_some() => h.confirm = None,
            // Back to the agents.
            Key::Esc => h.front = false,
            Key::Tab if !held => {
                let on = !h.mail;
                return self.ai_history_show_mail(on);
            }
            // The mailbox's messages are only read.
            Key::Up | Key::Down | Key::Enter if h.mail => {}
            Key::Up => {
                h.sel = h.sel.saturating_sub(1);
                h.confirm = None;
            }
            Key::Down => {
                h.sel = (h.sel + 1).min(n.saturating_sub(1));
                h.confirm = None;
            }
            Key::Enter => {
                let i = h.sel;
                return self.ai_history_reopen(i, false);
            }
            Key::Backspace => {
                if held || chord.alt {
                    h.query.clear();
                } else {
                    h.query.pop();
                }
                h.sel = 0;
                return self.ai_history_refresh();
            }
            _ => match self.keymap.lookup(&chord) {
                Some(c) if crate::app::app_wide(c) => return self.exec(c),
                _ if !held => {
                    let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                    if !typed.is_empty() {
                        h.query.push_str(&typed);
                        h.sel = 0;
                        return self.ai_history_refresh();
                    }
                }
                _ => {}
            },
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // replay
    // -----------------------------------------------------------------------------------------

    /// Walk the editor through the shown conversation's steps, one after the other, as Follow
    /// would have shown them (the files as they are now).
    pub(super) fn ai_replay_start(&mut self) -> Task<Message> {
        let key = self.ai.key;
        if self.ai_following() != Some(key) {
            let _ = self.ai_follow_toggle();
        }
        self.ai.replay = Some((key, 0));
        Task::perform(async_sleep(Duration::from_millis(300)), |_| Message::Ai(Msg::ReplayTick))
    }

    pub(super) fn ai_replay_tick(&mut self) -> Task<Message> {
        let Some((key, from)) = self.ai.replay else { return Task::none() };
        let chat = self.ai.chats.iter().position(|c| c.key == key);
        // Stopped following (the user typed, or pressed the button): the replay is over.
        let (Some(i), true) = (chat, self.ai_following() == Some(key)) else {
            self.ai.replay = None;
            return Task::none();
        };
        // The last step is still being shown.
        if self.ai_follow_playing() {
            return Task::perform(async_sleep(REPLAY_WAIT), |_| Message::Ai(Msg::ReplayTick));
        }
        let next = self.ai.chats[i].turns.iter().enumerate().skip(from).find_map(|(n, t)| t.step.as_ref().map(|s| (n, step_copy(s))));
        let Some((n, step)) = next else {
            self.ai.replay = None;
            let _ = self.ai_as(i, |a| {
                a.ai_follow_turn_done();
                Task::none()
            });
            let shown = self.ai_follow_apply();
            return Task::batch([shown, self.ai_follow_stop(true)]);
        };
        self.ai.replay = Some((key, n + 1));
        let _ = self.ai_as(i, |a| {
            a.ai_follow_step(&step);
            a.ai_follow_done(&step);
            Task::none()
        });
        let shown = self.ai_follow_apply();
        Task::batch([shown, Task::perform(async_sleep(REPLAY_STEP), |_| Message::Ai(Msg::ReplayTick))])
    }

    // -----------------------------------------------------------------------------------------
    // state, view
    // -----------------------------------------------------------------------------------------

    pub(super) fn ai_history_json(&self) -> Value {
        let h = &self.ai.history;
        json!({
            "open": h.open,
            "front": self.ai_history_front(),
            "query": h.query,
            "selected": h.sel,
            "loading": h.loading,
            "confirm_delete": h.confirm,
            "view": if h.mail { "mailbox" } else { "conversations" },
            "letters": h.letters.iter().map(|l| json!({ "from": l.from, "to": l.to, "text": l.text, "at": l.at })).collect::<Vec<_>>(),
            "replaying": self.ai.replay.is_some(),
            "items": h.items.iter().map(|p| json!({
                "id": p.id, "title": p.title, "name": p.name, "agent": p.agent.name(), "model": p.model,
                "messages": p.messages, "steps": p.steps, "files": p.files, "hit": p.hit, "updated": p.updated,
            })).collect::<Vec<_>>(),
        })
    }

    /// The history tab: the search field and the list on the left, the selected conversation on
    /// the right.
    pub(in crate::app) fn ai_history_view(&self) -> Element<'_, Message> {
        let h = &self.ai.history;
        let th = &self.theme;
        let (fg, dim, accent, cur_bg, divider, mono) = (th.foreground, th.dim, th.accent, th.current_line, th.divider, self.face.font);
        let none = text::Wrapping::None;
        let size = self.face.size.min(14.0);

        // The search field always has the keys.
        let caret: Span<'_, (), Font> = span("▏").color(accent);
        let field: Element<'_, Message> = if h.query.is_empty() {
            let what = if h.mail { "Search the mailbox: who sent, to whom, what" } else { "Search conversations: what was asked, said, run or changed" };
            rich_text([caret, span(what).color(dim)]).size(size).width(Length::Fill).into()
        } else {
            rich_text([span(h.query.as_str()).color(fg), caret]).size(size).width(Length::Fill).into()
        };
        let search = container(field).padding([7, 10]).width(Length::Fill).style(move |_| container::Style { border: Border { color: accent, width: 1.0, radius: 4.0.into() }, ..Default::default() });

        // Conversations or the mailbox: the one showing lit.
        let tab = |label: &'static str, mail: bool| {
            let on = h.mail == mail;
            button(text(label).size(11).color(if on { fg } else { dim })).on_press(Message::Ai(Msg::HistoryMail(mail))).padding([2, 8]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: if on { accent } else { divider }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            })
        };
        let tabs = row![tab("conversations", false), tab("mailbox", true)].spacing(6);
        let top = container(column![tabs, search].spacing(8)).padding(Padding { left: 12.0, right: 12.0, top: 10.0, bottom: 8.0 });
        if h.mail {
            return self.ai_history_mail_view(top);
        }

        let key = |label: String, color: Color, msg: Msg| {
            button(text(label).size(11).color(color)).on_press(Message::Ai(msg)).padding([1, 7]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: if color == BAD { BAD } else { divider }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            })
        };
        let mut list = column![];
        for (i, p) in h.items.iter().enumerate() {
            let on = i == h.sel;
            let mut meta = vec![p.agent.name().to_string()];
            meta.extend(p.model.clone());
            meta.push(plural(p.steps as usize, "step"));
            meta.push(plural(p.files.len(), "file") + " changed");
            let mut body = column![
                // The date keeps its place: the title gives way.
                row![container(text(clip(&p.title, 90)).size(12).color(fg).wrapping(none)).width(Length::Fill).clip(true), text(ago(p.updated.max(p.started))).size(11).color(dim).wrapping(none)].spacing(10),
                row![text(p.name.clone().unwrap_or_default()).size(11).font(mono).color(accent).wrapping(none), text(meta.join(" · ")).size(11).color(dim).wrapping(none)].spacing(8),
            ]
            .spacing(2);
            if let Some(hit) = &p.hit {
                body = body.push(text(format!("… {hit}")).size(11).font(mono).color(th.warn).wrapping(none));
            }
            let delete: Element<'_, Message> = if h.confirm.as_deref() == Some(p.id.as_str()) {
                row![key("delete for good".into(), BAD, Msg::HistoryDelete(i)), key("keep".into(), dim, Msg::HistoryPick(i))].spacing(6).into()
            } else {
                key("delete".into(), dim, Msg::HistoryDelete(i)).into()
            };
            let line = row![container(body).width(Length::Fill).clip(true), delete].spacing(10).align_y(Alignment::Center);
            list = list
                .push(button(line).on_press(Message::Ai(Msg::HistoryPick(i))).padding([7, 12]).width(Length::Fill).style(move |_, _| button::Style {
                    background: on.then(|| cur_bg.into()),
                    border: Border { color: if on { accent } else { Color::TRANSPARENT }, width: 1.0, radius: 2.0.into() },
                    ..Default::default()
                }))
                .push(self.hrule());
        }
        if h.items.is_empty() {
            let why = if h.loading {
                "Looking…"
            } else if h.query.is_empty() {
                "No conversations yet. Every agent you give a task is kept here."
            } else {
                "Nothing matches. Search looks at what you asked, what the agent said, the commands it ran and the files it changed."
            };
            list = list.push(container(text(why).size(12).color(dim)).padding(14));
        }
        let left = column![top, self.hrule(), scrollable(list).height(Length::Fill)];

        let right: Element<'_, Message> = match h.items.get(h.sel) {
            None => container(text("Pick a conversation.").size(12).color(dim)).padding(14).into(),
            Some(p) => {
                let mut meta = vec![p.agent.name().to_string()];
                meta.extend(p.model.clone());
                meta.push(ago(p.updated.max(p.started)));
                let mut col = column![
                    text(p.title.as_str()).size(14).color(fg),
                    text(meta.join(" · ")).size(11).color(dim),
                    text(format!("{} · {} · you spoke {}", plural(p.steps as usize, "step"), plural(p.files.len(), "file") + " changed", plural(p.messages as usize, "time"))).size(11).color(dim),
                ]
                .spacing(6);
                for f in p.files.iter().take(12) {
                    col = col.push(text(format!("✎ {f}")).size(11).font(mono).color(Color::from_rgb8(0x98, 0xc3, 0x79)).wrapping(none));
                }
                if p.files.len() > 12 {
                    col = col.push(text(format!("… {} more", p.files.len() - 12)).size(11).color(dim));
                }
                if !p.last.is_empty() {
                    col = col.push(space().height(6)).push(text("It last said").size(10).color(dim)).push(text(p.last.as_str()).size(12).color(fg));
                }
                col = col.push(space().height(10)).push(
                    row![key("↵ reopen and continue".into(), fg, Msg::HistoryOpen(h.sel)), key("replay in Follow".into(), fg, Msg::HistoryReplay(h.sel))].spacing(8),
                );
                let live = matches!(p.agent, AiAgent::Claude | AiAgent::Codex | AiAgent::OpenCode | AiAgent::Grok);
                if live {
                    col = col.push(text("Continuing resumes the assistant's own session of it, where the code lives. Replay walks the editor through its steps, with the files as they are now.").size(11).color(dim));
                }
                scrollable(container(col).padding(14)).height(Length::Fill).into()
            }
        };
        let hints = row![self.hint("type", "search"), self.hint("↑↓", "pick"), self.hint("↵", "reopen"), self.hint("tab", "mailbox"), self.hint("esc", "back to agents")].spacing(14);
        let body = row![container(left).width(Length::FillPortion(5)).height(Length::Fill), self.vrule(), container(right).width(Length::FillPortion(4)).height(Length::Fill)];
        column![body.height(Length::Fill), self.hrule(), container(hints).padding(Padding { left: 12.0, right: 12.0, top: 6.0, bottom: 8.0 })].into()
    }

    /// The history tab on the mailbox: every message the agents sent each other, the latest first,
    /// whole.
    fn ai_history_mail_view<'a>(&'a self, top: iced::widget::Container<'a, Message>) -> Element<'a, Message> {
        let h = &self.ai.history;
        let th = &self.theme;
        let (fg, dim, accent, mono) = (th.foreground, th.dim, th.accent, self.face.font);
        let none = text::Wrapping::None;
        let mut list = column![].padding(Padding { right: 8.0, ..Padding::ZERO });
        for l in &h.letters {
            let head = row![text(format!("{} → {}", l.from, l.to)).size(11).font(mono).color(accent).wrapping(none), space::horizontal(), text(ago(l.at)).size(11).color(dim).wrapping(none)]
                .spacing(10)
                .align_y(Alignment::Center);
            list = list.push(container(column![head, text(l.text.trim()).size(12).color(fg)].spacing(3)).padding([8, 12]).width(Length::Fill)).push(self.hrule());
        }
        if h.letters.is_empty() {
            let why = if h.loading {
                "Looking…"
            } else if h.query.is_empty() {
                "No messages yet. When agents working here at once talk through the mailbox, it is all kept here."
            } else {
                "Nothing matches. Search looks at who sent each message, to whom, and what it says."
            };
            list = list.push(container(text(why).size(12).color(dim)).padding(14));
        }
        let body = column![top, self.hrule(), scrollable(list).style(crate::theme::scrollbar).height(Length::Fill)];
        let hints = row![self.hint("type", "search"), self.hint("tab", "conversations"), self.hint("esc", "back to agents")].spacing(14);
        column![body.height(Length::Fill), self.hrule(), container(hints).padding(Padding { left: 12.0, right: 12.0, top: 6.0, bottom: 8.0 })].into()
    }
}
