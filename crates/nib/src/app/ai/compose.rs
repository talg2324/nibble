//! Typing a message: `/` commands and skills, and `@` attachments.
//!
//! `/name` is one of nib's commands (`/clear`, `/compact`, `/model`), the same with every
//! assistant, or a skill: its `SKILL.md` goes in front of the message, so it works with any
//! assistant, even one that wouldn't load it. A command only starts a message; a skill can be
//! named anywhere in it (`fix the colours, then /nib-control to check`). The menu groups skills by where they come from (see
//! `SkillGroup`): the assistant's own, nib's, the user's, the project's; a name in more than one
//! place shows once, the most specific place winning (repo, user, model provider, nib).
//!
//! `@path` attaches a file (its contents; unsaved edits as the editor shows them) or a folder
//! (the names in it). While a `/name` or an `@path` is being typed, a menu above the field offers
//! what fits: ↑↓ choose, Tab or Enter completes (Enter on a command runs it), Esc closes it.

use super::{Msg, Session, Turn, Who};
use crate::app::{App, Message};
use iced::widget::{button, column, container, row, text};
use iced::{Border, Element, Length, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_proto::{AiAgent, AiAttach, AiSkill, ClientNotify, SkillGroup, TreeEntry};
use std::path::PathBuf;

pub(super) struct SlashCmd {
    pub name: &'static str,
    /// What it takes after the name, as shown (`[focus]`); empty: nothing.
    pub args: &'static str,
    pub about: &'static str,
}

pub(super) const COMMANDS: &[SlashCmd] = &[
    SlashCmd { name: "clear", args: "", about: "Start over: a new chat, same model" },
    SlashCmd { name: "compact", args: "[focus]", about: "Summarise the chat so far to free up context" },
    SlashCmd { name: "model", args: "[name]", about: "Switch model or assistant" },
    SlashCmd { name: "login", args: "", about: "Sign the assistant in again, in a terminal" },
];

/// Most paths the `@` menu offers.
const MAX_PATHS: usize = 8;
/// Most rows the menu shows at once; it scrolls with the choice.
const MAX_ROWS: usize = 10;
/// nib's skills that are opt-in (D-044): offered only once installed where an assistant looks.
const OPT_IN: &[&str] = &["nib-upstream"];

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Suggest {
    Cmd(&'static SlashCmd),
    Skill(AiSkill),
    Path { path: String, dir: bool },
}

impl Suggest {
    fn group(&self) -> Option<SkillGroup> {
        match self {
            Suggest::Cmd(_) => Some(SkillGroup::Nib),
            Suggest::Skill(s) => Some(s.group),
            Suggest::Path { .. } => None,
        }
    }
}

/// What `/name` stands for: a command, or a skill. A name that only starts one of them stands
/// for it (`/comp`).
fn resolve(name: &str, skills: &[AiSkill]) -> Option<Suggest> {
    let cmds = COMMANDS.iter().map(Suggest::Cmd);
    let all: Vec<Suggest> = cmds.chain(skills.iter().cloned().map(Suggest::Skill)).collect();
    let named = |s: &Suggest| match s {
        Suggest::Cmd(c) => c.name.to_string(),
        Suggest::Skill(k) => k.name.clone(),
        Suggest::Path { .. } => String::new(),
    };
    if let Some(s) = all.iter().find(|s| named(s) == name) {
        return Some(s.clone());
    }
    match all.iter().filter(|s| !name.is_empty() && named(s).starts_with(name)).collect::<Vec<_>>().as_slice() {
        [one] => Some((*one).clone()),
        _ => None,
    }
}

/// nib's own skills (next to the binary, or in the checkout it was built in) and the user's
/// (`skills/` next to `config.toml`), on this machine.
pub(super) fn local_skills() -> Vec<AiSkill> {
    let nib = std::env::current_exe().ok().and_then(|exe| exe.ancestors().skip(1).map(|d| d.join(".skills")).find(|d| d.is_dir()));
    let user = nib_editor::config::Config::path().and_then(|p| Some(p.parent()?.join("skills")));
    let mut out: Vec<AiSkill> = nib.map(|d| nib_proto::skills::list(&d, SkillGroup::Nib)).unwrap_or_default();
    out.retain(|s| !OPT_IN.contains(&s.name.as_str()));
    out.extend(user.map(|d| nib_proto::skills::list(&d, SkillGroup::User)).unwrap_or_default());
    out
}

/// Every skill once, in menu order: a name found in several places is the most specific one's.
pub(super) fn merge_skills(lists: [&[AiSkill]; 2]) -> Vec<AiSkill> {
    let rank = |g: SkillGroup| match g {
        SkillGroup::Repo => 0,
        SkillGroup::User => 1,
        SkillGroup::Provider => 2,
        SkillGroup::Nib => 3,
    };
    let mut all: Vec<&AiSkill> = lists.into_iter().flatten().collect();
    all.sort_by_key(|s| rank(s.group));
    let mut out: Vec<AiSkill> = Vec::new();
    for s in all {
        if !out.iter().any(|o| o.name == s.name) {
            out.push(s.clone());
        }
    }
    out.sort_by(|a, b| (a.group, &a.name).cmp(&(b.group, &b.name)));
    out
}

impl std::fmt::Debug for SlashCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "/{}", self.name)
    }
}

impl PartialEq for SlashCmd {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

/// The menu for what's typed: where the word it completes starts, and what it offers. `/`: the
/// commands and skills whose name has what's typed, by group, those starting with it first.
pub(super) fn suggest(input: &str, entries: &[TreeEntry], skills: &[AiSkill]) -> (usize, Vec<Suggest>) {
    if let Some(q) = input.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) {
        let q = q.to_lowercase();
        let cmds = COMMANDS.iter().filter(|c| c.name.contains(&q)).map(|c| ((SkillGroup::Nib, !c.name.starts_with(&q), 0, c.name.to_string()), Suggest::Cmd(c)));
        let mut all: Vec<_> = cmds.chain(rank_skills(&q, skills)).collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        return (0, all.into_iter().map(|t| t.1).collect());
    }
    if let Some(start) = open_quote(input) {
        return (start, rank_paths(&input[start + 2..], entries));
    }
    let start = input.rfind(char::is_whitespace).map_or(0, |i| i + input[i..].chars().next().map_or(1, char::len_utf8));
    let word = &input[start..];
    if let Some(q) = word.strip_prefix('@') {
        return (start, rank_paths(q, entries));
    }
    // Later in the message, `/` offers the skills only: a command acts on the whole message. A
    // path (`/etc/hosts`) is not a skill.
    match word.strip_prefix('/').filter(|q| !q.contains('/')) {
        Some(q) => (start, rank_skills(q, skills).into_iter().map(|(_, s)| s).collect()),
        None => (start, Vec::new()),
    }
}

/// The skills whose name has `q`, by group, those starting with it first; with the sort key.
fn rank_skills(q: &str, skills: &[AiSkill]) -> Vec<((SkillGroup, bool, u8, String), Suggest)> {
    let q = q.to_lowercase();
    skills
        .iter()
        .filter(|s| s.name.to_lowercase().contains(&q))
        .map(|s| ((s.group, !s.name.to_lowercase().starts_with(&q), 1, s.name.clone()), Suggest::Skill(s.clone())))
        .collect()
}

/// The skills a message names later in its text (`… then /nib-control …`), each once: a word
/// that is `/` and a skill's exact name, without the punctuation after it. A `/name` that starts
/// the message is not among them (`ai_slash` and `ai_outgoing` take that one).
fn inline_skills(text: &str, skills: &[AiSkill]) -> Vec<AiSkill> {
    let mut out: Vec<AiSkill> = Vec::new();
    for (i, _) in text.match_indices('/').filter(|(i, _)| *i > 0 && word_start(text, *i)) {
        let name = text[i + 1..].split(char::is_whitespace).next().unwrap_or("").trim_end_matches([',', ';', ':', '!', '?', ')', '"', '\'', '`', '.']);
        if let Some(s) = skills.iter().find(|s| s.name == name).filter(|s| !out.iter().any(|o| o.name == s.name)) {
            out.push(s.clone());
        }
    }
    out
}

/// The project's files and folders that fit `q`, best first: the path starting with it, then the
/// name, then containing it, then its letters in order; shorter first within each. Ignored ones
/// are left out. With nothing typed yet, the top level.
fn rank_paths(q: &str, entries: &[TreeEntry]) -> Vec<Suggest> {
    let shown = |e: &TreeEntry| if e.dir { format!("{}/", e.path) } else { e.path.clone() };
    let entries = entries.iter().filter(|e| !e.ignored);
    if q.is_empty() {
        return entries.filter(|e| !e.path.contains('/')).take(MAX_PATHS).map(|e| Suggest::Path { path: e.path.clone(), dir: e.dir }).collect();
    }
    let q = q.to_lowercase();
    let mut hits: Vec<(u8, usize, String, &TreeEntry)> = entries
        .filter_map(|e| {
            let s = shown(e).to_lowercase();
            let name = e.path.rsplit('/').next().unwrap_or(&e.path).to_lowercase();
            let tier = if s.starts_with(&q) {
                0
            } else if name.starts_with(&q) {
                1
            } else if s.contains(&q) {
                2
            } else if in_order(&q, &s) {
                3
            } else {
                return None;
            };
            Some((tier, s.len(), s, e))
        })
        .collect();
    hits.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    hits.into_iter().take(MAX_PATHS).map(|(.., e)| Suggest::Path { path: e.path.clone(), dir: e.dir }).collect()
}

/// `q`'s characters appear in `s` in order.
fn in_order(q: &str, s: &str) -> bool {
    let mut it = s.chars();
    q.chars().all(|c| it.any(|x| x == c))
}

/// Whether `i` starts a word of `s` (nothing or whitespace before it).
fn word_start(s: &str, i: usize) -> bool {
    s[..i].chars().next_back().is_none_or(char::is_whitespace)
}

/// Where a `@"…` still being typed starts: its quote isn't closed yet.
fn open_quote(input: &str) -> Option<usize> {
    let (i, _) = input.rmatch_indices("@\"").next()?;
    (word_start(input, i) && !input[i + 2..].contains('"')).then_some(i)
}

/// The paths a message's `@` mentions name, each once, without the punctuation after them. A
/// path with spaces is quoted: `@"my file.gif"`.
pub(super) fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut skip = 0;
    for (i, _) in text.match_indices('@') {
        if i < skip || !word_start(text, i) {
            continue;
        }
        let after = &text[i + 1..];
        let p = match after.strip_prefix('"') {
            Some(q) => {
                let Some(j) = q.find('"') else { continue };
                skip = i + 2 + j + 1;
                &q[..j]
            }
            None => after.split(char::is_whitespace).next().unwrap_or("").trim_end_matches([',', ';', ':', '!', '?', ')', '"', '\'', '`', '.']),
        };
        if !p.is_empty() && !out.iter().any(|o| o.trim_end_matches('/') == p.trim_end_matches('/')) {
            out.push(p.to_string());
        }
    }
    out
}

/// `input` with the choice put in place of the word from `start`. A folder stays open (ends in
/// `/`) to go on into it; choosing what's already there finishes the word. A path with spaces
/// goes in quotes, left open on a folder.
fn complete(input: &mut String, start: usize, s: &Suggest) {
    match s {
        Suggest::Cmd(c) => *input = format!("/{}{}", c.name, if c.args.is_empty() { "" } else { " " }),
        Suggest::Skill(s) => {
            input.truncate(start);
            input.push_str(&format!("/{} ", s.name));
        }
        Suggest::Path { path, dir } => {
            let quoted = path.contains(char::is_whitespace);
            let word = match (quoted, dir) {
                (false, _) => format!("@{path}{}", if *dir { "/" } else { "" }),
                (true, true) => format!("@\"{path}/"),
                (true, false) => format!("@\"{path}\""),
            };
            if input[start..] == word {
                if quoted && *dir {
                    input.push('"');
                }
                input.push(' ');
            } else {
                input.truncate(start);
                input.push_str(&word);
                if !dir {
                    input.push(' ');
                }
            }
        }
    }
}

impl App {
    /// The menu for what's typed now: where its word starts, the choices, the one highlighted.
    pub(super) fn ai_menu_items(&self) -> (usize, Vec<Suggest>, usize) {
        let ai = &self.ai;
        let fresh = ai.menu_for != ai.input;
        // The menu completes the word at the end: not while the caret is back in the text.
        if (!fresh && ai.menu_hidden) || ai.caret() < ai.input.len() {
            return (0, Vec::new(), 0);
        }
        let (start, items) = suggest(&ai.input, self.tree.project_listing(), &self.ai_skills());
        let sel = if fresh { 0 } else { ai.menu_sel.min(items.len().saturating_sub(1)) };
        (start, items, sel)
    }

    fn ai_menu_set(&mut self, sel: usize, hidden: bool) {
        self.ai.menu_for = self.ai.input.clone();
        self.ai.menu_sel = sel;
        self.ai.menu_hidden = hidden;
    }

    /// A key while the menu is showing; `None`: not the menu's, it goes to the field.
    pub(super) fn ai_menu_key(&mut self, chord: KeyChord) -> Option<Task<Message>> {
        let (_, items, sel) = self.ai_menu_items();
        if items.is_empty() || chord.ctrl || chord.logo || chord.alt || (chord.shift && matches!(chord.key, Key::Up | Key::Down)) {
            return None;
        }
        let n = items.len();
        match chord.key {
            Key::Up => self.ai_menu_set((sel + n - 1) % n, false),
            Key::Down => self.ai_menu_set((sel + 1) % n, false),
            Key::Esc => self.ai_menu_set(sel, true),
            Key::Tab => return Some(self.ai_choose(sel, false)),
            Key::Enter if !chord.shift => return Some(self.ai_choose(sel, true)),
            _ => return None,
        }
        Some(Task::none())
    }

    /// Take choice `i`: complete the word; `run`: a command runs too (Enter), unless it takes
    /// something after its name and the user may want to add it (Tab first).
    pub(super) fn ai_choose(&mut self, i: usize, run: bool) -> Task<Message> {
        let (start, items, _) = self.ai_menu_items();
        let Some(s) = items.get(i) else { return Task::none() };
        if let (Suggest::Cmd(c), true) = (s, run) {
            self.ai.input.clear();
            return self.ai_command(c.name, "");
        }
        complete(&mut self.ai.input, start, s);
        self.ai.back = 0;
        self.ai.anchor = None;
        Task::none()
    }

    /// The skills the `/` menu offers: this machine's (nib's, the user's) and the daemon's
    /// (the assistant's own, the project's).
    pub(super) fn ai_skills(&self) -> Vec<AiSkill> {
        merge_skills([&self.ai.local_skills, &self.ai.skills])
    }

    /// A message typed out and sent that starts with `/`.
    /// `text` is one of nibble's own commands (`/clear`, `/model`, …): run it. Anything else
    /// (a skill, words): `None`. For while the agent waits for an answer, where words are the
    /// answer but a command is still a command.
    pub(super) fn ai_slash_command(&mut self, text: &str) -> Option<Task<Message>> {
        let rest = text.strip_prefix('/')?;
        let name = rest.split_whitespace().next().unwrap_or("");
        match resolve(name, &self.ai_skills()) {
            Some(Suggest::Cmd(_)) => match self.ai_slash(text) {
                Slash::Run(t) => Some(t),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn ai_slash(&mut self, text: &str) -> Slash {
        let Some(rest) = text.strip_prefix('/') else { return Slash::Text };
        let (name, arg) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(n, a)| (n, a.trim()));
        match resolve(name, &self.ai_skills()) {
            Some(Suggest::Cmd(c)) => {
                self.ai.input.clear();
                Slash::Run(self.ai_command(c.name, arg))
            }
            // Sent as a message, its name spelled out (`ai_outgoing` puts the skill in).
            Some(Suggest::Skill(s)) => Slash::Send(if arg.is_empty() { format!("/{}", s.name) } else { format!("/{} {arg}", s.name) }),
            // A path like `/etc/hosts …` is just text.
            _ if name.contains('/') => Slash::Text,
            _ => Slash::Run(self.flash_warn(format!("no command or skill /{name}: type / to see them"))),
        }
    }

    fn ai_command(&mut self, name: &str, arg: &str) -> Task<Message> {
        match name {
            "clear" => self.on_ai_msg(Msg::NewChat),
            "compact" => self.ai_compact(arg),
            "model" => self.ai_model_cmd(arg),
            "login" => match self.ai.agent {
                Some(AiAgent::OpenCode) => self.flash_info("OpenCode signs in per model provider: run `opencode auth login` in a terminal"),
                Some(agent) => self.ai_setup(agent, true),
                None => self.ai_pick(),
            },
            _ => Task::none(),
        }
    }

    fn ai_compact(&mut self, focus: &str) -> Task<Message> {
        if self.ai.busy {
            return self.flash_info(format!("{} is still replying", self.ai_name()));
        }
        let Session::Ready(id) = self.ai.session else {
            return self.flash_info("nothing to compact: no chat is running");
        };
        if !self.ai.turns.iter().any(|t| t.who == Who::User) {
            return self.flash_info("nothing to compact yet");
        }
        let focus = Some(focus.to_string()).filter(|f| !f.is_empty());
        self.notify(ClientNotify::AiCompact { id, focus });
        self.ai.busy = true;
        self.ai.compacting = true;
        self.ai.error = None;
        self.ai_scroll_to_end()
    }

    /// `/model`: the dropdown; `/model <name>`: that model of this assistant, or that assistant.
    fn ai_model_cmd(&mut self, arg: &str) -> Task<Message> {
        if arg.is_empty() {
            return self.ai_pick();
        }
        let want = arg.to_lowercase();
        let ready = |a: AiAgent| self.ai.agents.as_ref().is_some_and(|l| l.iter().any(|i| i.agent == a && i.ready()));
        let named = [AiAgent::Claude, AiAgent::Codex, AiAgent::Grok, AiAgent::OpenCode].into_iter().find(|a| a.name().to_lowercase().starts_with(&want));
        if let Some(agent) = named.filter(|a| ready(*a)) {
            return self.on_ai_msg(Msg::Use(agent, None));
        }
        let Some(agent) = self.ai.agent else { return self.ai_pick() };
        let models = self.ai_models(agent);
        let exact = models.iter().find(|m| m.id.to_lowercase() == want || m.name.to_lowercase() == want);
        let found: Vec<_> = models.iter().filter(|m| m.id.to_lowercase().contains(&want) || m.name.to_lowercase().contains(&want)).collect();
        match (exact, found.as_slice()) {
            (Some(m), _) | (None, &[m]) => {
                let id = m.id.clone();
                self.on_ai_msg(Msg::Use(agent, Some(id)))
            }
            (None, []) => self.flash_warn(format!("{} has no model like {arg:?}", agent.name())),
            (None, _) => {
                let n = found.len();
                Task::batch([self.ai_pick(), self.flash_info(format!("{n} models match {arg:?}: pick one"))])
            }
        }
    }

    /// Where a `@` mention points.
    fn mention_path(&self, path: &str) -> PathBuf {
        PathBuf::from(nib_proto::path::join(&self.root, path.trim_end_matches('/')))
    }

    /// Each GIF the message mentions goes with it as a sheet of its distinct frames (models see
    /// only a GIF's first frame, and the agent would open the file itself). Only on this
    /// machine: on another, the file isn't here to read. What couldn't be read, if anything.
    pub(super) fn ai_gif_mentions(&mut self, text: &str) -> Option<String> {
        if self.label != "local" {
            return None;
        }
        let mut failed = Vec::new();
        for p in mentions(text).into_iter().filter(|p| p.to_ascii_lowercase().ends_with(".gif")) {
            match super::Pic::from_file(&self.mention_path(&p)) {
                Ok(pic) => self.ai.images.push(pic),
                Err(e) => failed.push(e),
            }
        }
        (!failed.is_empty()).then(|| failed.join("; "))
    }

    /// What goes to the assistant for a message as typed: `/skill …` becomes the rest of it with
    /// the skill attached (nib's and the user's are read here, the others where they are); the
    /// `@` mentions are attached, open files with unsaved edits as the editor has them.
    pub(super) fn ai_outgoing(&self, text: &str) -> (String, Vec<AiAttach>) {
        let mut attach = Vec::new();
        let mut text = text.to_string();
        if let Some(rest) = text.strip_prefix('/') {
            let (name, arg) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(n, a)| (n, a.trim()));
            if let Some(Suggest::Skill(s)) = resolve(name, &self.ai_skills()) {
                let local = matches!(s.group, SkillGroup::Nib | SkillGroup::User);
                let body = if local { std::fs::read_to_string(&s.path).ok() } else { None };
                if !local || body.is_some() {
                    attach.push(AiAttach { path: s.path.clone(), text: body, skill: Some(s.name.clone()) });
                    text = if arg.is_empty() { "Use the skill above.".to_string() } else { arg.to_string() };
                }
            }
        }
        // Skills named later in the message go in too; the text keeps their names where they
        // were written.
        for s in inline_skills(&text, &self.ai_skills()) {
            let local = matches!(s.group, SkillGroup::Nib | SkillGroup::User);
            let body = if local { std::fs::read_to_string(&s.path).ok() } else { None };
            if (!local || body.is_some()) && !attach.iter().any(|a: &AiAttach| a.skill.as_ref() == Some(&s.name)) {
                attach.push(AiAttach { path: s.path.clone(), text: body, skill: Some(s.name.clone()) });
            }
        }
        attach.extend(mentions(&text).into_iter().map(|path| {
            let abs = self.mention_path(&path);
            let open = self.tabs.iter().find(|t| t.doc.is_modified() && t.doc.path.as_deref() == Some(abs.as_path()));
            let text = self.ai.dropped.get(&path).cloned().or_else(|| open.map(|t| t.doc.text.to_string()));
            AiAttach { text, path, skill: None }
        }));
        (text, attach)
    }

    /// The menu above the message field, when there's something to offer: up to `MAX_ROWS`
    /// around the choice, under the heading of each group.
    pub(super) fn ai_menu(&self) -> Option<Element<'_, Message>> {
        let (_, items, sel) = self.ai_menu_items();
        if items.is_empty() {
            return None;
        }
        let th = &self.theme;
        let (fg, dim, accent, sel_bg) = (th.foreground, th.dim, th.accent, th.selection);
        let first = sel.saturating_sub(MAX_ROWS - 1).min(items.len().saturating_sub(MAX_ROWS));
        let shown = first..items.len().min(first + MAX_ROWS);
        let mut col = column![].spacing(1);
        let mut last_group = None;
        for i in shown.clone() {
            let s = &items[i];
            if let Some(g) = s.group().filter(|g| Some(*g) != last_group) {
                let top = if last_group.is_some() { 6.0 } else { 2.0 };
                col = col.push(container(text(g.label()).size(10).color(th.skill_group(g))).padding(iced::Padding { left: 8.0, top, bottom: 1.0, right: 0.0 }));
                last_group = Some(g);
            }
            let (label, about) = match s {
                Suggest::Cmd(c) => (format!("/{} {}", c.name, c.args), c.about.to_string()),
                Suggest::Skill(k) => (format!("/{}", k.name), k.description.clone()),
                Suggest::Path { path, dir: true } => (format!("{path}/"), String::new()),
                Suggest::Path { path, dir: false } => (path.clone(), String::new()),
            };
            // One line each: the description takes what the name leaves of the panel's width.
            let about = clip(&about, 54usize.saturating_sub(label.chars().count() * 11 / 10));
            let on = i == sel;
            let one_line = iced::widget::text::Wrapping::None;
            let line = row![
                text(label).size(12).color(if on { accent } else { fg }).wrapping(one_line),
                text(about).size(11).color(dim).wrapping(one_line)
            ]
            .spacing(10)
            .clip(true);
            col = col.push(
                button(line)
                    .on_press(Message::Ai(Msg::Suggest(i)))
                    .width(Length::Fill)
                    .padding([3, 8])
                    .style(move |_: &iced::Theme, status: button::Status| button::Style {
                        background: (on || matches!(status, button::Status::Hovered)).then(|| sel_bg.into()),
                        border: Border { radius: 3.0.into(), ..Default::default() },
                        ..Default::default()
                    }),
            );
        }
        let (above, below) = (shown.start, items.len() - shown.end);
        if above + below > 0 {
            let more = match (above, below) {
                (0, b) => format!("↓ {b} more"),
                (a, 0) => format!("↑ {a} more"),
                (a, b) => format!("↑ {a} · ↓ {b} more"),
            };
            col = col.push(container(text(more).size(10).color(dim)).padding([2, 8]));
        }
        let divider = th.divider;
        Some(
            container(col)
                .padding(3)
                .width(Length::Fill)
                .style(move |_| container::Style { border: Border { color: divider, width: 1.0, radius: 4.0.into() }, ..Default::default() })
                .into(),
        )
    }

    /// A note in the transcript (not from either side): a compaction.
    pub(super) fn ai_note(&mut self, text: String) {
        self.ai.turns.push(Turn { who: Who::Note, text, md: iced::widget::markdown::Content::new(), math: Default::default(), images: Default::default(), step: None });
    }
}

/// What a sent message starting with `/` turned out to be.
pub(super) enum Slash {
    /// A command (or a refusal): done here.
    Run(Task<Message>),
    /// A skill: send this (the name spelled out).
    Send(String),
    /// Not a command: send it as typed.
    Text,
}

/// `s` cut to `n` characters, with `…` if it was longer.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out = out.trim_end().to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, dir: bool) -> TreeEntry {
        TreeEntry { path: path.into(), dir, ignored: false }
    }
    fn listing() -> Vec<TreeEntry> {
        vec![
            entry("crates", true),
            entry("README.md", false),
            TreeEntry { path: "target".into(), dir: true, ignored: true },
            entry("crates/nib", true),
            entry("crates/nibd", true),
            entry("crates/nib/src", true),
            entry("crates/nibd/src", true),
            entry("crates/nib/src/app.rs", false),
            entry("crates/nibd/src/ai.rs", false),
            entry("crates/nib/src/app/ai.rs", false),
        ]
    }
    fn paths(s: &[Suggest]) -> Vec<String> {
        s.iter()
            .map(|s| match s {
                Suggest::Path { path, dir } => format!("{path}{}", if *dir { "/" } else { "" }),
                Suggest::Cmd(c) => format!("/{}", c.name),
                Suggest::Skill(s) => format!("{}:/{}", s.group.label(), s.name),
            })
            .collect()
    }
    fn skill(name: &str, group: SkillGroup) -> AiSkill {
        AiSkill { name: name.into(), description: String::new(), group, path: format!("/{name}/SKILL.md") }
    }

    #[test]
    fn skills_are_grouped_and_a_name_shows_once() {
        let local = [skill("nib-control", SkillGroup::Nib), skill("nib-ai", SkillGroup::Nib), skill("review", SkillGroup::User)];
        let remote = [skill("humanizer", SkillGroup::Provider), skill("nib-ai", SkillGroup::Repo), skill("review", SkillGroup::Provider)];
        let all = merge_skills([&local, &remote]);
        let (_, s) = suggest("/", &[], &all);
        assert_eq!(
            paths(&s),
            ["model provider:/humanizer", "/clear", "/compact", "/login", "/model", "nib:/nib-control", "user:/review", "repo:/nib-ai"],
            "the more specific place wins; commands lead nib's group"
        );
        assert_eq!(paths(&suggest("/ai", &[], &all).1), ["repo:/nib-ai"]);
        assert!(matches!(resolve("hum", &all), Some(Suggest::Skill(s)) if s.name == "humanizer"));
        assert!(matches!(resolve("c", &all), None), "clear, compact: not one");
        assert!(matches!(resolve("compact", &all), Some(Suggest::Cmd(_))));
    }

    #[test]
    fn slash_offers_the_commands_that_start_so() {
        let (_, s) = suggest("/c", &[], &[]);
        assert_eq!(paths(&s), ["/clear", "/compact"]);
        assert!(suggest("/compact the api", &[], &[]).1.is_empty(), "past the name, it's the argument");
        let skills = [skill("nib-control", SkillGroup::Nib), skill("review", SkillGroup::User)];
        let (start, s) = suggest("see /c", &[], &skills);
        assert_eq!((start, paths(&s)), (4, vec!["nib:/nib-control".to_string()]), "later on: skills, no commands");
        assert!(suggest("cat /etc/ho", &[], &skills).1.is_empty(), "a path is not a skill");
        let mut input = "please /nib-c".to_string();
        complete(&mut input, 7, &s[0]);
        assert_eq!(input, "please /nib-control ");
        assert_eq!(inline_skills("/review this, then /nib-control. and /nope /review", &skills).iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["nib-control", "review"]);
        assert!(inline_skills("a/review b", &skills).is_empty(), "only at a word's start");
    }

    #[test]
    fn at_offers_files_and_folders_best_first() {
        let l = listing();
        assert_eq!(paths(&suggest("@", &l, &[]).1), ["crates/", "README.md"], "the top level, nothing ignored");
        let (start, s) = suggest("look at @ai.rs", &l, &[]);
        assert_eq!(start, 8);
        assert_eq!(paths(&s), ["crates/nibd/src/ai.rs", "crates/nib/src/app/ai.rs", "crates/nib/src/app.rs"], "by name, shorter first; then letters in order");
        assert_eq!(paths(&suggest("@crates/nib/", &l, &[]).1)[..2], ["crates/nib/", "crates/nib/src/"]);
        assert_eq!(paths(&suggest("@cnsapp", &l, &[]).1), ["crates/nib/src/app.rs", "crates/nib/src/app/ai.rs"], "letters in order");
        assert!(suggest("@target", &l, &[]).1.is_empty());
    }

    #[test]
    fn completing_keeps_folders_open() {
        let mut s = "see @cr".to_string();
        complete(&mut s, 4, &Suggest::Path { path: "crates".into(), dir: true });
        assert_eq!(s, "see @crates/");
        complete(&mut s, 4, &Suggest::Path { path: "crates".into(), dir: true });
        assert_eq!(s, "see @crates/ ", "choosing it again finishes it");
        let mut s = "@ap".to_string();
        complete(&mut s, 0, &Suggest::Path { path: "crates/nib/src/app.rs".into(), dir: false });
        assert_eq!(s, "@crates/nib/src/app.rs ");
    }

    #[test]
    fn mentions_are_the_at_words() {
        assert_eq!(mentions("compare @a.rs, @src/ and @a.rs. Mail me@x.com or @src"), ["a.rs", "src/"]);
        assert_eq!(mentions("see @\"my rec.gif\", @\"a @b\" and @\"open"), ["my rec.gif", "a @b"], "quoted: spaces, an @ inside, unclosed skipped");
    }
}
