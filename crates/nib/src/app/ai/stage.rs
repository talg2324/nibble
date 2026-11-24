//! The agents tab: several agents at once, shown working instead of chatting.
//!
//! It takes the editor area like a file tab. Left, the agents: each row says what that agent is
//! doing right now (state, a clock, two lines of what it's looking at). Right, the one selected:
//! its steps (what it said, read, edited, ran), its terminal beside them, and the message box.
//! The list folds away to leave one agent.
//!
//! The terminal is a rendering of what the harness reported: the command, then its output. nib
//! doesn't run the agent's commands. Claude Code only hands over a command's output when the
//! command ends, so until then the terminal shows the command and a clock.
//!
//! Panic pauses every agent (the daemon freezes their processes) and asks which to stop.

use super::act::Act;
use super::ink::{self, Ink};
use super::caret::next_char;
use super::select::{plain_block, Blocks};
use super::{compose, wait, Chat, Effort, Msg, Session, Side, Step, Turn, Who, TRANSCRIPT_ID};
use crate::app::plan::{self, Outcome, PlanMsg};
use crate::app::{async_sleep, App, Focus, Message};
use iced::advanced::widget::{operate, operation, Id as WidgetId};

/// The message box's scroller.
const INPUT_ID: &str = "ai-input";
/// How tall the message box grows before it scrolls.
const INPUT_MAX_HEIGHT: f32 = 200.0;
/// The column left of each thing in the feed that marks what it is (🤖 🔧 📚 ✉️ ✅ …).
const MARK_WIDTH: f32 = 18.0;
/// The tallest a picture an agent read is drawn in the feed (it shrinks to fit, keeping its shape).
const PICTURE_HEIGHT: f32 = 360.0;
use iced::widget::text::Span;
use iced::widget::{button, column, container, markdown, mouse_area, pick_list, responsive, rich_text, row, scrollable, space, span, stack, svg, text, tooltip};
use iced::gradient::Linear;
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Padding, Task};
use nib_editor::keymap::KeyChord;
use nib_editor::Command;
use nib_proto::{AiAgent, AiMode, AiStepKind, ClientNotify, SkillGroup};
use serde_json::{json, Value};
use std::time::Duration;

const RAIL: f32 = 270.0;
/// The mailbox's height in the rail: it keeps it however many messages there are.
const MAIL: f32 = 320.0;
const TERMINAL_ID: &str = "ai-terminal";
const OK: Color = Color::from_rgb8(0x98, 0xc3, 0x79);
const BAD: Color = Color::from_rgb8(0xe0, 0x6c, 0x75);
/// The tools Claude Code's steps are named after, for the skills drawer.
const TOOLS: &[(&str, &str)] = &[
    ("Read", "read a file"),
    ("Edit", "change part of a file"),
    ("Write", "write a whole file"),
    ("Bash", "run a command (shown in the agent's terminal)"),
    ("Grep / Glob", "search file contents and names"),
    ("Skill", "load one of the skills above"),
    ("Agent", "start a sub-agent"),
];

/// How an agent is doing, for its dot and status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Working,
    Paused,
    Idle,
    Failed,
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_end()
}

/// `line` with each Markdown image (`![alt](url)`) written as `<image>`, for the list's preview.
fn hide_images(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("![") {
        let after = &rest[at + 2..];
        let end = after.find("](").and_then(|close| after[close + 2..].find(')').map(|paren| close + 2 + paren + 1));
        match end {
            Some(end) => {
                out.push_str(&rest[..at]);
                out.push_str("<image>");
                rest = &after[end..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

/// What the other agents call this one: after its task, or its assistant and session number
/// until it has one. `None` before it has started.
pub(super) fn agent_name(c: &Chat) -> Option<String> {
    match (&c.name, c.agent, &c.session) {
        (Some(n), _, _) => Some(n.clone()),
        (None, Some(a), Session::Ready(id)) => Some(a.mail_name(*id)),
        _ => None,
    }
}

/// A small outlined button that answers what the agent asks.
fn answer_key<'a>(label: impl Into<String>, color: Color, msg: Msg) -> button::Button<'a, Message> {
    button(text(label.into()).size(11).color(color)).on_press(Message::Ai(msg)).padding([2, 8]).style(move |_: &iced::Theme, _: button::Status| button::Style {
        border: Border { color, width: 1.0, radius: 3.0.into() },
        ..Default::default()
    })
}

/// A plan Claude Code keeps while in plan mode (`~/.claude/plans/*.md`).
fn is_plan_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.contains("/.claude/plans/") && p.ends_with(".md")
}

/// The step still under way, if any (the latest one).
fn running(c: &Chat) -> Option<&Step> {
    c.turns.iter().rev().filter_map(|t| t.step.as_ref()).find(|s| s.done.is_none())
}

/// How many sub-agents it sent to the background are still at work.
fn helpers(c: &Chat) -> usize {
    c.turns.iter().filter_map(|t| t.step.as_ref()).filter(|s| s.background && s.done.is_none()).count()
}

/// What the agent is doing, in a word or two.
pub(super) fn state_of(c: &Chat) -> (String, Tone) {
    if c.frozen {
        return ("paused".into(), Tone::Paused);
    }
    if c.ask.is_some() {
        return ("waiting for you".into(), Tone::Paused);
    }
    match &c.session {
        Session::Starting => return ("starting".into(), Tone::Working),
        Session::Failed(_) => return ("failed".into(), Tone::Failed),
        _ => {}
    }
    if c.compacting {
        return ("compacting".into(), Tone::Working);
    }
    if !c.busy {
        // Its turn is over, but a sub-agent it sent to the background still works.
        if helpers(c) > 0 {
            return ("idle · helper at work".into(), Tone::Working);
        }
        return (if c.turns.is_empty() { "new" } else { "idle" }.into(), Tone::Idle);
    }
    let step = running(c);
    // What it does to a file, from its harness's tools or the shell alike (the last change, else
    // the first read).
    let file = step.and_then(|s| s.acts.iter().rev().find(|a| a.writes()).or(s.acts.first()));
    if let Some(a) = file {
        let verb = match a {
            Act::Read { .. } => "reading",
            Act::Edit { .. } | Act::Patch { .. } => "editing",
            Act::Write { append: true, .. } => "adding to",
            Act::Write { .. } => "writing",
        };
        return (format!("{verb} {}", file_name(a.path())), Tone::Working);
    }
    let doing = match step.map(|s| &s.kind) {
        None => "thinking".to_string(),
        Some(AiStepKind::Command { .. }) => "in its terminal".into(),
        Some(AiStepKind::Read { path, .. }) => format!("reading {}", file_name(path)),
        Some(AiStepKind::Edit { path, .. }) => format!("editing {}", file_name(path)),
        Some(AiStepKind::Write { path, .. }) => format!("writing {}", file_name(path)),
        Some(AiStepKind::Skill { name }) => format!("loading {name}"),
        Some(AiStepKind::Search { .. }) => "searching".into(),
        Some(AiStepKind::Agent { .. }) => "running a sub-agent".into(),
        Some(AiStepKind::Message { .. }) => "messaging a sub-agent".into(),
        Some(AiStepKind::Other { tool, .. }) => format!("using {tool}"),
    };
    (doing, Tone::Working)
}

/// A running command's clock: the text, whether it counts down, and what kind of wait it is.
fn step_clock(s: &Step) -> Option<(String, bool, &'static str)> {
    if s.done.is_some() {
        return None;
    }
    let elapsed = s.started.elapsed().as_secs_f64();
    let command = match &s.kind {
        AiStepKind::Command { command, .. } => command,
        AiStepKind::Agent { .. } => return Some((wait::clock(elapsed.floor()), false, if s.background { "in the background" } else { "running" })),
        _ => return None,
    };
    Some(match wait::of(command) {
        Some(w) if w.leading && elapsed < w.secs => (wait::clock(w.secs - elapsed), true, if w.bound { "timeout: may end sooner" } else { "sleeping" }),
        _ => (wait::clock(elapsed.floor()), false, "running"),
    })
}

/// The sub-agents an agent's row shows: those started since the user last wrote, and any
/// still at work, oldest first.
pub(super) fn subagents(c: &Chat) -> Vec<&Step> {
    let since = c.turns.iter().rposition(|t| t.who == Who::User).unwrap_or(0);
    c.turns
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.step.as_ref().map(|s| (i, s)))
        .filter(|(i, s)| matches!(s.kind, AiStepKind::Agent { .. }) && (*i >= since || s.done.is_none()))
        .map(|(_, s)| s)
        .collect()
}

/// A step's ancestors' ids, nearest first (at most as many as the chat has steps).
fn ancestors<'a>(c: &'a Chat, s: &'a Step) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut parent = s.parent.as_deref();
    while let Some(p) = parent.filter(|_| out.len() < c.turns.len()) {
        out.push(p);
        parent = c.turns.iter().filter_map(|t| t.step.as_ref()).find(|s| s.id == p).and_then(|s| s.parent.as_deref());
    }
    out
}

/// Whether a step belongs to sub-agent `id`'s thread: the sub-agent itself, what it (or a
/// sub-agent of its own) did, and what its agent wrote to it.
pub(super) fn in_thread(c: &Chat, s: &Step, id: &str) -> bool {
    s.id == id || matches!(&s.kind, AiStepKind::Message { to: Some(t), .. } if t == id) || ancestors(c, s).contains(&id)
}

/// A sub-agent's state, in a word. One its harness called done is at work again while a step
/// of its own still runs (a sub-agent woken by its own background command).
fn sub_state(c: &Chat, s: &Step) -> (&'static str, Tone) {
    let busy = || c.turns.iter().filter_map(|t| t.step.as_ref()).any(|x| x.done.is_none() && x.id != s.id && ancestors(c, x).contains(&s.id.as_str()));
    match &s.done {
        Some(_) if busy() => ("running", Tone::Working),
        None if s.background => ("in the background", Tone::Working),
        None => ("running", Tone::Working),
        Some(d) if d.ok => ("done", Tone::Idle),
        Some(d) if d.output.is_empty() => ("stopped", Tone::Paused),
        Some(_) => ("failed", Tone::Failed),
    }
}

/// What a sub-agent is called in the mailbox: its agent's name, then its task in a few words.
fn sub_name(agent: &str, description: &str) -> String {
    let slug: Vec<String> = description
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(str::to_lowercase)
        .collect();
    if slug.is_empty() { format!("{agent}/helper") } else { format!("{agent}/{}", slug.join("-")) }
}

/// The description of the sub-agent with step `id`.
fn sub_description<'a>(c: &'a Chat, id: &str) -> Option<&'a str> {
    c.turns.iter().filter_map(|t| t.step.as_ref()).find(|s| s.id == id).and_then(|s| match &s.kind {
        AiStepKind::Agent { description, .. } => Some(description.as_str()),
        _ => None,
    })
}

/// Who wrote a sub-agent message step and to whom (mailbox names); `None`: not one.
fn sub_ends(c: &Chat, s: &Step) -> Option<(String, String)> {
    sub_ends_of(c, &s.kind, s.parent.as_deref())
}

fn sub_ends_of(c: &Chat, kind: &AiStepKind, parent: Option<&str>) -> Option<(String, String)> {
    let agent = agent_name(c).unwrap_or_else(|| "agent".into());
    // A sub-agent's own name, and its agent's: a sub-agent of a sub-agent answers to that one.
    let name = |id: &str| sub_name(&agent, sub_description(c, id).unwrap_or_default());
    let caller = |id: &str| {
        let up = c.turns.iter().filter_map(|t| t.step.as_ref()).find(|s| s.id == id).and_then(|s| s.parent.clone());
        up.map_or_else(|| agent.clone(), |p| name(&p))
    };
    match kind {
        AiStepKind::Message { to: Some(to), .. } => Some((parent.map_or_else(|| agent.clone(), name), name(to))),
        AiStepKind::Message { to: None, .. } => parent.map(|p| (name(p), caller(p))),
        _ => None,
    }
}

/// What an agent and its sub-agents say to each other, for the mailbox list: a sub-agent's
/// brief as it starts, a message either way.
pub(super) fn sub_mail(c: &Chat, s: &nib_proto::AiStep) -> Option<nib_proto::AiMail> {
    let agent = agent_name(c).unwrap_or_else(|| "agent".into());
    let (from, to, text) = match &s.kind {
        AiStepKind::Agent { description, prompt, .. } if !prompt.trim().is_empty() => {
            let from = match s.parent.as_deref() {
                Some(p) => sub_name(&agent, sub_description(c, p).unwrap_or_default()),
                None => agent.clone(),
            };
            (from, sub_name(&agent, description), prompt.clone())
        }
        AiStepKind::Message { text, .. } => {
            let (from, to) = sub_ends_of(c, &s.kind, s.parent.as_deref())?;
            (from, to, text.clone())
        }
        _ => return None,
    };
    Some(nib_proto::AiMail { from, to, text, incoming: false })
}

/// A sub-agent for `nib ctl state`.
pub(super) fn sub_json(c: &Chat, s: &Step) -> Value {
    let AiStepKind::Agent { description, kind, .. } = &s.kind else { return Value::Null };
    let steps = c.turns.iter().filter_map(|t| t.step.as_ref()).filter(|x| x.id != s.id && in_thread(c, x, &s.id)).count();
    json!({
        "id": s.id,
        "parent": s.parent,
        "description": description,
        "kind": kind,
        "name": sub_name(&agent_name(c).unwrap_or_else(|| "agent".into()), description),
        "state": sub_state(c, s).0,
        "steps": steps,
        "clock": step_clock(s).map(|(c, _, _)| c),
    })
}

/// Two lines of what a step is doing, for a row in the agent list.
fn step_preview(s: &Step) -> String {
    match &s.kind {
        AiStepKind::Command { command, .. } => {
            let (clock, down, note) = step_clock(s).unwrap_or_default();
            format!("➜ {}\n{} {clock} {}", clip(first_line(command), 36), if down { "◷" } else { "…" }, if down { note } else { "" })
        }
        AiStepKind::Read { path, offset, .. } => format!("◎ {}{}", file_name(path), offset.map(|o| format!(":{o}")).unwrap_or_default()),
        AiStepKind::Edit { path, new, .. } => format!("✎ {}\n{}", file_name(path), clip(first_line(new).trim(), 38)),
        AiStepKind::Write { path, content } => format!("✎ {}\n{}", file_name(path), clip(first_line(content).trim(), 38)),
        AiStepKind::Skill { name } => format!("skill {name}"),
        AiStepKind::Search { pattern, .. } => format!("⌕ {}", clip(pattern, 36)),
        AiStepKind::Agent { description, .. } => format!("sub-agent\n{}", clip(description, 38)),
        AiStepKind::Message { text, .. } => format!("» {}", clip(first_line(text.trim()), 36)),
        AiStepKind::Other { tool, .. } => tool.clone(),
    }
}

/// A step for `nib ctl state`.
pub(super) fn step_json(s: &Step) -> Value {
    let (kind, what) = match &s.kind {
        AiStepKind::Command { command, timeout_ms, background, .. } => {
            let w = wait::of(command).map(|w| json!({ "seconds": w.secs, "timeout": w.bound, "counts_down": w.leading }));
            ("command", json!({ "command": command, "timeout_ms": timeout_ms, "background": background, "wait": w }))
        }
        AiStepKind::Read { path, offset, limit } => ("read", json!({ "path": path, "offset": offset, "limit": limit })),
        AiStepKind::Edit { path, old, new } => ("edit", json!({ "path": path, "old": old, "new": new })),
        AiStepKind::Write { path, content } => ("write", json!({ "path": path, "bytes": content.len() })),
        AiStepKind::Skill { name } => ("skill", json!({ "name": name })),
        AiStepKind::Search { pattern, path } => ("search", json!({ "pattern": pattern, "path": path })),
        AiStepKind::Agent { description, prompt, kind } => ("agent", json!({ "description": description, "prompt": prompt, "kind": kind })),
        AiStepKind::Message { to, text } => ("message", json!({ "to": to, "text": text })),
        AiStepKind::Other { tool, input } => ("other", json!({ "tool": tool, "input": input })),
    };
    json!({
        "id": s.id,
        "parent": s.parent,
        "kind": kind,
        "what": what,
        "running": s.done.is_none(),
        "background": s.background,
        "ok": s.done.as_ref().map(|d| d.ok),
        "output": s.done.as_ref().map(|d| d.output.as_str()),
        "line": s.done.as_ref().and_then(|d| d.line),
        "seconds": s.done.as_ref().map(|d| d.took.as_secs_f64()),
        "clock": step_clock(s).map(|(c, _, _)| c),
        // What it does to files, its harness's tools and the shell alike.
        "acts": s.acts.iter().map(|a| match a {
            Act::Read { path, from, to } => json!({ "act": "read", "path": path, "from": from, "to": to }),
            Act::Edit { path } => json!({ "act": "edit", "path": path }),
            Act::Patch { path } => json!({ "act": "patch", "path": path }),
            Act::Write { path, body, append } => json!({ "act": if *append { "append" } else { "write" }, "path": path, "lines": body.as_ref().map(|b| b.lines().count()) }),
        }).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------------------------
// what the agents tab does
// ---------------------------------------------------------------------------------------------

impl App {
    pub(super) fn ai_select(&mut self, i: usize) {
        if i < self.ai.chats.len() {
            if self.ai.cur != i {
                self.ai.accepting = false;
            }
            self.ai.cur = i;
            self.ai.thread = None;
            self.ai.picking = false;
            self.ai.pull_focus = false;
            self.focus = Focus::Ai;
            // It said it is done: ask whether to accept that.
            if self.ai.finished.is_some() {
                self.ai.accepting = true;
            }
        }
    }

    /// Keys while the accept dialog is up: ⌘↵ accepts the job as done and ends the agent, ⌘D
    /// shows its work, Esc puts the question away. Anything else puts it away too, and is not
    /// used up (`None`): it goes on to the chat, so typing back to the agent loses nothing.
    pub(super) fn ai_accept_key(&mut self, chord: nib_editor::keymap::KeyChord) -> Option<Task<Message>> {
        use nib_editor::keymap::Key;
        let held = if cfg!(target_os = "macos") { chord.logo } else { chord.ctrl || chord.logo };
        if chord.key == Key::Enter && held {
            return Some(self.ai_accept(true));
        }
        // ⌘D: its work as a pull request, in the diff tab (from where it can still be landed).
        if chord.key == Key::Char('d') && held {
            return Some(self.ai_accept_diff());
        }
        let away = self.ai_accept(false);
        (chord.key == Key::Esc).then_some(away)
    }

    /// Put the question away and show the agent's work in the diff tab.
    pub(in crate::app) fn ai_accept_diff(&mut self) -> Task<Message> {
        self.ai.accepting = false;
        match self.ai_shown_session() {
            Some(id) => self.diff_show(nib_proto::DiffScope::Agent(id)),
            None => Task::none(),
        }
    }

    /// Accept the shown agent's job as done (it is ended), or put the question away.
    pub(in crate::app) fn ai_accept(&mut self, yes: bool) -> Task<Message> {
        self.ai.accepting = false;
        let Some(f) = self.ai.finished.clone().filter(|_| yes) else { return Task::none() };
        // Worktree-per-feature on, and some of its work is in the user's tree, where accepting
        // would leave it uncommitted: not done. It is told to move it, and goes on.
        if self.ai.wpf && !f.stray.is_empty() {
            if let Some(id) = self.ai_shown_session() {
                let text = format!(
                    "(nib: the user tried to accept your work, but worktree-per-feature is on and these files you changed are still changed in the shared project folder, not on your branch: {}. Move them (`nib-work start SLUG --carry`, or `nib-work carry` if you have a branch), check your work there, and run nib-mail done again.)",
                    f.stray.join(", ")
                );
                self.ai_tell(id, text);
                return self.flash_warn(format!("not accepted: {} file(s) of its work are uncommitted in your tree; it was told to move them to its branch", f.stray.len()));
            }
        }
        // On its own branch: the work is landed first, and the agent is ended once that worked.
        if let Some(id) = self.ai_shown_session().filter(|id| self.diff_branch(*id).is_some()) {
            return self.diff_land(id, &f);
        }
        self.ai_kill(self.ai.cur)
    }

    /// The next (or previous) agent in the list.
    pub(in crate::app) fn ai_cycle(&mut self, back: bool) {
        let n = self.ai.chats.len();
        self.ai_select(if back { (self.ai.cur + n - 1) % n } else { (self.ai.cur + 1) % n });
    }

    /// Another agent beside the ones there are: a blank one with the assistant and model picked
    /// last, and the dropdown open to pick another (unless `[ai] pick_model` is off).
    pub(in crate::app) fn ai_new_agent(&mut self) -> Task<Message> {
        self.ai.open = true;
        self.ai.front = true;
        self.focus = Focus::Ai;
        let i = match self.ai.chats.iter().position(Chat::blank) {
            // It may have been seeded before the last thinking level / system prompt was set.
            Some(i) => {
                let c = std::mem::take(&mut self.ai.chats[i]);
                self.ai.chats[i] = self.ai.seeded(c);
                i
            }
            None => {
                let key = self.ai.next_key;
                self.ai.next_key += 1;
                let (agent, chosen) = match &self.ai.last {
                    Some((a, m)) => (Some(*a), m.clone()),
                    None => (self.ai.agent, self.ai.chosen.clone()),
                };
                let skills = if agent == self.ai.agent { self.ai.skills.clone() } else { Vec::new() };
                let chat = self.ai.seeded(Chat { key, agent, chosen, skills, ..Default::default() });
                self.ai.chats.push(chat);
                self.ai.chats.len() - 1
            }
        };
        self.ai.cur = i;
        if self.ai.pick_model {
            return self.ai_pick();
        }
        // Straight to the message box: the chat starts on the assistants known (once they are).
        self.ai.picking = false;
        self.ai_recheck()
    }

    /// A new agent with `draft` already in its message box, the caret at its end, for the user
    /// to finish and send (the Apps panel's "Register via AI chat").
    pub(in crate::app) fn ai_new_agent_with_draft(&mut self, draft: String) -> Task<Message> {
        let t = self.ai_new_agent();
        let c = &mut self.ai.chats[self.ai.cur];
        c.input = draft;
        c.back = 0;
        c.anchor = None;
        t
    }

    /// What the shown agent's message box holds.
    #[cfg(test)]
    pub(in crate::app) fn ai_draft(&self) -> String {
        self.ai.chats[self.ai.cur].input.clone()
    }

    /// End agent `i`'s conversation and take it off the list. A local model nobody else is
    /// using is unloaded from memory.
    pub(in crate::app) fn ai_kill(&mut self, i: usize) -> Task<Message> {
        let Some(c) = self.ai.chats.get(i) else { return Task::none() };
        // Following it: the show ends with it (its tab and marks tidied away).
        if self.ai_following() == Some(c.key) {
            let stop = self.ai_follow_stop(true);
            return Task::batch([stop, self.ai_kill(i)]);
        }
        let (key, agent, model) = (c.key, c.agent, c.chosen.clone());
        // The daemon thaws a frozen one before it ends it.
        let mut sessions: Vec<_> = c.retired.into_iter().chain(c.switching.as_ref().map(|s| s.0)).collect();
        if let Session::Ready(id) = c.session { sessions.push(id); }
        sessions.sort_unstable();
        sessions.dedup();
        for id in sessions { self.notify(ClientNotify::AiClose { id }); }
        self.ai.chats.remove(i);
        if let Some((rows, sel)) = &mut self.ai.panic {
            rows.retain(|(k, _)| *k != key);
            *sel = (*sel).min(rows.len().saturating_sub(1));
            if rows.is_empty() {
                self.ai.panic = None;
            }
        }
        if self.ai.chats.is_empty() {
            let key = self.ai.next_key;
            self.ai.next_key += 1;
            let chat = self.ai.seeded(Chat { key, agent, chosen: model.clone(), ..Default::default() });
            self.ai.chats.push(chat);
        }
        let shown = self.ai.cur == i;
        if self.ai.cur > i {
            self.ai.cur -= 1;
        }
        self.ai.cur = self.ai.cur.min(self.ai.chats.len() - 1);
        // Another agent takes the place of the one shown: if it said it is done, ask, as
        // going to it would (one landed, the next one's question comes up).
        if shown {
            self.ai.accepting = self.ai.finished.is_some();
        }
        let (Some(AiAgent::OpenCode), Some(model)) = (agent, model) else { return Task::none() };
        let local = self.ai_models(AiAgent::OpenCode).iter().any(|m| m.id == model && m.warm == Some(true));
        let live = |c: &Chat| matches!(c.session, Session::Starting | Session::Ready(_));
        let used = self.ai.chats.iter().any(|c| c.agent == Some(AiAgent::OpenCode) && c.chosen.as_deref() == Some(model.as_str()) && live(c));
        if local && !used {
            return self.ai_warm(model, false);
        }
        Task::none()
    }

    /// Stop what the shown agent is doing. Its conversation stays.
    pub(in crate::app) fn ai_stop(&mut self) {
        if let (Session::Ready(id), true) = (&self.ai.session, self.ai.busy) {
            let id = *id;
            self.ai.stopping = true;
            self.notify(ClientNotify::AiInterrupt { id });
        }
    }

    /// Pause every working agent and ask which to stop: all are marked to begin with.
    pub(in crate::app) fn ai_panic(&mut self) -> Task<Message> {
        if self.ai.panic.is_some() {
            return Task::none();
        }
        let working: Vec<(u64, u32)> = self
            .ai
            .chats
            .iter()
            .filter(|c| c.busy)
            .filter_map(|c| match c.session {
                Session::Ready(id) => Some((c.key, id)),
                _ => None,
            })
            .collect();
        if working.is_empty() {
            return self.flash_info("no agent is working");
        }
        for (key, id) in &working {
            self.notify(ClientNotify::AiSuspend { id: *id, on: true });
            if let Some(c) = self.ai.chats.iter_mut().find(|c| c.key == *key) {
                c.frozen = true;
            }
        }
        // While following, only the followed agent is marked to begin with.
        let followed = self.ai_following().filter(|f| working.iter().any(|(key, _)| key == f) && !self.ai.shown());
        self.ai.panic = Some((working.iter().map(|(key, _)| (*key, followed.is_none_or(|f| f == *key))).collect(), 0));
        self.ai.open = true;
        self.ai.front = true;
        self.ai.picking = false;
        self.focus = Focus::Ai;
        Task::none()
    }

    /// Let go: the marked agents are stopped, the others carry on from where they were.
    pub(super) fn ai_release(&mut self) -> Task<Message> {
        let Some((rows, _)) = self.ai.panic.take() else { return Task::none() };
        for (key, marked) in rows {
            let Some(c) = self.ai.chats.iter_mut().find(|c| c.key == key) else { continue };
            let Session::Ready(id) = c.session else { continue };
            if std::mem::take(&mut c.resume) {
                // It couldn't be frozen and was interrupted instead.
                if !marked {
                    c.busy = true;
                    c.turns.push(Turn::new(Who::Note, "Paused by you; told to continue.".into()));
                    self.notify(ClientNotify::AiSend { id, text: "continue".into(), attach: Vec::new(), images: Vec::new() });
                }
                continue;
            }
            c.frozen = false;
            if marked {
                c.stopping = true;
                // Written while it is frozen; it acts on it once thawed.
                self.notify(ClientNotify::AiInterrupt { id });
            }
            self.notify(ClientNotify::AiSuspend { id, on: false });
        }
        Task::none()
    }

    /// Keys while the panic dialog is up. Esc doesn't let go: only Enter does.
    pub(super) fn ai_panic_key(&mut self, key: nib_editor::keymap::Key) -> Task<Message> {
        use nib_editor::keymap::Key;
        let Some((rows, sel)) = &mut self.ai.panic else { return Task::none() };
        match key {
            Key::Up => *sel = sel.saturating_sub(1),
            Key::Down => *sel = (*sel + 1).min(rows.len().saturating_sub(1)),
            Key::Char(' ') => {
                if let Some(r) = rows.get_mut(*sel) {
                    r.1 = !r.1;
                }
            }
            Key::Enter => return self.ai_release(),
            _ => {}
        }
        Task::none()
    }

    /// Start the one-second clock if a command is running and it isn't ticking yet.
    pub(super) fn ai_arm_clock(&mut self) -> Task<Message> {
        let needed = self.ai.chats.iter().any(|c| running(c).is_some());
        if !needed || self.ai.ticking {
            return Task::none();
        }
        self.ai.ticking = true;
        Task::perform(async_sleep(Duration::from_secs(1)), |_| Message::Ai(Msg::Clock))
    }

    pub(super) fn ai_clock(&mut self) -> Task<Message> {
        self.ai.ticking = false;
        self.ai_arm_clock()
    }

    /// A path as the user knows it: inside the project, relative to its root.
    fn ai_rel<'a>(&self, path: &'a str) -> &'a str {
        path.strip_prefix(self.root.as_str()).map(|p| p.trim_start_matches('/')).filter(|p| !p.is_empty()).unwrap_or(path)
    }

    fn ai_group_color(&self, skill: &str) -> Color {
        let skill = skill.rsplit(':').next().unwrap_or(skill);
        let g = self.ai_skills().iter().find(|s| s.name == skill).map_or(SkillGroup::Nib, |s| s.group);
        self.theme.skill_group(g)
    }
}

// ---------------------------------------------------------------------------------------------
// view
// ---------------------------------------------------------------------------------------------

type Spans<'a> = Vec<Span<'a, (), Font>>;

impl App {
    fn ai_tone(&self, t: Tone) -> Color {
        let th = &self.theme;
        match t {
            Tone::Working => th.accent,
            Tone::Paused => th.warn,
            Tone::Idle => th.dim,
            Tone::Failed => BAD,
        }
    }

    /// A command with the names nib knows (`nib ctl`, `nib ai`) in nib's colour.
    /// A command as the shell would split it, `nib ctl` / `nib ai` in nib's colour.
    fn ai_command_spans(&self, command: &str, fg: Color) -> Spans<'static> {
        ink::command(command).into_iter().map(|(t, k)| span(t.to_string()).color(self.ai_ink(k, fg))).collect()
    }

    /// The theme colour for a piece of the agent's terminal; `fg` for plain text.
    fn ai_ink(&self, k: Ink, fg: Color) -> Color {
        let th = &self.theme;
        match k {
            Ink::Plain => fg,
            Ink::Dim => th.dim,
            Ink::Cmd => th.scope("function"),
            Ink::Flag => th.scope("constant"),
            Ink::Str => th.scope("string"),
            Ink::Var => th.scope("variable"),
            Ink::Op => th.scope("keyword.control"),
            Ink::Nib => th.skill_nib,
            Ink::Path => th.scope("type"),
            Ink::Num => th.scope("constant.numeric"),
            Ink::Hash => th.scope("constant.character"),
            Ink::Add | Ink::Good => OK,
            Ink::Del | Ink::Bad => BAD,
            Ink::Hunk => th.scope("keyword"),
            Ink::Head => th.foreground,
            Ink::Warn => th.warn,
        }
    }

    fn ai_chip<'a>(&self, name: &str) -> Element<'a, Message> {
        let c = self.theme.skill_tool;
        container(text(name.to_string()).size(10).font(self.face.font).color(c))
            .padding([0, 5])
            .style(move |_| container::Style { border: Border { color: c, width: 1.0, radius: 3.0.into() }, ..Default::default() })
            .into()
    }

    /// One step in the list.
    fn ai_step_row<'a>(&'a self, s: &'a Step) -> Element<'a, Message> {
        let th = &self.theme;
        let (fg, dim, mono) = (th.foreground, th.dim, self.face.font);
        let none = text::Wrapping::None;
        let small = |t: String, c: Color| text(t).size(11).font(mono).color(c).wrapping(none);
        let status = || -> Element<'_, Message> {
            match (&s.done, step_clock(s)) {
            (None, Some((clock, down, note))) => row![small(clock, if down { th.warn } else { dim }), small(note.into(), dim)].spacing(6).into(),
            (None, None) => small("…".into(), dim).into(),
            (Some(d), _) => {
                let (word, c) = if d.ok { ("ok", OK) } else if d.output.is_empty() { ("stopped", th.warn) } else { ("failed", BAD) };
                small(format!("{word} {}", wait::took(d.took.as_secs_f64())), c).into()
            }
            }
        };
        // What fills a row gives way (clipped) so the status always shows at the right edge.
        let fit = |e: Element<'a, Message>| -> Element<'a, Message> { container(e).width(Length::Fill).clip(true).into() };
        let line = |chip: &str, what: String, more: String| -> Element<'_, Message> {
            row![self.ai_chip(chip), fit(row![small(what, fg), small(more, dim)].spacing(8).into()), status()].spacing(8).align_y(Alignment::Center).into()
        };
        let body: Element<'_, Message> = match &s.kind {
            // Never the command's output here: it is in the terminal beside the list.
            // A command that reads or writes files shows as those files, like the harness's
            // own tools; the command itself goes under them, dim.
            AiStepKind::Command { command, description, .. } if !s.acts.is_empty() => {
                let mut rows = column![].spacing(3);
                for (n, a) in s.acts.iter().enumerate() {
                    let at = self.ai_rel(a.path()).to_string();
                    let (chip, more, body) = match a {
                        Act::Read { from, to, .. } => ("Read", match to {
                            Some(t) if t != from => format!(":{from}–{t}"),
                            Some(_) => format!(":{from}"),
                            None if *from > 1 => format!(":{from}"),
                            None => String::new(),
                        }, None),
                        Act::Patch { .. } => ("Edit", "in place".to_string(), None),
                        Act::Write { body, append, .. } => {
                            let lines = body.as_ref().map(|b| format!("{} lines", b.lines().count())).unwrap_or_default();
                            (if *append { "Append" } else { "Write" }, lines, body.as_deref())
                        }
                        Act::Edit { .. } => ("Edit", String::new(), None),
                    };
                    let head: Element<'_, Message> = if n == 0 {
                        line(chip, at, more)
                    } else {
                        row![self.ai_chip(chip), small(at, fg), small(more, dim)].spacing(8).align_y(Alignment::Center).into()
                    };
                    rows = rows.push(head);
                    if let Some(b) = body {
                        rows = rows.push(self.ai_diff("", b));
                    }
                }
                let how = description.as_deref().map(|d| format!("  · {d}")).unwrap_or_default();
                rows.push(small(clip(&format!("$ {}{how}", first_line(command)), 100), dim)).into()
            }
            AiStepKind::Command { command, description, background, .. } => {
                let more = description.clone().unwrap_or_default();
                let cmd = rich_text(self.ai_command_spans(&clip(first_line(command), 72), fg)).size(11).font(mono).wrapping(none);
                let bg = if *background { "term (background) →" } else { "term →" };
                column![
                    row![self.ai_chip("Bash"), fit(row![small(bg.into(), dim), cmd].spacing(8).into()), status()].spacing(8).align_y(Alignment::Center),
                    small(clip(&more, 90), dim),
                ]
                .spacing(1)
                .into()
            }
            AiStepKind::Read { path, offset, limit } => {
                let lines = match (offset, limit) {
                    (Some(o), Some(l)) => format!(":{o}–{}", o + l.saturating_sub(1)),
                    (Some(o), None) => format!(":{o}"),
                    _ => String::new(),
                };
                line("Read", self.ai_rel(path).to_string(), lines)
            }
            // Claude Code keeps its plan in a file under ~/.claude/plans: the plan itself shows as
            // a card when it is handed over, so its Markdown source isn't shown here too.
            AiStepKind::Edit { path, .. } | AiStepKind::Write { path, .. } if is_plan_file(path) => line("Plan", "writes its plan".into(), self.ai_rel(path).rsplit('/').next().unwrap_or_default().to_string()),
            AiStepKind::Edit { path, old, new } => {
                let at = s.done.as_ref().and_then(|d| d.line).map(|l| format!(":{l}")).unwrap_or_default();
                let head = line("Edit", format!("{}{at}", self.ai_rel(path)), format!("+{} −{}", new.lines().count(), old.lines().count()));
                column![head, self.ai_diff(old, new)].spacing(3).into()
            }
            AiStepKind::Write { path, content } => {
                let head = line("Write", self.ai_rel(path).to_string(), format!("{} lines", content.lines().count()));
                column![head, self.ai_diff("", content)].spacing(3).into()
            }
            AiStepKind::Skill { name } => {
                // Loud: a skill changes what the agent does next.
                let c = self.ai_group_color(name);
                let name = name.rsplit(':').next().unwrap_or(name);
                let group = self.ai_skills().iter().find(|k| k.name == name).map_or("nib", |k| k.group.label());
                let label = row![text(format!("skill {name}")).size(12).font(mono).color(c), small(format!("loaded by the agent · {group}"), dim)].spacing(10);
                container(label)
                    .padding(Padding { left: 8.0, top: 2.0, bottom: 2.0, right: 0.0 })
                    .style(move |_| container::Style { border: Border { color: c, width: 1.0, radius: 2.0.into() }, ..Default::default() })
                    .into()
            }
            AiStepKind::Search { pattern, path } => line("Search", clip(pattern, 60), path.as_deref().map(|p| self.ai_rel(p).to_string()).unwrap_or_default()),
            AiStepKind::Agent { description, prompt, kind } => {
                let head = line("Agent", clip(description, 80), kind.clone().unwrap_or_default());
                match self.ai_sub_brief(s, prompt) {
                    Some(brief) => column![head, brief].spacing(4).into(),
                    None => head,
                }
            }
            AiStepKind::Message { text, .. } => self.ai_sub_message(s, text),
            AiStepKind::Other { tool, input } => line(tool, clip(input, 80), String::new()),
        };
        // What a sub-agent does sits under it.
        container(body).padding(Padding { left: if s.parent.is_some() { 18.0 } else { 0.0 }, ..Padding::ZERO }).width(Length::Fill).into()
    }

    /// A message between an agent and its sub-agent: who to whom, then what it says, folded to
    /// `lines` lines until clicked (`key`: what remembers it is open).
    fn ai_sub_card<'a>(&self, key: String, from: String, to: String, body: &'a str, lines: usize) -> Element<'a, Message> {
        let th = &self.theme;
        let (fg, dim, accent, mono) = (th.foreground, th.dim, th.accent, self.face.font);
        let body = body.trim();
        let total = body.lines().count();
        let long = total > lines || body.chars().count() > lines * 120;
        let open = self.ai.unfolded.contains(&key);
        let mut head = row![text(format!("{from} → {to}")).size(11).font(mono).color(accent), space::horizontal()].align_y(Alignment::Center);
        if long {
            head = head.push(text(if open { "less".to_string() } else { format!("{total} lines · more") }).size(10).color(dim));
        }
        let shown = if open || !long {
            body.to_string()
        } else {
            let mut t: String = body.lines().take(lines).collect::<Vec<_>>().join("\n");
            if t.chars().count() > lines * 120 {
                t = clip(&t, lines * 120);
            }
            format!("{t} …")
        };
        let card = container(column![head, text(shown).size(12).color(fg)].spacing(3))
            .padding([4, 10])
            .width(Length::Fill)
            .style(move |_| container::Style { border: Border { color: Color { a: 0.6, ..accent }, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        if long {
            mouse_area(card).on_press(Message::Ai(Msg::Unfold(key))).interaction(iced::mouse::Interaction::Pointer).into()
        } else {
            card.into()
        }
    }

    /// The brief an agent wrote its sub-agent (a step of it), as a message, under the step.
    fn ai_sub_brief<'a>(&self, s: &'a Step, prompt: &'a str) -> Option<Element<'a, Message>> {
        if prompt.trim().is_empty() {
            return None;
        }
        let m = sub_mail(&self.ai, &nib_proto::AiStep { id: s.id.clone(), parent: s.parent.clone(), kind: s.kind.clone() })?;
        Some(self.ai_sub_card(format!("brief:{}", s.id), m.from, m.to, prompt, 3))
    }

    /// A message between the agent and a sub-agent (a step of it).
    fn ai_sub_message<'a>(&self, s: &'a Step, body: &'a str) -> Element<'a, Message> {
        let (from, to) = sub_ends(&self.ai, s).unwrap_or_else(|| ("sub-agent".into(), "agent".into()));
        self.ai_sub_card(s.id.clone(), from, to, body, 8)
    }

    /// The sub-agents of agent `i`, indented under its row: each one's task, state and what it
    /// is doing; a click shows its thread.
    fn ai_sub_rows(&self, i: usize, c: &Chat) -> Option<Element<'_, Message>> {
        let subs = subagents(c);
        if subs.is_empty() {
            return None;
        }
        let th = &self.theme;
        let (fg, dim, mono, cur_bg, accent) = (th.foreground, th.dim, self.face.font, th.current_line, th.accent);
        let none = text::Wrapping::None;
        let mut col = column![].spacing(1);
        for s in subs {
            let AiStepKind::Agent { description, kind, .. } = &s.kind else { continue };
            let depth = ancestors(c, s).len();
            let (state, tone) = sub_state(c, s);
            let color = self.ai_tone(tone);
            // What it is doing now: its latest step still at work, else what it last said.
            let mine = |x: &&Step| x.parent.as_deref() == Some(s.id.as_str());
            let steps: Vec<&Step> = c.turns.iter().filter_map(|t| t.step.as_ref()).filter(mine).collect();
            let doing = match steps.iter().rev().find(|x| x.done.is_none()) {
                Some(x) => step_preview(x).replace('\n', "  "),
                None => steps.iter().rev().find_map(|x| match &x.kind {
                    AiStepKind::Message { to: None, text } => Some(format!("» {}", first_line(text.trim()))),
                    _ => None,
                }).unwrap_or_default(),
            };
            let mut status = row![text(state).size(10).color(color).wrapping(none), text(format!("· {} steps", steps.len())).size(10).color(dim).wrapping(none), space::horizontal()].spacing(4).align_y(Alignment::Center);
            if let Some((clock, _, _)) = step_clock(s) {
                status = status.push(text(clock).size(10).font(mono).color(dim));
            }
            let mut body = column![
                row![svg(self.icons.network.clone()).width(10).height(10), text(clip(description, 30)).size(11).color(fg).wrapping(none), text(clip(kind.as_deref().unwrap_or_default(), 16)).size(9).color(dim).wrapping(none)]
                    .spacing(5)
                    .align_y(Alignment::Center),
                status,
            ]
            .spacing(1);
            if !doing.is_empty() {
                body = body.push(container(text(clip(&doing, 40)).size(10).font(mono).color(dim).wrapping(none)).clip(true));
            }
            let on = i == self.ai.cur && self.ai.thread.as_deref() == Some(s.id.as_str());
            let edge = if on { accent } else { Color::TRANSPARENT };
            // A rule down its left: it belongs to the agent above.
            let rule = container(space::horizontal().width(0)).width(2).height(Length::Fill).style(move |_| container::Style::default().background(Color { a: 0.5, ..color }));
            let row = row![rule, container(body).padding([3, 8]).width(Length::Fill)].height(Length::Shrink);
            let b = button(row)
                .on_press(Message::Ai(Msg::SelectSub(i, s.id.clone())))
                .padding(Padding { left: 16.0 + 12.0 * depth as f32, right: 6.0, top: 1.0, bottom: 1.0 })
                .width(Length::Fill)
                .style(move |_, _| button::Style { background: on.then(|| cur_bg.into()), border: Border { color: edge, width: 1.0, radius: 2.0.into() }, ..Default::default() });
            col = col.push(b);
        }
        Some(col.padding(Padding { bottom: 4.0, ..Padding::ZERO }).into())
    }

    /// Above a sub-agent's thread: whose it is, and the way back to the whole chat.
    fn ai_thread_bar(&self, id: &str) -> Element<'_, Message> {
        let th = &self.theme;
        let (fg, dim, accent, panel, divider) = (th.foreground, th.dim, th.accent, th.panel, th.divider);
        let c: &Chat = &self.ai;
        let step = c.turns.iter().filter_map(|t| t.step.as_ref()).find(|s| s.id == id);
        let what = step.and_then(|s| match &s.kind {
            AiStepKind::Agent { description, .. } => Some(description.clone()),
            _ => None,
        });
        let (state, tone) = step.map(|s| sub_state(c, s)).unwrap_or(("gone", Tone::Idle));
        let name = agent_name(c).unwrap_or_else(|| self.ai_name().to_string());
        let back = button(text(format!("← all of {name}")).size(11).color(accent)).on_press(Message::Ai(Msg::CloseThread)).padding([2, 6]).style(move |_, _| button::Style {
            border: Border { color: accent, width: 1.0, radius: 3.0.into() },
            ..Default::default()
        });
        let bar = row![
            back,
            svg(self.icons.network.clone()).width(12).height(12),
            text(format!("sub-agent: {}", what.unwrap_or_default())).size(12).color(fg),
            text(state).size(11).color(self.ai_tone(tone)),
            space::horizontal(),
            text("esc: back · it can't be written to: messages go to its agent").size(10).color(dim),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        container(bar)
            .padding([5, 10])
            .width(Length::Fill)
            .style(move |_| container::Style { background: Some(panel.into()), border: Border { color: divider, width: 1.0, radius: 2.0.into() }, ..Default::default() })
            .into()
    }

    /// The lines an edit took out and put in (the first few of each).
    fn ai_diff<'a>(&self, old: &'a str, new: &'a str) -> Element<'a, Message> {
        const MOST: usize = 10;
        let mono = self.face.font;
        let divider = self.theme.divider;
        let mut col = column![];
        let side = |lines: &'a str, sign: &'static str, color: Color, bg: Color, mut col: iced::widget::Column<'a, Message>| {
            let total = lines.lines().count();
            for l in lines.lines().take(MOST) {
                let t = text(format!("{sign} {l}")).size(11).font(mono).color(color).wrapping(text::Wrapping::None);
                col = col.push(container(t).width(Length::Fill).clip(true).style(move |_| container::Style::default().background(bg)));
            }
            if total > MOST {
                col = col.push(text(format!("{sign} … {} more", total - MOST)).size(11).font(mono).color(color));
            }
            col
        };
        col = side(old, "-", Color::from_rgb8(0xee, 0x99, 0x99), Color::from_rgb8(0x2a, 0x12, 0x15), col);
        col = side(new, "+", Color::from_rgb8(0x99, 0xdd, 0x99), Color::from_rgb8(0x11, 0x26, 0x1a), col);
        container(col).width(Length::Fill).style(move |_| container::Style { border: Border { color: divider, width: 1.0, radius: 2.0.into() }, ..Default::default() }).into()
    }

    /// Two lines of what an agent is looking at right now, for its row in the list.
    fn ai_preview(&self, c: &Chat) -> String {
        if let Some(s) = running(c) {
            return step_preview(s);
        }
        if let Session::Failed(why) = &c.session {
            return clip(first_line(why), 76);
        }
        // Idle: the end of what it last said.
        let last = c.turns.iter().rev().find(|t| t.who == Who::Assistant).map(|t| t.text.trim()).unwrap_or("");
        clip(&hide_images(last.lines().last().unwrap_or("")), 76)
    }

    fn ai_rail(&self) -> Element<'_, Message> {
        let ai = &self.ai;
        let th = &self.theme;
        let (fg, dim, mono) = (th.foreground, th.dim, self.face.font);
        let (cur_bg, accent, divider, panel) = (th.current_line, th.accent, th.divider, th.panel);
        let none = text::Wrapping::None;
        let mut list = column![];
        for (i, c) in ai.chats.iter().enumerate() {
            let (state, tone) = state_of(c);
            let color = self.ai_tone(tone);
            let name = c.agent.map_or("new agent", AiAgent::name);
            // Its task name leads once it has one, the assistant beside it, small. Until then: the
            // assistant, and what the others call it for now (or the model, before it starts).
            let (name, model) = match &c.name {
                Some(task) => (task.as_str(), name.to_string()),
                None => (name, agent_name(c).unwrap_or_else(|| c.chosen.clone().or_else(|| c.model.clone()).unwrap_or_default())),
            };
            let task = c.turns.iter().find(|t| t.who == Who::User).map(|t| clip(first_line(&t.text), 34)).unwrap_or_default();
            let clock = running(c).and_then(step_clock).map(|(clock, down, _)| text(clock).size(11).font(mono).color(if down { th.warn } else { dim }));
            let mut status = row![text(state).size(11).color(color).wrapping(none), space::horizontal()].align_y(Alignment::Center);
            if let Some(clock) = clock {
                status = status.push(clock);
            }
            let preview = container(text(self.ai_preview(c)).size(10).font(mono).color(dim).wrapping(none))
                .padding([2, 6])
                .width(Length::Fill)
                .height(34)
                .clip(true)
                .style(move |_| container::Style { background: Some(panel.into()), border: Border { color: divider, width: 1.0, radius: 2.0.into() }, ..Default::default() });
            let body = column![
                row![text("●").size(9).color(color), text(name).size(12).color(fg), text(clip(&model, 20)).size(10).color(dim).wrapping(none)].spacing(6).align_y(Alignment::Center),
                text(task).size(11).color(dim).wrapping(none),
                status,
                preview,
            ]
            .spacing(3);
            let on = i == ai.cur;
            // Done by its own word, waiting for the user to accept: green.
            let done = c.finished.is_some();
            let edge = if done { OK } else if on { accent } else { Color::TRANSPARENT };
            let b = button(body).on_press(Message::Ai(Msg::Select(i))).padding([7, 10]).width(Length::Fill).style(move |_, _| button::Style {
                background: on.then(|| cur_bg.into()),
                border: Border { color: edge, width: if done { 2.0 } else { 1.0 }, radius: 2.0.into() },
                ..Default::default()
            });
            list = list.push(b);
            if let Some(subs) = self.ai_sub_rows(i, c) {
                list = list.push(subs);
            }
            list = list.push(self.hrule());
        }
        let mut new = row![text("+ new agent").size(11).color(dim), space::horizontal()].align_y(Alignment::Center);
        if let Some(k) = self.keymap.chords_for(Command::NewAgent).first() {
            new = new.push(self.chord_caps(k, false, true));
        }
        let new = button(new).on_press(Message::Ai(Msg::NewAgent)).padding([7, 10]).width(Length::Fill).style(|_, _| button::Style::default());
        // Past conversations: their own tab.
        let mut past = row![text("history").size(11).color(dim), space::horizontal()].align_y(Alignment::Center);
        if let Some(k) = self.keymap.chords_for(Command::AgentHistory).first() {
            past = past.push(self.chord_caps(k, false, true));
        }
        let past = button(past).on_press(Message::Ai(Msg::History)).padding([7, 10]).width(Length::Fill).style(|_, _| button::Style::default());
        // The mailbox, under the agents: always its full height, newest at the bottom, scrolling
        // once there is more than fits. The agents above grow down and scroll when they meet it.
        let mut mail = column![text("MAILBOX").size(10).color(dim)].spacing(4).padding([6, 10]).height(MAIL);
        if ai.mailbox.is_empty() {
            mail = mail.push(text("Agents working here at once talk through it. Nothing yet.").size(10).color(dim));
        }
        // Each one clipped; click one to read it in full (and again to clip it).
        let first = ai.mail_count - ai.mailbox.len() as u64;
        let mut msgs = column![].spacing(4).padding(Padding { right: 8.0, ..Padding::ZERO });
        for (i, m) in ai.mailbox.iter().enumerate() {
            let n = first + i as u64;
            let full = m.text.trim();
            let open = ai.mail_open.contains(&n);
            let long = full.chars().count() > 150;
            let mut head = row![text(format!("{} → {}", m.from, m.to)).size(10).font(mono).color(accent), space::horizontal()].align_y(Alignment::Center);
            if long {
                head = head.push(text(if open { "less" } else { "more" }).size(9).color(dim));
            }
            let body = text(if open { full.to_string() } else { clip(full, 150) }).size(10).color(fg);
            let item: Element<'_, Message> = column![head, body].spacing(1).width(Length::Fill).into();
            msgs = msgs.push(if long { mouse_area(item).on_press(Message::Ai(Msg::MailOpen(n))).interaction(iced::mouse::Interaction::Pointer).into() } else { item });
        }
        mail = mail.push(scrollable(msgs).style(crate::theme::scrollbar).anchor_bottom().height(Length::Fill));
        // History, at the very bottom.
        let agents = scrollable(column![list, new]).style(crate::theme::scrollbar).height(Length::Fill);
        container(column![agents, self.hrule(), mail, self.hrule(), past]).width(RAIL).height(Length::Fill).into()
    }

    /// The selected agent's terminal: every command it ran and what came back. Read-only.
    fn ai_terminal(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let (dim, mono, size) = (th.dim, self.face.font, self.face.size.min(13.0));
        let mut col = column![].spacing(2).padding(Padding { left: 10.0, right: 10.0, top: 8.0, bottom: 8.0 });
        for s in self.ai.turns.iter().filter_map(|t| t.step.as_ref()) {
            let AiStepKind::Command { command, .. } = &s.kind else { continue };
            let mut spans: Spans<'static> = vec![span("➜ ").color(OK)];
            spans.extend(self.ai_command_spans(command, Color::WHITE));
            col = col.push(rich_text(spans).size(size).font(mono));
            match (&s.done, step_clock(s)) {
                (None, Some((clock, down, note))) => {
                    let (mark, color) = if down { ("◷", th.warn) } else { ("…", dim) };
                    col = col.push(text(format!("{mark} {clock} {note}")).size(size).font(mono).color(color));
                }
                (Some(d), _) => {
                    if !d.output.is_empty() {
                        let fg = th.terminal_fg;
                        let spans: Spans<'_> = ink::output(&d.output).into_iter().map(|(t, k)| span(t).color(self.ai_ink(k, fg))).collect();
                        col = col.push(rich_text(spans).size(size).font(mono));
                    }
                    if !d.ok {
                        col = col.push(text(if d.output.is_empty() { "■ stopped" } else { "✗ failed" }).size(size).font(mono).color(BAD));
                    }
                }
                _ => {}
            }
            col = col.push(space().height(4));
        }
        let title = self.ai_pane_title("read-only: what it ran and what came back".into());
        let bg = th.terminal_bg;
        let body = scrollable(col).style(crate::theme::scrollbar).id(WidgetId::from(TERMINAL_ID)).anchor_bottom().width(Length::Fill).height(Length::Fill);
        container(column![title, self.hrule(), body]).style(move |_| container::Style::default().background(bg)).width(Length::Fill).height(Length::Fill).into()
    }

    /// Everything the agent can load, in the `/` menu's groups, each in its colour: a heading
    /// with a rule under it, then its skills. A description is cut to the width there is; the
    /// whole of it shows when the pointer rests on the row.
    fn ai_drawer(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let (dim, fg, panel, divider, mono) = (th.dim, th.foreground, th.panel, th.divider, self.face.font);
        let skills = self.ai_skills();
        let mut groups: Vec<(String, Color, Vec<(String, String)>)> = Vec::new();
        for g in [SkillGroup::Provider, SkillGroup::Nib, SkillGroup::User, SkillGroup::Repo] {
            groups.push((g.label().to_string(), th.skill_group(g), skills.iter().filter(|s| s.group == g).map(|s| (s.name.clone(), s.description.clone())).collect()));
        }
        groups.push(("commands".into(), th.skill_command, compose::COMMANDS.iter().map(|c| (format!("/{}", c.name), c.about.to_string())).collect()));
        if self.ai.agent == Some(AiAgent::Claude) {
            groups.push(("tools".into(), th.skill_tool, TOOLS.iter().map(|(n, a)| (n.to_string(), a.to_string())).collect()));
        }
        groups.retain(|(_, _, rows)| !rows.is_empty());
        let title = self.ai_pane_title(format!("what {} can load", self.ai_name()));
        let list = responsive(move |size| {
            const NAME: f32 = 190.0;
            let none = text::Wrapping::None;
            // Roughly what fits beside the name at this width (the text is cut again at the edge).
            let fits = (((size.width - NAME - 46.0) / 6.9).max(8.0)) as usize;
            let mut col = column![].spacing(2).padding(Padding { left: 12.0, right: 12.0, top: 4.0, bottom: 10.0 });
            for (name, color, rows) in &groups {
                let (color, n) = (*color, rows.len());
                let rule = Color { a: 0.55, ..color };
                col = col
                    .push(space().height(12))
                    .push(row![text(name.to_uppercase()).size(13).color(color), text(n.to_string()).size(11).color(dim)].spacing(8).align_y(Alignment::End))
                    .push(container(space()).height(1).width(Length::Fill).style(move |_| container::Style::default().background(rule)))
                    .push(space().height(3));
                for (skill, about) in rows {
                    let line = row![
                        container(text(skill.clone()).size(11).font(mono).color(color).wrapping(none)).width(NAME).clip(true),
                        container(text(clip(about, fits)).size(11).color(dim).wrapping(none)).width(Length::Fill).clip(true),
                    ]
                    .spacing(10);
                    let tip = container(column![text(skill.clone()).size(11).font(mono).color(color), text(about.clone()).size(11).color(fg)].spacing(4))
                        .padding(8)
                        .max_width(440)
                        .style(move |_| container::Style { background: Some(panel.into()), border: Border { color: divider, width: 1.0, radius: 3.0.into() }, ..Default::default() });
                    col = col.push(tooltip(line, tip, tooltip::Position::FollowCursor));
                }
            }
            scrollable(col).style(crate::theme::scrollbar).width(Length::Fill).height(Length::Fill).into()
        });
        column![title, self.hrule(), list].into()
    }

    /// What the pane beside the steps is, over it.
    fn ai_pane_title(&self, about: String) -> Element<'_, Message> {
        container(text(about).size(11).color(self.theme.dim)).padding([4, 8]).into()
    }

    /// The header's switches for the pane beside the steps: the agent's terminal, its skills or
    /// its system prompt, the one showing lit. Pressing the lit one closes the pane and the chat
    /// takes its width. The prompt's switch is marked when there is one.
    fn ai_side_tabs(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let (fg, dim, accent, divider) = (th.foreground, th.dim, th.accent, th.divider);
        let tab = |side: Side| {
            let on = self.ai.drawer == Some(side);
            let msg = Msg::Side(if on { None } else { Some(side) });
            let set = side == Side::Prompt && !self.ai.prompt.text().trim().is_empty();
            let label = if set { format!("{} •", side.name()) } else { side.name().to_string() };
            button(text(label).size(11).color(if on { fg } else { dim })).on_press(Message::Ai(msg)).padding([1, 6]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: if on { accent } else { divider }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            })
        };
        row![tab(Side::Terminal), tab(Side::Skills), tab(Side::Prompt), self.ai_wpf_switch()].spacing(8).align_y(Alignment::Center).into()
    }

    /// WPF: worktree-per-feature, filled while on. A switch, not a pane: the fill sets
    /// it apart from the three tabs.
    fn ai_wpf_switch(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let (fg, dim, accent, divider, panel) = (th.foreground, th.dim, th.accent, th.divider, th.panel);
        let on = self.ai.wpf;
        let b = button(text("WPF").size(11).color(if on { fg } else { dim }))
            .on_press(Message::Ai(Msg::Wpf(!on)))
            .padding([1, 6])
            .style(move |_: &iced::Theme, _: button::Status| button::Style {
                background: on.then(|| Color { a: 0.18, ..accent }.into()),
                border: Border { color: if on { accent } else { divider }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            });
        let tip = container(text("worktree-per-feature: each agent that changes files does it on a branch of its own, landed on yours as one commit when you accept").size(11).color(fg))
            .padding([4, 8])
            .max_width(320)
            .style(move |_| container::Style { background: Some(panel.into()), border: Border { color: divider, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        tooltip(b, tip, tooltip::Position::Bottom).into()
    }

    /// The system prompt pane: what the agent's sessions add to its assistant's own, to edit.
    /// It is taken when a session starts, so a change to a running agent waits for the next.
    fn ai_prompt_pane(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let (dim, fg, divider, accent, sel) = (th.dim, th.foreground, th.divider, th.accent, th.selection);
        let who = self.ai_name();
        let title = self.ai_pane_title(format!("system prompt · added to {who}'s own"));
        let editor = iced::widget::text_editor(&self.ai.prompt)
            .placeholder("Instructions every message of this agent gets, e.g. “Answer tersely. Never commit.”")
            .on_action(|a| Message::Ai(Msg::Prompt(a)))
            .font(self.face.font)
            .size(12)
            .padding(8)
            .height(Length::Fill)
            .style(move |_: &iced::Theme, status| {
                let focused = matches!(status, iced::widget::text_editor::Status::Focused { .. });
                iced::widget::text_editor::Style {
                    background: Color::TRANSPARENT.into(),
                    border: Border { color: if focused { accent } else { divider }, width: 1.0, radius: 3.0.into() },
                    placeholder: dim,
                    value: fg,
                    selection: sel,
                }
            });
        let note = match (self.ai_tuning_fixed(), self.ai.agent) {
            (true, _) => "This agent is under way: a change applies from its next session (clear starts one).",
            (false, Some(AiAgent::Grok)) => "Grok has no system prompt: this goes at the top of the first message.",
            _ => "Used from the first message; new agents start with the last one used.",
        };
        column![title, self.hrule(), container(editor).padding(8).height(Length::Fill), container(text(note).size(11).color(dim)).padding(Padding { left: 10.0, right: 10.0, top: 0.0, bottom: 8.0 })].into()
    }

    /// The thinking dropdown: the levels the shown agent's assistant has, and its own default.
    /// `None` when it has none.
    fn ai_effort_pick(&self) -> Option<Element<'_, Message>> {
        let levels = self.ai_efforts();
        if levels.is_empty() {
            return None;
        }
        let th = &self.theme;
        let (fg, dim, panel, divider, sel) = (th.foreground, th.dim, th.panel, th.divider, th.selection);
        // Default says the level it turned out to be, once the session has said.
        let running = self.ai.running_effort.clone();
        let options: Vec<Effort> = std::iter::once(Effort::Default(running.clone())).chain(levels.iter().map(|l| Effort::Level(l.to_string()))).collect();
        // One picked for another assistant shows as the default, which is what this one gets.
        let current = Effort::of(&self.ai.effort.clone().filter(|e| levels.contains(e)), running);
        let pick = pick_list(options, Some(current), |e| Message::Ai(Msg::Effort(e)))
            .text_size(11)
            .padding([2, 8])
            .style(move |_: &iced::Theme, _| pick_list::Style {
                text_color: fg,
                placeholder_color: dim,
                handle_color: dim,
                background: Color::TRANSPARENT.into(),
                border: Border { color: divider, width: 1.0, radius: 3.0.into() },
            })
            .menu_style(move |_: &iced::Theme| iced::overlay::menu::Style {
                background: panel.into(),
                border: Border { color: divider, width: 1.0, radius: 3.0.into() },
                text_color: fg,
                selected_text_color: Color::WHITE,
                selected_background: sel.into(),
                shadow: Default::default(),
            });
        Some(row![text("thinking").size(11).color(dim), pick].spacing(8).align_y(Alignment::Center).into())
    }

    fn ai_header(&self) -> Element<'_, Message> {
        let ai = &self.ai;
        let th = &self.theme;
        let (dim, fg) = (th.dim, th.foreground);
        let plain = |_: &iced::Theme, _: button::Status| button::Style::default();
        let key = |label: String, color: Color, msg: Msg| {
            button(text(label).size(11).color(color)).on_press(Message::Ai(msg)).padding([1, 6]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: th.divider, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            })
        };
        let mut head = row![].spacing(8).align_y(Alignment::Center).padding(Padding { left: 8.0, right: 8.0, top: 5.0, bottom: 4.0 });
        // Folded: where this agent is among the others, and the way back to the list.
        if ai.fold {
            let n = ai.chats.len();
            head = head
                .push(key("▸ agents".into(), dim, Msg::Fold))
                .push(key("‹".into(), dim, Msg::Select((ai.cur + n - 1) % n)))
                .push(text(format!("{} of {n}", ai.cur + 1)).size(11).color(dim))
                .push(key("›".into(), dim, Msg::Select((ai.cur + 1) % n)));
        } else {
            head = head.push(key("◂".into(), dim, Msg::Fold));
        }
        let (state, tone) = state_of(ai);
        let arrow = if ai.picking { "▴" } else { "▾" };
        let model = if ai.agent.is_some() { self.ai_model_label() } else { String::new() };
        let picked = row![text("●").size(9).color(self.ai_tone(tone)), text(self.ai_name()).size(12).color(fg), text(model).size(11).color(dim), text(arrow).size(11).color(dim)]
            .spacing(8)
            .align_y(Alignment::Center);
        let switch = button(picked).on_press(Message::Ai(Msg::Switch)).padding([2, 6]).style(move |_: &iced::Theme, status: button::Status| {
            let hot = ai.picking || matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style { border: Border { color: if hot { th.divider } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() }, ..Default::default() }
        });
        head = head.push(switch).push(text(state).size(11).color(self.ai_tone(tone))).push(space::horizontal());
        // How much it may do unasked: one of four, the current one lit.
        let (panel, divider, sel) = (th.panel, th.divider, th.selection);
        let modes = pick_list(AiMode::ALL, Some(ai.mode), |m| Message::Ai(Msg::Mode(m)))
            .text_size(11)
            .padding([2, 8])
            .style(move |_: &iced::Theme, _| pick_list::Style {
                text_color: fg,
                placeholder_color: dim,
                handle_color: dim,
                background: Color::TRANSPARENT.into(),
                border: Border { color: divider, width: 1.0, radius: 3.0.into() },
            })
            .menu_style(move |_: &iced::Theme| iced::overlay::menu::Style {
                background: panel.into(),
                border: Border { color: divider, width: 1.0, radius: 3.0.into() },
                text_color: fg,
                selected_text_color: Color::WHITE,
                selected_background: sel.into(),
                shadow: Default::default(),
            });
        head = head.push(self.ai_side_tabs());
        if let Some(effort) = self.ai_effort_pick() {
            head = head.push(effort);
        }
        // Grok runs with read-only tools whatever the mode: say so instead of offering a choice
        // that does nothing.
        if ai.agent == Some(AiAgent::Grok) {
            head = head.push(text("mode").size(11).color(dim)).push(text("read-only").size(11).color(fg));
        } else {
            head = head.push(text("mode").size(11).color(dim)).push(modes);
        }
        let following = self.ai_following() == Some(ai.key);
        head = head.push(key(if following { "◉ following".into() } else { "◉ follow".into() }, if following { th.accent } else { dim }, Msg::Follow));
        if ai.busy && matches!(ai.session, Session::Ready(_)) {
            head = head.push(key("stop".into(), th.warn, Msg::Stop));
        }
        if !ai.turns.is_empty() {
            head = head.push(button(text("clear").size(11).color(dim)).on_press(Message::Ai(Msg::NewChat)).padding([1, 6]).style(plain));
        }
        head = head.push(key("kill".into(), BAD, Msg::Kill(ai.cur)));
        head.into()
    }

    /// The questions the shown agent asks, as a form: each with its options to pick (a click or
    /// its number), the preview of the one the arrows are on, and what the user typed instead.
    fn ai_questions<'a>(&'a self, ask: &'a nib_proto::AiAsk) -> Element<'a, Message> {
        let th = &self.theme;
        let (accent, dim, fg, warn, sel_bg) = (th.accent, th.dim, th.foreground, th.warn, th.selection);
        let form = &self.ai.form;
        let many = ask.questions.len() > 1;
        let title = if many { format!("{} questions for you", ask.questions.len()) } else { "Question for you".to_string() };
        let mut card = column![text(title).size(12).color(warn)].spacing(8);
        for (q, question) in ask.questions.iter().enumerate() {
            let on = q == form.q;
            // With several, the one the keys work on is marked.
            let mut head = row![].spacing(6).align_y(Alignment::Center);
            if many {
                head = head.push(text(if on { "▸" } else { " " }).size(12).color(accent).font(self.face.font));
            }
            if !question.header.is_empty() {
                let chip = container(text(question.header.as_str()).size(10).color(accent)).padding([1, 6]).style(move |_| container::Style {
                    border: Border { color: accent, width: 1.0, radius: 8.0.into() },
                    ..Default::default()
                });
                head = head.push(chip);
            }
            head = head.push(text(question.question.as_str()).size(13).color(fg).font(Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT }));
            if question.multi {
                head = head.push(text("pick any").size(10).color(dim));
            }
            let mut block = column![head].spacing(4);
            for (o, option) in question.options.iter().enumerate() {
                let picked = form.picks.get(q).is_some_and(|p| p.contains(&o));
                let lit = on && o == form.lit;
                let tick = match (question.multi, picked) {
                    (true, true) => "☑",
                    (true, false) => "☐",
                    (false, true) => "●",
                    (false, false) => "○",
                };
                let mut words = column![text(option.label.as_str()).size(12).color(if picked { accent } else { fg })];
                if !option.description.is_empty() {
                    words = words.push(text(option.description.as_str()).size(11).color(dim));
                }
                let number = text(if o < 9 { format!("{}", o + 1) } else { " ".into() }).size(11).color(dim).font(self.face.font);
                let line = row![number, text(tick).size(12).color(if picked { accent } else { dim }), words].spacing(8).align_y(Alignment::Start);
                let b = button(line).width(Length::Fill).padding([3, 8]).on_press(Message::Ai(Msg::Pick(q, o))).style(move |_: &iced::Theme, status: button::Status| button::Style {
                    background: (lit || matches!(status, button::Status::Hovered)).then_some(Background::Color(sel_bg)),
                    border: Border { color: if picked { accent } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                });
                block = block.push(b);
            }
            if let Some(own) = form.own.get(q).and_then(Option::as_deref) {
                block = block.push(row![text("✎").size(12).color(accent), text(own).size(12).color(accent)].spacing(8).padding([0, 8]));
            } else if on {
                block = block.push(text("…or type your own answer below").size(11).color(dim));
            }
            let preview = on.then(|| question.options.get(form.lit).and_then(|o| o.preview.as_deref())).flatten();
            if let Some(p) = preview {
                block = block.push(container(text(p).size(12).font(self.face.font).color(fg)).padding(8).width(Length::Fill).style(move |_| container::Style {
                    border: Border { color: dim, width: 1.0, radius: 3.0.into() },
                    ..Default::default()
                }));
            }
            card = card.push(block);
        }
        let ready = form.complete();
        let send: Element<'a, Message> = if ready {
            answer_key("send · ↵", OK, Msg::Answer(true)).into()
        } else {
            text(if many { "answer each to send" } else { "pick one" }).size(11).color(dim).into()
        };
        let hint = if many { "1–9 pick · ↑↓ · space · tab next question" } else { "1–9 pick · ↑↓ · space" };
        card = card.push(row![send, answer_key("skip · esc", dim, Msg::Answer(false)), text(hint).size(11).color(dim)].spacing(12).align_y(Alignment::Center));
        container(card).padding(10).width(Length::Fill).style(move |_| container::Style { border: Border { color: warn, width: 1.0, radius: 3.0.into() }, ..Default::default() }).into()
    }

    /// How plans and the comments on them are drawn here.
    fn ai_plan_look(&self) -> plan::Look {
        plan::Look::of(&self.theme, self.face.font, self.face.size.min(14.0))
    }

    /// A plan the shown agent handed over. The one it is waiting on (`ok` is `None` and it is
    /// asked about) can be commented on and answered; any other is a record, folded to its
    /// title, of what became of it. `ok`: how the step that carried it ended, if it has.
    fn ai_plan_card<'a>(&'a self, key: &'a str, ok: Option<bool>) -> Element<'a, Message> {
        let ai = &self.ai;
        let Some(p) = ai.plans.get(key) else { return space().into() };
        let th = &self.theme;
        let (accent, dim, fg, panel) = (th.accent, th.dim, th.foreground, th.panel);
        let live = ai.asked_plan().is_some_and(|k| k == key);
        let look = self.ai_plan_look();
        let small = |t: String, c: Color| text(t).size(11).color(c);
        let n = p.comments.len();
        let comments = match n {
            0 => String::new(),
            1 => "1 comment".into(),
            n => format!("{n} comments"),
        };
        let on = move |m: PlanMsg| Message::Ai(Msg::Plan(key.to_string(), m));
        let bold = Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT };
        if !live {
            // A record: what it was called, what became of it, unfolded on a click.
            let (word, c) = match p.outcome.or(match ok {
                Some(true) => Some(Outcome::Approved),
                Some(false) => Some(Outcome::Revise),
                None => None,
            }) {
                Some(Outcome::Approved) => ("approved", OK),
                Some(Outcome::Revise) => ("sent back", th.warn),
                Some(Outcome::Declined) => ("declined", BAD),
                None => ("not answered", dim),
            };
            let fold = if p.open { "▾" } else { "▸" };
            let head = button(
                row![small(fold.into(), dim), text("Plan").size(12).font(bold).color(fg), container(small(p.title_text(), fg).wrapping(text::Wrapping::None)).width(Length::Fill).clip(true), small(comments, dim), small(word.into(), c)]
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .on_press(on(PlanMsg::Fold))
            .padding(0)
            .style(|_: &iced::Theme, _: button::Status| button::Style::default());
            let mut body = column![head].spacing(10);
            if p.open {
                body = body.push(plan::view(p, &look, false, on));
            }
            let edge = c;
            return container(body).padding([6, 10]).width(Length::Fill).style(move |_| container::Style { border: Border { color: Color { a: 0.5, ..edge }, width: 1.0, radius: 6.0.into() }, ..Default::default() }).into();
        }
        // Waiting for the user: the whole plan, rendered, then how to answer it.
        let head = row![text("Plan for your approval").size(12).font(bold).color(th.warn), space::horizontal(), small(comments, accent)].align_y(Alignment::Center);
        let typed = !ai.input.trim().is_empty();
        // A comment being written is filed with the rest.
        let n = n + usize::from(p.comments.draft.as_ref().is_some_and(|d| !d.blank()));
        let plural = if n == 1 { "" } else { "s" };
        // Two ways it can end: approved, or sent back with what the user said. ↵ does whichever
        // applies; the other is a click away.
        let mut keys = row![].spacing(12).align_y(Alignment::Center);
        if n > 0 || typed {
            let label = match (n, typed) {
                (0, _) => "send back with your reply · ↵".to_string(),
                (n, true) => format!("send back with {n} comment{plural} + your reply · ↵"),
                (n, false) => format!("send back with {n} comment{plural} · ↵"),
            };
            keys = keys.push(answer_key(label, th.warn, Msg::Revise));
            keys = keys.push(answer_key("approve anyway", dim, Msg::Answer(true)));
        } else {
            keys = keys.push(answer_key("approve · ↵", OK, Msg::Answer(true)));
            keys = keys.push(button(text("send back").size(11).color(dim)).padding([2, 8]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: Color { a: 0.5, ..dim }, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            }));
            keys = keys.push(small("comment on rows or type a reply to send it back".into(), dim));
        }
        let hint = if p.comments.draft.is_some() { "" } else { "↑↓ select a row · ⇧↑↓ several · ↵ comment on them · type below to reply to the whole plan" };
        let warn = th.warn;
        let card = column![head, plan::view(p, &look, true, on), container(space().height(1)).width(Length::Fill).style(move |_| container::Style { background: Some(Color { a: 0.4, ..dim }.into()), ..Default::default() }), keys, small(hint.into(), dim)].spacing(10);
        container(card).padding(Padding { left: 6.0, right: 12.0, top: 10.0, bottom: 10.0 }).width(Length::Fill).style(move |_| container::Style {
            background: Some(Color { a: 0.35, ..panel }.into()),
            border: Border { color: warn, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        }).into()
    }

    fn ai_feed<'a>(&'a self) -> Element<'a, Message> {
        let ai = &self.ai;
        let th = &self.theme;
        let (accent, dim, fg, sel_bg) = (th.accent, th.dim, th.foreground, th.selection);
        let (panel, divider) = (th.panel, th.divider);
        let mut style = markdown::Style::from_palette(iced::Theme::Dark.palette());
        style.link_color = accent;
        style.inline_code_color = th.scope("string");
        style.inline_code_font = self.face.font;
        style.code_block_font = self.face.font;
        let size = self.face.size.min(14.0);
        let settings = markdown::Settings::with_text_size(size, style);

        let mut feed = column![].spacing(10).padding(Padding { left: 14.0, right: 16.0, top: 10.0, bottom: 10.0 });
        // The text that can be selected, noted afresh as it is drawn (see `select`).
        ai.feed_sel.forget_blocks();
        let shown = ai.feed_sel.of(&self.ai_feed_owner()).is_some();
        // Selected text in the user's bubble, which is already the selection's colour.
        let bubble_sel = Color { a: 0.45, ..accent };
        let dropdown = ai.picking || ai.setup.is_some();
        if ai.turns.is_empty() && !dropdown {
            feed = feed.push(text(self.ai_intro()).size(12).color(dim));
        }
        // Who did each thing, in a narrow column on the left: the model's reply, a tool it ran,
        // a skill it loaded, a sub-agent it started, a message between agents, the finish.
        let mark = |icon: Element<'a, Message>, body: Element<'a, Message>| -> Element<'a, Message> {
            row![container(icon).width(Length::Fixed(MARK_WIDTH)).center_x(Length::Fixed(MARK_WIDTH)).padding(Padding { top: 2.0, ..Padding::ZERO }), body].spacing(4).into()
        };
        let emoji = |e: &'static str| -> Element<'a, Message> { text(e).size(12).into() };
        // A box's own copy button, in its bottom-right corner, under what it says.
        // `fill`: the box is as wide as the feed; the button goes to its right edge, not to
        // that of the text (a reply's text is only as wide as its longest line).
        let with_copy = |body: Element<'a, Message>, i: usize, fill: bool| -> Element<'a, Message> {
            let copy = button(text("📋").size(12))
                .on_press(Message::Ai(Msg::CopyTurn(i)))
                .padding([0, 4])
                .style(move |_: &iced::Theme, status: button::Status| button::Style {
                    border: Border { color: if matches!(status, button::Status::Hovered) { dim } else { Color::TRANSPARENT }, width: 1.0, radius: 3.0.into() },
                    ..Default::default()
                });
            let tip = container(text("copy").size(11).color(fg))
                .padding([2, 6])
                .style(move |_| container::Style { background: Some(panel.into()), border: Border { color: divider, width: 1.0, radius: 3.0.into() }, ..Default::default() });
            let copy = tooltip(copy, tip, tooltip::Position::Left);
            if fill {
                column![body, container(copy).align_right(Length::Fill)].spacing(2).width(Length::Fill).into()
            } else {
                column![body, copy].spacing(2).align_x(Alignment::End).into()
            }
        };
        // The sub-agent the steps just shown were under: where another's (or the agent's own)
        // start, a line says whose they are, since they come mixed in with the agent's own work.
        let mut under: Option<&str> = None;
        // One sub-agent's thread: only what is its, under a bar that leads back.
        let thread = ai.thread.as_deref();
        if let Some(id) = thread {
            feed = feed.push(self.ai_thread_bar(id));
        }
        for (i, t) in ai.turns.iter().enumerate() {
            if let Some(id) = thread {
                if !t.step.as_ref().is_some_and(|s| in_thread(ai, s, id)) {
                    continue;
                }
            }
            let parent = t.step.as_ref().and_then(|s| s.parent.as_deref());
            if thread.is_none() && t.who == Who::Step && parent != under {
                if let Some(p) = parent {
                    let helper = ai.turns.iter().filter_map(|t| t.step.as_ref()).find(|s| s.id == p);
                    let what = match helper.map(|h| &h.kind) {
                        Some(AiStepKind::Agent { description, .. }) => clip(description, 80),
                        _ => "a sub-agent".into(),
                    };
                    let state = match helper.and_then(|h| h.done.as_ref()) {
                        None if helper.is_some_and(|h| h.background) => "· in the background, its own steps:",
                        None => "· its own steps:",
                        Some(_) => "· its steps:",
                    };
                    let head = row![
                        svg(self.icons.network.clone()).width(11).height(11),
                        text(format!("helper: {what}")).size(11).font(self.face.font).color(fg),
                        text(state).size(11).color(dim),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center);
                    feed = feed.push(container(head).padding(Padding { left: MARK_WIDTH + 4.0, ..Padding::ZERO }));
                }
                under = parent;
            }
            let turn: Element<'a, Message> = match (t.who, &t.step) {
                // The plan being asked about shows at the end, with what to answer.
                (_, Some(s)) if ai.asked_plan() == Some(&s.id) => continue,
                (_, Some(s)) if ai.plans.contains_key(&s.id) => mark(emoji("📋"), self.ai_plan_card(&s.id, s.done.as_ref().map(|d| d.ok))),
                (_, Some(s)) => {
                    let icon = match s.kind {
                        AiStepKind::Agent { .. } => svg(self.icons.network.clone()).width(13).height(13).into(),
                        AiStepKind::Message { .. } => svg(self.icons.network.clone()).width(13).height(13).into(),
                        AiStepKind::Skill { .. } => emoji("📚"),
                        _ => emoji("🔧"),
                    };
                    let row = self.ai_step_row(s);
                    // A picture the agent read, once it has arrived.
                    let pic = match &s.kind {
                        AiStepKind::Read { path, .. } => t.images.get(path).and_then(Option::as_ref),
                        _ => None,
                    };
                    match pic {
                        Some(p) => mark(icon, column![row, container(crate::app::peek::peek(p.view(), p, self.ai.image_zoom_delay)).max_height(PICTURE_HEIGHT)].spacing(4).into()),
                        None => mark(icon, row),
                    }
                }
                (Who::Mail, _) => {
                    let accent = th.accent;
                    let blocks = Blocks::new(&ai.feed_sel, shown, i, sel_bg);
                    let card = container(with_copy(plain_block(&blocks, &t.text, 12.0, fg, Length::Shrink), i, true))
                        .padding([4, 10])
                        .width(Length::Fill)
                        .style(move |_| container::Style { border: Border { color: accent, width: 1.0, radius: 3.0.into() }, ..Default::default() });
                    mark(emoji("✉️"), card.into())
                }
                (Who::Done, _) => {
                    let mut card = column![text("Finished").size(12).color(OK)].spacing(4);
                    let blocks = Blocks::new(&ai.feed_sel, shown, i, sel_bg);
                    for line in t.text.lines() {
                        let (head, rest) = line.split_once(": ").unwrap_or(("", line));
                        let line = [Span::new(format!("{head}  ")).color(OK).font(Font { weight: iced::font::Weight::Bold, ..Font::DEFAULT }), Span::new(rest.to_string()).color(fg)];
                        card = card.push(blocks.block(&line, true).size(12.0));
                    }
                    let card = container(with_copy(card.into(), i, true)).padding([8, 10]).width(Length::Fill).style(move |_| container::Style {
                        background: Some(Color { a: 0.10, ..OK }.into()),
                        border: Border { color: OK, width: 1.0, radius: 6.0.into() },
                        ..Default::default()
                    });
                    mark(emoji("✅"), card.into())
                }
                (Who::Note, _) | (Who::Step, None) => container(text(t.text.as_str()).size(11).color(dim)).center_x(Length::Fill).into(),
                (Who::User, _) => {
                    let blocks = Blocks::new(&ai.feed_sel, shown, i, bubble_sel);
                    let bubble = container(with_copy(plain_block(&blocks, &t.text, size, fg, Length::Shrink), i, false)).padding([6, 10]).style(move |_| container::Style {
                        background: Some(sel_bg.into()),
                        border: Border { radius: 6.0.into(), ..Default::default() },
                        ..Default::default()
                    });
                    Element::from(row![space::horizontal().width(Length::FillPortion(1)), container(bubble).width(Length::FillPortion(5)).align_right(Length::Fill)])
                }
                (Who::Assistant, _) => {
                    let viewer = crate::app::code::MarkdownViewer {
                        font: self.face.font,
                        size,
                        color: fg,
                        math: Some(&t.math),
                        images: Some((&t.images, self.ai.image_zoom_delay)),
                        blocks: Some(Blocks::new(&ai.feed_sel, shown, i, sel_bg)),
                    };
                    // Outlined, on the feed's own background: the reply stands apart from the steps
                    // around it without competing with the user's filled bubble.
                    let reply = container(with_copy(markdown::view_with(t.md.items(), settings, &viewer), i, true)).padding([6, 10]).width(Length::Fill).style(move |_| container::Style {
                        border: Border { color: accent, width: 1.0, radius: 6.0.into() },
                        ..Default::default()
                    });
                    mark(emoji("🤖"), reply.into())
                }
            };
            feed = feed.push(self.search_outline(ai.key, i, turn));
        }
        if let Some(ask) = ai.ask.as_ref().filter(|a| !a.questions.is_empty()) {
            feed = feed.push(mark(emoji("❓"), self.ai_questions(ask)));
        } else if let Some(key) = ai.asked_plan() {
            feed = feed.push(mark(emoji("📋"), self.ai_plan_card(key, None)));
        } else if let Some(ask) = &ai.ask {
            let warn = th.warn;
            let title = match ask.tool.as_str() {
                super::PLAN_TOOL => "Plan to approve".to_string(),
                tool => format!("{} wants to use {tool}", self.ai_name()),
            };
            let mut card = column![text(title).size(12).color(warn), text(ask.what.as_str()).size(12).font(self.face.font).color(fg)].spacing(6);
            card = card.push(row![answer_key("allow · ↵", OK, Msg::Answer(true)), answer_key("refuse · esc", BAD, Msg::Answer(false)), text("or type what to do instead").size(11).color(dim)].spacing(12).align_y(Alignment::Center));
            let card = container(card).padding(10).width(Length::Fill).style(move |_| container::Style { border: Border { color: warn, width: 1.0, radius: 3.0.into() }, ..Default::default() });
            feed = feed.push(mark(emoji("✋"), card.into()));
        }
        let waiting = ai.busy && ai.ask.is_none() && running(ai).is_none() && !ai.turns.last().is_some_and(|t| t.who == Who::Assistant);
        // Only the model at work gets a mark; pauses and errors stay plain.
        let status = match &ai.session {
            _ if ai.frozen => Some(("Paused.".to_string(), th.warn, None)),
            Session::Starting => Some((format!("Starting {}…", self.ai_name()), dim, None)),
            Session::Failed(why) => Some((why.clone(), th.warn, None)),
            _ if ai.compacting => Some(("Compacting the chat…".to_string(), dim, Some("🧠"))),
            _ if waiting => Some(("Thinking…".to_string(), dim, Some("💭"))),
            _ => ai.error.as_ref().map(|e| (format!("The reply stopped: {e}"), th.warn, None)),
        };
        if let Some((s, color, icon)) = status {
            let line: Element<'a, Message> = text(s).size(12).color(color).into();
            feed = feed.push(match icon {
                Some(e) => mark(emoji(e), line),
                None => line,
            });
        }
        let feed = scrollable(feed).style(crate::theme::scrollbar).id(WidgetId::from(TRANSCRIPT_ID)).anchor_bottom().width(Length::Fill).height(Length::Fill);
        // The dropdown lies over the steps instead of pushing them down. A click beside it closes
        // it, unless it's all there is (nothing is ready yet, or a setup is running).
        if dropdown {
            let closable = ai.setup.is_none() && ai.agents.as_ref().is_some_and(|a| a.iter().any(|i| i.ready()));
            let backdrop = mouse_area(space().width(Length::Fill).height(Length::Fill));
            let backdrop = if closable { backdrop.on_press(Message::Ai(Msg::Switch)) } else { backdrop };
            let menu = container(scrollable(self.ai_picker()).style(crate::theme::scrollbar).height(Length::Shrink)).padding(Padding { left: 8.0, right: 8.0, top: 6.0, bottom: 8.0 }).max_width(560);
            stack![feed, backdrop, menu].into()
        } else {
            feed.into()
        }
    }

    /// A key in the AI stage; whatever it did to the message, its caret stays in view.
    pub(in crate::app) fn ai_key_in_view(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let before = (self.ai.input.len(), self.ai.caret());
        let task = self.ai_key(chord, text);
        if self.focus == Focus::Ai && (self.ai.input.len(), self.ai.caret()) != before {
            return Task::batch([task, self.ai_follow_caret()]);
        }
        task
    }

    /// Scroll the message box so the caret's line is in view. The box shows a window of the
    /// message; scrolled to the caret's share of the rows, the caret lands inside that window.
    /// Rows are guessed from the text's own lines, a long one counted as the rows it wraps to at
    /// a typical width: exact at the ends, close enough between them.
    pub(super) fn ai_follow_caret(&self) -> Task<Message> {
        const WRAP: usize = 70;
        let (s, i) = (self.ai.input.as_str(), self.ai.caret());
        let rows = |t: &str| t.split('\n').map(|l| l.chars().count().div_ceil(WRAP).max(1)).sum::<usize>();
        let total = rows(s);
        if total <= 1 {
            return Task::none();
        }
        // Rows above the caret's own, plus the part of its line before it.
        let above = rows(&s[..i]) - 1;
        let share = above as f32 / (total - 1) as f32;
        // Pinned to the bottom, offsets count up from the end.
        let y = 1.0 - share.clamp(0.0, 1.0);
        operate(operation::scrollable::snap_to(WidgetId::from(INPUT_ID), operation::scrollable::RelativeOffset { x: None, y: Some(y) }))
    }

    fn ai_footer(&self) -> Element<'_, Message> {
        let ai = &self.ai;
        let th = &self.theme;
        let (accent, dim, fg) = (th.accent, th.dim, th.foreground);
        let focused = self.focus == Focus::Ai && ai.panic.is_none();
        let size = self.face.size.min(14.0);
        let name = self.ai_name();
        // What a message does depends on what the agent is doing.
        let steers = ai.busy && ai.agent == Some(AiAgent::Claude);
        let caption = if let Some(p) = ai.asked_plan().and_then(|k| ai.plans.get(k)) {
            match p.comments.len() {
                _ if p.comments.draft.is_some() => "Writing a comment: ↵ files it · ⇧↵ new line · ⇧↑↓ more rows · esc drops it".to_string(),
                _ if p.sel.is_some() => "↵ comments on the selected rows · ⇧↑↓ selects more · esc lets go".to_string(),
                0 => "↵ approves · ↑↓ selects rows to comment on · or type a reply here to send it back…".to_string(),
                n => format!("↵ sends the plan back with {n} comment{} · type here to add a reply…", if n == 1 { "" } else { "s" }),
            }
        } else if let Some(ask) = &ai.ask {
            if ask.questions.is_empty() { "↵ allows, esc refuses, or say what to do instead…".to_string() } else { "Or answer in your own words…".to_string() }
        } else if steers {
            format!("Steer {name}: it reads this after its current step")
        } else if ai.busy {
            format!("{name} is replying…")
        } else if ai.thread.is_some() {
            format!("Message {name} (its sub-agent can't be written to)")
        } else if ai.turns.is_empty() {
            format!("Tell {name} what to do…")
        } else {
            format!("Message {name}: continues this conversation")
        };
        let field: Element<'_, Message> = if ai.input.is_empty() {
            let caret: Span<'_, (), Font> = span("▏").color(if focused { accent } else { Color::TRANSPARENT });
            rich_text([caret, span(caption).color(dim)]).size(size).width(Length::Fill).into()
        } else {
            // The caret is drawn on the text, so it stays beside its character wherever the text
            // wraps (a widget beside the text gets pushed to the right edge once it wraps). It's a
            // stripe on the left edge of the character after it, not a glyph of its own: a glyph
            // takes a cell and pushes the rest of the line over each time the caret moves.
            let (s, i) = (ai.input.as_str(), ai.caret());
            let (a, b) = ai.selection().unwrap_or((i, i));
            let sel = th.selection;
            fn run<'s>(t: &'s str, on: bool, fg: Color, sel: Color) -> Span<'s, (), Font> {
                let p = span(t).color(fg);
                if on { p.background(sel) } else { p }
            }
            let piece = |t, on| run(t, on, fg, sel);
            let bar = |on: bool| {
                let under = if on { sel } else { Color::TRANSPARENT };
                let g = Linear::new(std::f32::consts::FRAC_PI_2).add_stop(0.0, accent).add_stop(0.2, accent).add_stop(0.201, under).add_stop(1.0, under);
                Background::Gradient(g.into())
            };
            let mut spans = vec![piece(&s[..a], false)];
            let mut placed = !focused;
            for (lo, hi, on) in [(a, b, true), (b, s.len(), false)] {
                let t = &s[lo..hi];
                if t.is_empty() {
                    continue;
                }
                if placed || lo != i {
                    spans.push(piece(t, on));
                    continue;
                }
                placed = true;
                let n = next_char(t, 0);
                if &t[..n] == "\n" {
                    // End of a line: nothing after it to push over, and a newline has no width to draw on.
                    spans.push(span("▏").color(accent));
                    spans.push(piece(t, on));
                } else {
                    spans.push(piece(&t[..n], on).background(bar(on)));
                    spans.push(piece(&t[n..], on));
                }
            }
            if !placed {
                // At the very end of the text.
                spans.push(span("▏").color(accent));
            }
            rich_text(spans).size(size).width(Length::Fill).into()
        };
        // Taller than the box can show, the message scrolls inside it; `ai_follow_caret` keeps the
        // caret's line in view as it moves (pinned to the bottom, typing at the end needs nothing).
        let field = scrollable(container(field).padding([7, 10]).width(Length::Fill))
            .id(WidgetId::from(INPUT_ID))
            // The anchor lives in the scrollbar's settings: set after the direction, or it's lost.
            .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4).margin(1)))
            .anchor_bottom()
            .style(crate::theme::scrollbar)
            .width(Length::Fill)
            .height(Length::Shrink);
        let input = container(field).width(Length::Fill).max_height(INPUT_MAX_HEIGHT).clip(true).style(move |_| container::Style {
            border: Border { color: if focused { accent } else { th.divider }, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        });
        // Pictures pasted into the message: a chip each, a click takes it off.
        let mut pics = row![].spacing(6);
        for (i, p) in ai.images.iter().enumerate() {
            let label = text(format!("▣ image {} · {}×{}  ×", i + 1, p.width, p.height)).size(11).color(fg);
            pics = pics.push(button(label).on_press(Message::Ai(Msg::DropImage(i))).padding([2, 8]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: accent, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            }));
        }
        let input: Element<'_, Message> = if ai.images.is_empty() { input.into() } else { column![pics.wrap(), input].spacing(6).into() };
        let menu = self.ai_menu();
        let chord = |c: Command| self.keymap.chords_for(c).first().map(|k| k.to_string());
        let mut hints = row![].spacing(14);
        if menu.is_some() {
            hints = hints.push(self.hint("↑↓", "choose")).push(self.hint("⇥", "complete")).push(self.hint("esc", "close"));
        } else {
            hints = hints.push(self.hint("↵", if steers { "steer" } else { "send" })).push(self.hint("/", "skills")).push(self.hint("@", "files"));
            if ai.chats.len() > 1 {
                hints = hints.push(self.hint("⇥ ⇧⇥", "next / previous agent"));
            }
            if let Some(k) = chord(Command::KillAgent) {
                hints = hints.push(self.hint(&k, "kill"));
            }
            if let Some(k) = chord(Command::Panic) {
                hints = hints.push(self.hint(&k, "panic"));
            }
        }
        let footer = column![input, hints.wrap()].spacing(8);
        let footer = match menu {
            Some(menu) => column![menu, footer].spacing(6),
            None => footer,
        };
        let footer = match self.ai_meter() {
            Some(meter) => column![meter, footer].spacing(8),
            None => footer,
        };
        footer.padding(Padding { left: 12.0, right: 12.0, top: 8.0, bottom: 10.0 }).into()
    }

    /// Every agent is paused: which ones to stop.
    fn ai_panic_dialog(&self) -> Option<Element<'_, Message>> {
        let (rows, sel) = self.ai.panic.as_ref()?;
        let th = &self.theme;
        let (dim, fg, panel, cur_bg, mono) = (th.dim, th.foreground, th.panel, th.current_line, self.face.font);
        let stop = rows.iter().filter(|(_, m)| *m).count();
        let mut col = column![
            text("‖ Every agent is paused").size(14).color(BAD),
            text("Marked agents stop when you release. The others carry on from where they were.").size(11).color(dim),
            space().height(4),
        ]
        .spacing(4);
        for (i, (key, marked)) in rows.iter().enumerate() {
            let Some(c) = self.ai.chats.iter().find(|c| c.key == *key) else { continue };
            let doing = running(c).map(|s| self.ai_step_brief(s)).unwrap_or_else(|| "thinking".into());
            let line = row![
                text(if *marked { "[x]" } else { "[ ]" }).size(12).font(mono).color(if *marked { BAD } else { dim }),
                text(c.agent.map_or("agent", AiAgent::name)).size(12).color(fg),
                text(clip(&doing, 60)).size(11).font(mono).color(dim).wrapping(text::Wrapping::None),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            let on = i == *sel;
            col = col.push(button(line).on_press(Message::Ai(Msg::PanicMark(i))).padding([3, 6]).width(Length::Fill).style(move |_, _| button::Style {
                background: on.then(|| cur_bg.into()),
                ..Default::default()
            }));
        }
        let release = button(text(format!("release: stop {stop}, resume {}", rows.len() - stop)).size(12).color(BAD))
            .on_press(Message::Ai(Msg::PanicRelease))
            .padding([3, 10])
            .style(|_, _| button::Style { border: Border { color: BAD, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        col = col.push(space().height(6)).push(row![release, self.hint("↑↓", "move"), self.hint("space", "mark"), self.hint("↵", "release")].spacing(14).align_y(Alignment::Center));
        let dialog = container(col).padding(16).max_width(640).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: BAD, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        });
        let veil = Color { a: 0.72, ..Color::BLACK };
        Some(container(dialog).center(Length::Fill).style(move |_| container::Style::default().background(veil)).into())
    }

    /// The shown agent says its job is done: accept (it is ended) or go back to it.
    fn ai_accept_dialog(&self) -> Option<Element<'_, Message>> {
        let ai = &self.ai;
        let f = ai.finished.as_ref().filter(|_| ai.accepting)?;
        let th = &self.theme;
        let (dim, fg, panel) = (th.dim, th.foreground, th.panel);
        let name = agent_name(ai).unwrap_or_else(|| self.ai_name().to_string());
        let mut col = column![text(format!("✓ {name} says its job is done")).size(14).color(OK), space().height(4)].spacing(6);
        let part = |head: &'static str, body: &str| column![text(head).size(10).color(dim), text(body.trim().to_string()).size(12).color(fg)].spacing(1);
        col = col.push(part("ASKED", &f.asked));
        if let Some(found) = f.found.as_deref().filter(|s| !s.trim().is_empty()) {
            col = col.push(part("FOUND", found));
        }
        col = col.push(part("DONE", &f.did));
        let chord = if cfg!(target_os = "macos") { "⌘↵" } else { "ctrl+↵" };
        let see_chord = if cfg!(target_os = "macos") { "⌘D" } else { "ctrl+D" };
        // On its own branch: accepting lands it.
        let branch = self.ai_shown_session().and_then(|id| self.diff_branch(id));
        let accept_label = match &branch {
            Some((b, base)) => {
                col = col.push(part("BRANCH", &format!("{b}, to land on {base} as one commit")));
                format!("land on {base} and end it · {chord}")
            }
            None => format!("accept and end it · {chord}"),
        };
        // What would be left uncommitted in the user's tree.
        if !f.stray.is_empty() {
            let warn = th.warn;
            let files = f.stray.join(", ");
            let (head, body) = if ai.wpf {
                ("NOT ON A BRANCH", format!("worktree-per-feature is on, but these are changed in your tree: {files}. Accepting sends it back to move them to its branch."))
            } else {
                ("LEFT IN YOUR TREE", format!("not committed; accepting leaves them as they are: {files}"))
            };
            col = col.push(column![text(head).size(10).color(warn), text(body).size(12).color(warn)].spacing(1));
        }
        let see = button(text(format!("see its work · {see_chord}")).size(12).color(fg))
            .on_press(Message::Ai(Msg::AcceptDiff))
            .padding([3, 10])
            .style(move |_, _| button::Style { border: Border { color: dim, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        let accept = button(text(accept_label).size(12).color(OK))
            .on_press(Message::Ai(Msg::Accept(true)))
            .padding([3, 10])
            .style(|_, _| button::Style { border: Border { color: OK, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        let back = button(text("keep working · any key").size(12).color(dim))
            .on_press(Message::Ai(Msg::Accept(false)))
            .padding([3, 10])
            .style(move |_, _| button::Style { border: Border { color: dim, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        col = col.push(space().height(6)).push(row![accept, see, back].spacing(14).align_y(Alignment::Center));
        let dialog = container(col).padding(16).max_width(640).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: OK, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        });
        let veil = Color { a: 0.6, ..Color::BLACK };
        let backdrop = mouse_area(container(dialog).center(Length::Fill).style(move |_| container::Style::default().background(veil))).on_press(Message::Ai(Msg::Accept(false)));
        Some(backdrop.into())
    }

    fn ai_step_brief(&self, s: &Step) -> String {
        match &s.kind {
            AiStepKind::Command { command, .. } => format!("$ {}", first_line(command)),
            AiStepKind::Read { path, .. } => format!("reading {}", self.ai_rel(path)),
            AiStepKind::Edit { path, .. } => format!("editing {}", self.ai_rel(path)),
            AiStepKind::Write { path, .. } => format!("writing {}", self.ai_rel(path)),
            AiStepKind::Skill { name } => format!("loading skill {name}"),
            AiStepKind::Search { pattern, .. } => format!("searching for {pattern}"),
            AiStepKind::Agent { description, .. } => format!("sub-agent: {description}"),
            AiStepKind::Message { text, .. } => format!("message: {}", first_line(text)),
            AiStepKind::Other { tool, .. } => tool.clone(),
        }
    }

    /// The control strip's word on the agents, shown while any is at work: how many, the nearest
    /// countdown, and the panic button.
    pub(in crate::app) fn ai_strip(&self) -> Option<Element<'_, Message>> {
        let th = &self.theme;
        let working = self.ai.chats.iter().filter(|c| c.busy).count();
        let follow = self.ai_follow_label().map(|(name, live)| {
            let accent = th.accent;
            let chip = container(text(name).size(14.4).wrapping(iced::widget::text::Wrapping::None).color(accent)).padding([0, 6]).style(move |_| container::Style {
                border: Border { color: accent, width: 1.0, radius: 3.0.into() },
                background: Some(Background::Color(Color { a: 0.12, ..accent })),
                ..Default::default()
            });
            let mut label = row![text("◉ following").size(14.4).wrapping(iced::widget::text::Wrapping::None).color(accent), chip].spacing(6).align_y(Alignment::Center);
            if !live {
                label = label.push(text("(not running)").size(14.4).wrapping(iced::widget::text::Wrapping::None).color(accent));
            }
            button(label).on_press(Message::Ai(Msg::Follow)).padding(0).style(|_, _| button::Style::default())
        });
        if working == 0 {
            return follow.map(Into::into);
        }
        let paused = self.ai.chats.iter().filter(|c| c.frozen).count();
        let label = if paused > 0 { format!("‖ {paused} paused") } else { format!("● {working} working") };
        let mut line = row![].spacing(12).align_y(Alignment::Center);
        if let Some(f) = follow {
            line = line.push(f);
        }
        line = line.push(text(label).size(14.4).wrapping(iced::widget::text::Wrapping::None).color(if paused > 0 { th.warn } else { th.accent }));
        let countdown = self.ai.chats.iter().filter_map(running).filter_map(step_clock).find(|(_, down, _)| *down);
        if let Some((clock, _, _)) = countdown {
            line = line.push(text(format!("◷ {clock}")).size(14.4).wrapping(iced::widget::text::Wrapping::None).font(self.face.font).color(th.warn));
        }
        let chord = self.keymap.chords_for(Command::Panic).first().map(|k| format!(" · {k}")).unwrap_or_default();
        let panic = button(text(format!("‖ panic{chord}")).size(12).wrapping(iced::widget::text::Wrapping::None).color(BAD))
            .on_press(Message::Ai(Msg::Panic))
            .padding([2, 8])
            .style(|_, _| button::Style { border: Border { color: BAD, width: 1.0, radius: 3.0.into() }, ..Default::default() });
        Some(line.push(panic).into())
    }

    /// The agents tab: what the editor area shows while it is in front.
    pub(in crate::app) fn ai_stage(&self) -> Element<'_, Message> {
        let ai = &self.ai;
        let feed = container(self.ai_feed()).width(Length::FillPortion(5)).height(Length::Fill);
        let body = match ai.drawer {
            None => row![feed],
            Some(s) => {
                let side = match s {
                    Side::Skills => self.ai_drawer(),
                    Side::Terminal => self.ai_terminal(),
                    Side::Prompt => self.ai_prompt_pane(),
                };
                row![feed, self.vrule(), container(side).width(Length::FillPortion(4)).height(Length::Fill)]
            }
        };
        let main = column![self.ai_header(), self.hrule(), body.height(Length::Fill), self.hrule(), self.ai_footer()];
        let content: Element<'_, Message> = if ai.fold { main.into() } else { row![self.ai_rail(), self.vrule(), main].into() };
        let content: Element<'_, Message> = mouse_area(container(content).width(Length::Fill).height(Length::Fill)).on_press(Message::Ai(Msg::Focus)).into();
        match self.ai_panic_dialog().or_else(|| self.ai_accept_dialog()) {
            Some(dialog) => stack![content, dialog].into(),
            None => content,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::hide_images;

    #[test]
    fn preview_hides_images() {
        assert_eq!(hide_images("![gradient](gradient.png)"), "<image>");
        assert_eq!(hide_images("see ![a](x.png) and ![b](/y z.svg)."), "see <image> and <image>.");
        assert_eq!(hide_images("no ![image here"), "no ![image here");
        assert_eq!(hide_images("[link](x.md)"), "[link](x.md)");
    }
}
