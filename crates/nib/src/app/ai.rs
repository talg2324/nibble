//! The AI panel: a chat with the user's own coding assistant (Claude Code, Codex or Grok). The
//! daemon runs it where the code lives, so over SSH it is the remote's, signed in there; this side
//! shows the conversation and the message being typed. Enter sends, Shift+Enter starts a new
//! line, Esc gives the keyboard back.
//!
//! The header's one dropdown picks both: the assistants (model providers) across the top, the
//! chosen one's models below; picking a model starts a new chat with it. When none is ready on
//! that machine the panel doesn't send the user off to install one: the dropdown is open, and
//! Install / Sign in runs the vendor's installer and sign-in in a terminal. When that terminal
//! finishes, the chat starts. OpenCode comes with Ollama (its Install sets up both); its tab also
//! manages Ollama's models: each local one's size on disk and a delete button, and a field to
//! pull another.
//!
//! The message field takes `/` commands and `@` attachments (see `compose`).

mod act;
mod caret;
mod compose;
mod follow;
mod history;
mod ink;
mod question;
mod stage;
mod wait;

use super::viewers::PreviewImage;
use super::plan::{Outcome, Plan, PlanMsg};
use super::{App, Focus, Message, Pending};
use crate::links;
use crate::math;
use crate::preview::{self, Target};
use iced::advanced::widget::{operate, operation, Id as WidgetId};
use iced::widget::{button, column, container, markdown, progress_bar, row, space, text};
use iced::{Alignment, Border, Color, Element, Length, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::Command;
use nib_proto::{AiAgent, AiAgentInfo, AiAsk, AiEvent, AiFinished, AiId, AiImage, AiLimit, AiMode, AiModel, AiStepKind, AiUsage, ClientNotify, ErrorKind, OllamaInfo, OllamaModel, Request, Response, RpcError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TRANSCRIPT_ID: &str = "ai-transcript";
/// Where Ollama's models are found by name.
const OLLAMA_REGISTRY: &str = "https://ollama.com/search";

#[derive(Debug, Default)]
enum Session {
    #[default]
    None,
    /// `AiOpen` is on its way.
    Starting,
    Ready(AiId),
    /// Couldn't start, or the assistant exited. Sending again starts a new session.
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Who {
    User,
    Assistant,
    /// Said by nib, between the two: a compaction.
    Note,
    /// Something the agent did (`Turn::step`).
    Step,
    /// A message to or from another agent, through nib's mailbox.
    Mail,
    /// The agent's word that its job is done (`AiEvent::Finished`).
    Done,
}

struct Turn {
    who: Who,
    text: String,
    /// The reply, parsed as it streams in (user turns are shown as typed).
    md: markdown::Content,
    /// The formulas in `md` (see `crate::math`).
    math: math::Formulas,
    /// Pictures `md` shows (files on the daemon's side), by the URL written in the reply.
    /// `None`: asked for, not here (yet), or not readable.
    images: HashMap<String, Option<PreviewImage>>,
    step: Option<Step>,
}

/// One thing an agent did or is doing: a tool call of its harness.
struct Step {
    id: String,
    /// The step (a sub-agent) it happens inside.
    parent: Option<String>,
    kind: AiStepKind,
    /// What it does to files, read from `kind` when it arrived (`act::of`); not stored.
    acts: Vec<act::Act>,
    started: Instant,
    done: Option<StepEnd>,
}

struct StepEnd {
    ok: bool,
    output: String,
    /// An edit: the first line it changed.
    line: Option<u32>,
    took: Duration,
}

/// One conversation with one assistant. Several run at once; `Panel` derefs to the one shown.
#[derive(Default)]
pub struct Chat {
    /// Stays the same while the chat lives (its place in the list doesn't).
    key: u64,
    session: Session,
    /// Who the chat is with (the last one picked).
    agent: Option<AiAgent>,
    /// The model the chat runs (one of the agent's `models`); `None`: the one it's set up with.
    chosen: Option<String>,
    /// What the assistant said it runs (Claude Code says, the others don't).
    model: Option<String>,
    turns: Vec<Turn>,
    input: String,
    /// The caret, as bytes from the end of `input` (see `caret`).
    back: usize,
    /// Where the selection started, the same way; `None`: nothing selected.
    anchor: Option<usize>,
    /// Sent while the session was starting: goes out once it's ready.
    queued: Option<String>,
    /// A reply is streaming in, or the agent is at work.
    busy: bool,
    /// Why the last reply stopped early.
    error: Option<String>,
    /// What the chat uses, as of its last reply.
    usage: Option<AiUsage>,
    /// When this session started: what it cost is spread over the time since.
    since: Option<Instant>,
    /// `/compact` is under way.
    compacting: bool,
    /// The `/` / `@` menu's state is for this input; once the input changes it starts over (first
    /// choice, showing).
    menu_for: String,
    menu_sel: usize,
    /// Esc closed the menu.
    menu_hidden: bool,
    /// Skills on the daemon's machine for the assistant in use (its own, the project's).
    skills: Vec<nib_proto::AiSkill>,
    /// The user stopped the turn: its end is their doing, not an error.
    stopping: bool,
    /// Its processes are frozen (panic).
    frozen: bool,
    /// It couldn't be frozen, so panic interrupted it: "continue" goes out on release.
    resume: bool,
    /// What the other agents call it (from its task), once the daemon has said.
    name: Option<String>,
    /// The past conversation it was reopened from: its next session goes on with that one.
    past: Option<String>,
    /// How much it may do without asking.
    mode: AiMode,
    /// The mode the session was last told: one picked while it was starting goes out once it
    /// is ready.
    told_mode: AiMode,
    /// What it is waiting for leave to do.
    ask: Option<AiAsk>,
    /// The answers being picked, when what it asks is questions.
    form: question::Form,
    /// The plans it handed over to approve (`ExitPlanMode`), by the step that carried them (or
    /// the request, if no step showed): rendered, with the user's comments.
    plans: HashMap<String, Plan>,
    /// What was answered (allowed, the answers): the tests' connection doesn't keep it.
    #[cfg(test)]
    answered: Vec<(bool, Vec<String>)>,
    /// What went with the last refusal.
    #[cfg(test)]
    told: Option<String>,
    /// It said its job is done, and the user hasn't answered: outlined green in the list.
    finished: Option<AiFinished>,
    /// The accept dialog is up for it (it is shown, and finished).
    accepting: bool,
    /// Pictures pasted into the message being written.
    images: Vec<Pic>,
    /// How hard its model thinks: one of its assistant's `efforts`; `None`: the assistant's own.
    effort: Option<String>,
    /// What its sessions add to the assistant's system prompt.
    prompt: iced::widget::text_editor::Content,
    /// The thinking level and prompt its session started with.
    told_tuning: (Option<String>, Option<String>),
    /// Files dropped on the chat, read on this machine: `@` name -> contents. They go with the
    /// message instead of what the daemon would read (it may be on another machine).
    pub(super) dropped: HashMap<String, String>,
}

/// What the pane beside an agent's steps shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The agent's commands and what came back.
    Terminal,
    /// What it can load.
    Skills,
    /// Its system prompt, to edit.
    Prompt,
}

/// One entry of the thinking dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effort {
    /// The assistant's own setting.
    Default,
    Level(String),
}

impl Effort {
    fn of(e: &Option<String>) -> Effort {
        e.clone().map_or(Effort::Default, Effort::Level)
    }

    fn level(self) -> Option<String> {
        match self {
            Effort::Default => None,
            Effort::Level(l) => Some(l),
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effort::Default => f.write_str("default"),
            Effort::Level(l) => f.write_str(l),
        }
    }
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Terminal => "terminal",
            Side::Skills => "skills",
            Side::Prompt => "system",
        }
    }
}

/// The plan in a step that hands one over to approve.
fn plan_of(kind: &AiStepKind) -> Option<String> {
    let AiStepKind::Other { tool, input } = kind else { return None };
    if tool != "ExitPlanMode" {
        return None;
    }
    serde_json::from_str::<Value>(input).ok()?.get("plan")?.as_str().map(str::to_string)
}

impl Chat {
    /// The key of the plan it asks to have approved, if that is what it asks.
    pub(super) fn asked_plan(&self) -> Option<&String> {
        let a = self.ask.as_ref().filter(|a| a.tool == "ExitPlanMode")?;
        let key = a.step.as_ref().unwrap_or(&a.request);
        self.plans.contains_key(key).then_some(key)
    }

    pub(super) fn asked_plan_mut(&mut self) -> Option<&mut Plan> {
        let key = self.asked_plan()?.clone();
        self.plans.get_mut(&key)
    }

    /// Nothing has been said or typed in it, and no session started.
    pub(super) fn blank(&self) -> bool {
        self.turns.is_empty() && self.input.is_empty() && matches!(self.session, Session::None)
    }
}

/// A picture for a message, as PNG.
#[derive(Clone)]
pub struct Pic {
    png: Vec<u8>,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for Pic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pic({}x{}, {} bytes)", self.width, self.height, self.png.len())
    }
}

impl Pic {
    /// The longest side a picture is sent with: bigger ones are scaled down.
    const MAX_SIDE: u32 = 2000;

    fn from_rgba(img: image::RgbaImage) -> Option<Pic> {
        let (w, h) = img.dimensions();
        let img = if w.max(h) > Self::MAX_SIDE {
            let k = Self::MAX_SIDE as f64 / w.max(h) as f64;
            image::imageops::resize(&img, ((w as f64 * k) as u32).max(1), ((h as f64 * k) as u32).max(1), image::imageops::FilterType::Triangle)
        } else {
            img
        };
        let (width, height) = img.dimensions();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
        Some(Pic { png, width, height })
    }

    /// An image file on this machine.
    pub fn from_file(path: &std::path::Path) -> Result<Pic, String> {
        // A model sees only a GIF's first frame: send all its distinct frames, as one sheet.
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif")) {
            return Pic::from_rgba(crate::gif::sheet(path)?).ok_or_else(|| "couldn't encode the image".to_string());
        }
        let img = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Pic::from_rgba(img.to_rgba8()).ok_or_else(|| "couldn't encode the image".to_string())
    }
}

/// What the clipboard held when Paste was pressed in the message box.
#[derive(Debug, Clone)]
pub enum Clip {
    Text(String),
    Image(Pic),
    Nothing,
}

/// Text if the clipboard has any, else a picture (a screenshot, a copied image).
fn read_clipboard() -> Clip {
    let Ok(mut c) = arboard::Clipboard::new() else { return Clip::Nothing };
    if let Ok(t) = c.get_text() {
        if !t.is_empty() {
            return Clip::Text(t);
        }
    }
    if let Ok(i) = c.get_image() {
        if let Some(pic) = image::RgbaImage::from_raw(i.width as u32, i.height as u32, i.bytes.into_owned()).and_then(Pic::from_rgba) {
            return Clip::Image(pic);
        }
    }
    windows_clipboard_image().map_or(Clip::Nothing, Clip::Image)
}

/// On WSL, the picture on the Windows clipboard (a Snipping Tool / Win+Shift+S snip): WSLg
/// passes only text across to Linux, so arboard never sees it. Asks Windows PowerShell for it
/// as PNG bytes on its stdout (about a second: PowerShell is slow to start). `None` off WSL, or
/// when the Windows clipboard holds no picture.
fn windows_clipboard_image() -> Option<Pic> {
    let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft"));
    if !wsl {
        return None;
    }
    const SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
        $i = [Windows.Forms.Clipboard]::GetImage(); if ($i) { $m = New-Object IO.MemoryStream; \
        $i.Save($m, [Drawing.Imaging.ImageFormat]::Png); $o = [Console]::OpenStandardOutput(); \
        $m.WriteTo($o); $o.Flush() }";
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Pic::from_rgba(image::load_from_memory_with_format(&out.stdout, image::ImageFormat::Png).ok()?.to_rgba8())
}

pub struct Panel {
    /// The agents tab exists (in the tab bar).
    pub open: bool,
    /// It is the tab shown in the editor area.
    pub front: bool,
    /// The file tab that was in front when last looked: another one coming forward hides the
    /// agents tab.
    tabs_seen: (usize, usize, Option<u64>),
    /// Never empty: with nothing going on there is one blank chat.
    chats: Vec<Chat>,
    /// The chat shown.
    cur: usize,
    next_key: u64,
    /// The agent list is folded away: one agent at a time.
    fold: bool,
    /// The pane beside the steps; `None`: none, and the chat takes the whole width.
    drawer: Option<Side>,
    /// The thinking level and system prompt an agent last started with (`[ai] effort` /
    /// `prompt`): new agents start with them.
    last_effort: Option<String>,
    last_prompt: String,
    /// Bumped by each edit of the system prompt: a save waits until the typing stops.
    prompt_save_seq: u64,
    /// Every agent is paused and the dialog is up: each working chat's key and whether it is
    /// marked for stopping; the row the keyboard is on.
    panic: Option<(Vec<(u64, bool)>, usize)>,
    /// A one-second clock is running for the countdowns.
    ticking: bool,
    /// The agent being followed (see `follow`).
    follow: Option<follow::Follow>,
    /// What the agents said to each other (and what nib told them), oldest first.
    mailbox: Vec<nib_proto::AiMail>,
    /// How many messages have ever gone into `mailbox`: `mailbox[i]` is number
    /// `mail_count - mailbox.len() + i`, which stays put when old ones are dropped.
    mail_count: u64,
    /// The mailbox messages shown in full (by that number); the rest are clipped.
    mail_open: std::collections::HashSet<u64>,
    /// The history tab (see `history`).
    history: history::History,
    /// A conversation being replayed in Follow: the chat, and the turn to go on from.
    replay: Option<(u64, usize)>,
    /// What the daemon's machine has; `None` until asked.
    agents: Option<Vec<AiAgentInfo>>,
    /// `AiAgents` is on its way.
    checking: bool,
    /// The assistant / model dropdown is open (none is ready, or the user asked to switch).
    picking: bool,
    /// The assistant whose models the dropdown shows; `None`: the one in use.
    tab: Option<AiAgent>,
    /// The dropdown row the arrow keys are on (0: Default, then the models); `None`: the one in
    /// use, else the first.
    pick_sel: Option<usize>,
    /// The assistant and model last picked (`[ai] agent` / `model`): new agents start with it.
    last: Option<(AiAgent, Option<String>)>,
    /// `[ai] pick_model`: a new agent opens the dropdown (off: it starts with `last`).
    pick_model: bool,
    /// The terminal installing / signing in an assistant: its key, and which one.
    setup: Option<(u64, AiAgent)>,
    /// The terminal updating an assistant: its key, and which one.
    update: Option<(u64, AiAgent)>,
    /// Local models being loaded / unloaded (`AiWarm` is on its way).
    warming: std::collections::HashSet<String>,
    /// The Ollama model typed in the dropdown's pull field.
    pull_input: String,
    /// Keys go to the pull field (it was clicked), not the message.
    pull_focus: bool,
    /// The model pulling now (`AiPull` is on its way).
    pulling: Option<Pull>,
    /// A model whose 🗑 was clicked once: the next click deletes it.
    confirm_delete: Option<String>,
    /// Ollama models being deleted.
    deleting: std::collections::HashSet<String>,
    /// Why the last setup didn't leave the assistant ready.
    setup_failed: Option<AiAgent>,
    return_to: Option<Focus>,
    /// `[ai] cloud_models`: list OpenCode's cloud models, not only the downloaded ones.
    cloud_models: bool,
    /// nib's and the user's skills, on this machine.
    local_skills: Vec<nib_proto::AiSkill>,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            open: false,
            front: false,
            tabs_seen: (0, 0, None),
            chats: vec![Chat::default()],
            cur: 0,
            next_key: 1,
            fold: false,
            drawer: Some(Side::Terminal),
            last_effort: None,
            last_prompt: String::new(),
            prompt_save_seq: 0,
            panic: None,
            ticking: false,
            follow: None,
            mailbox: Vec::new(),
            mail_count: 0,
            mail_open: Default::default(),
            history: Default::default(),
            replay: None,
            agents: None,
            checking: false,
            picking: false,
            tab: None,
            pick_sel: None,
            last: None,
            pick_model: true,
            setup: None,
            update: None,
            warming: Default::default(),
            pull_input: String::new(),
            pull_focus: false,
            pulling: None,
            confirm_delete: None,
            deleting: Default::default(),
            setup_failed: None,
            return_to: None,
            cloud_models: false,
            local_skills: Vec::new(),
        }
    }
}

impl std::ops::Deref for Panel {
    type Target = Chat;
    fn deref(&self) -> &Chat {
        &self.chats[self.cur]
    }
}

impl std::ops::DerefMut for Panel {
    fn deref_mut(&mut self) -> &mut Chat {
        &mut self.chats[self.cur]
    }
}

impl Panel {
    /// The agents tab (or the history tab) is what the editor area shows.
    pub fn shown(&self) -> bool {
        self.open && self.front
    }

    /// The history tab exists.
    pub fn history_open(&self) -> bool {
        self.history.open
    }

    fn history_pick(&mut self, i: usize) {
        self.history.pick(i);
    }
}

/// An Ollama model being pulled: what it's doing now (`pulling 3a4f…`, `verifying`) and how far
/// that step is, in bytes.
#[derive(Debug, Clone)]
struct Pull {
    model: String,
    status: String,
    completed: u64,
    total: u64,
}

impl Panel {
    pub fn new(cfg: nib_editor::config::AiConfig) -> Self {
        let last = cfg.agent.as_deref().and_then(AiAgent::from_id).map(|a| (a, cfg.model.clone()));
        let mut p = Self { cloud_models: cfg.cloud_models, pick_model: cfg.pick_model, last: last.clone(), last_effort: cfg.effort, last_prompt: cfg.prompt.unwrap_or_default(), ..Default::default() };
        // The first chat is with the one picked last time.
        if let Some((agent, model)) = last {
            (p.chats[0].agent, p.chats[0].chosen) = (Some(agent), model);
        }
        let first = std::mem::take(&mut p.chats[0]);
        p.chats[0] = p.seeded(first);
        p
    }

    /// The remembered system prompt, `None` when empty.
    fn last_prompt_opt(&self) -> Option<String> {
        Some(self.last_prompt.clone()).filter(|p| !p.is_empty())
    }

    /// config.toml's `[ai] effort` / `prompt` changed: new agents start with them. The thinking
    /// level is each agent's own, like its model, so open agents keep theirs; the prompt goes to
    /// the agents that had the old one and haven't started talking.
    pub(super) fn retune(&mut self, effort: Option<String>, prompt: Option<String>) {
        self.last_effort = effort;
        let old_prompt = std::mem::replace(&mut self.last_prompt, prompt.unwrap_or_default());
        // A save on its way would write back the old prompt.
        self.prompt_save_seq += 1;
        for c in &mut self.chats {
            if c.turns.iter().any(|t| t.who == Who::User) {
                continue;
            }
            if c.prompt.text().trim() == old_prompt.trim() {
                c.prompt = iced::widget::text_editor::Content::with_text(&self.last_prompt);
            }
        }
    }

    /// `c` with the thinking level and system prompt the last agent started with.
    pub(super) fn seeded(&self, mut c: Chat) -> Chat {
        c.effort = self.last_effort.clone();
        c.prompt = iced::widget::text_editor::Content::with_text(&self.last_prompt);
        c
    }
}

/// A step on its own, for the code that reacts to it while the chat is borrowed.
fn step_copy(s: &Step) -> Step {
    Step {
        id: s.id.clone(),
        parent: s.parent.clone(),
        kind: s.kind.clone(),
        acts: s.acts.clone(),
        started: s.started,
        done: s.done.as_ref().map(|d| StepEnd { ok: d.ok, output: String::new(), line: d.line, took: d.took }),
    }
}

/// The turn is over: a step the harness never closed (the turn was stopped) is over too.
fn end_steps(c: &mut Chat) {
    for s in c.turns.iter_mut().filter_map(|t| t.step.as_mut()).filter(|s| s.done.is_none()) {
        s.done = Some(StepEnd { ok: false, output: String::new(), line: None, took: s.started.elapsed() });
    }
}

/// Whether an assistant's error means its sign-in is missing or no longer good.
fn needs_login(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["not logged in", "please run /login", "run `claude login`", "invalid api key", "oauth token has expired", "authentication_error"]
        .iter()
        .any(|k| m.contains(k))
}

#[derive(Debug, Clone)]
pub enum Msg {
    Toggle,
    Close,
    NewChat,
    /// Open / close the assistant / model dropdown.
    Switch,
    /// Show this assistant's models in the dropdown.
    Tab(AiAgent),
    /// Chat with this one now, with this model (`None`: the one it's set up with).
    Use(AiAgent, Option<String>),
    /// Install and / or sign in this one, in a terminal.
    Setup(AiAgent),
    /// Update this one to its latest version, in a terminal.
    Update(AiAgent),
    /// Load this local model into memory and keep it there (`true`), or unload it.
    Warm(String, bool),
    /// The pull field was clicked: keys go there.
    PullFocus,
    /// Pull the model typed in the field.
    Pull,
    /// Open Ollama's model registry in the browser.
    Registry,
    /// Delete this Ollama model (`name:tag`): the first click asks, the second deletes.
    Delete(String),
    /// The panel was clicked: give it the keyboard.
    Focus,
    /// Choice `i` of the `/` / `@` menu was clicked.
    Suggest(usize),
    Pasted(Option<String>),
    /// A picture file dropped on the chat, decoded (`drop.rs`).
    DroppedPic(Result<Pic, String>),
    /// A file or folder dropped on the chat: its `@` name and, for a file, its contents.
    DroppedText(Result<(String, Option<String>), String>),
    /// Bring the agents tab to the front.
    Front,
    /// Show this agent (its place in the list).
    Select(usize),
    /// Another agent, next to the ones there are.
    NewAgent,
    /// End this agent's conversation.
    Kill(usize),
    /// Stop what the shown agent is doing; the conversation stays.
    Stop,
    /// Fold / unfold the agent list.
    Fold,
    /// What the pane beside the steps shows; `None`: nothing.
    Side(Option<Side>),
    /// Pause every agent and ask which to stop.
    Panic,
    /// Mark / unmark this row of the panic dialog.
    PanicMark(usize),
    /// Stop the marked agents; the others carry on.
    PanicRelease,
    /// The countdowns' one-second clock.
    Clock,
    /// How much the shown agent may do without asking.
    Mode(AiMode),
    /// How hard the shown agent's model thinks (`Effort::Default`: the assistant's own).
    Effort(Effort),
    /// An edit in the system prompt pane.
    Prompt(iced::widget::text_editor::Action),
    /// Allow (or refuse) what the shown agent asked leave for. Its questions: send the answers
    /// (or skip them).
    Answer(bool),
    /// Pick option `.1` of question `.0` of what the shown agent asks.
    Pick(usize, usize),
    /// Something done in the plan of the shown agent kept under this key (`Chat::plans`).
    Plan(String, PlanMsg),
    /// Send the plan the shown agent asks to have approved back to it, with the comments on it
    /// and what is typed in the message box.
    Revise,
    /// Accept the shown agent's word that its job is done, ending it (`true`), or go back to it.
    Accept(bool),
    /// Put that question away and show the agent's work in the diff tab.
    AcceptDiff,
    /// Paste was pressed in the message box: what the clipboard held.
    Clip(Clip),
    /// Take picture `i` off the message being written.
    DropImage(usize),
    /// Follow the shown agent in the editor, or stop.
    Follow,
    /// Show the history tab.
    History,
    HistoryClose,
    /// Show the mailbox's messages (`true`) or the conversations in the history tab.
    HistoryMail(bool),
    /// Select conversation `i` of the list.
    HistoryPick(usize),
    /// Reopen it as an agent, to read or continue.
    HistoryOpen(usize),
    /// Reopen it and walk the editor through its steps.
    HistoryReplay(usize),
    /// Delete it: the first press asks.
    HistoryDelete(usize),
    /// The next step of a replay.
    ReplayTick,
    /// Whether a new agent opens the dropdown (`[ai] pick_model`).
    AskModel(bool),
    /// config.toml couldn't be written: why.
    Saved(Option<String>),
    /// The system prompt was last edited at this `prompt_save_seq`: save it if nothing came after.
    SavePrompt(u64),
    /// A picture in a reply was clicked: open it in a tab (URL as written).
    OpenImage(String),
    /// A mailbox message was clicked: show it in full, or clip it again (its number, see `mail_count`).
    MailOpen(u64),
    /// The copy button of turn `i` of the shown agent: its text to the clipboard (a reply as
    /// the markdown it was written in).
    CopyTurn(usize),
}

impl App {
    pub(super) fn ai_tabs_seen(&self) -> (usize, usize, Option<u64>) {
        self.ai.tabs_seen
    }

    pub(super) fn ai_set_tabs_seen(&mut self, seen: (usize, usize, Option<u64>)) {
        self.ai.tabs_seen = seen;
    }

    /// Answer what the shown agent asked leave for. Questions are answered with a yes once
    /// each has an answer (`form`); a no skips them.
    pub(super) fn ai_answer(&mut self, allow: bool, message: Option<String>) {
        let answers = match &self.ai.ask {
            Some(a) if !a.questions.is_empty() && allow => {
                if !self.ai.form.complete() {
                    return;
                }
                self.ai.form.answers(&a.questions)
            }
            _ => vec![],
        };
        self.ai_send_answer(allow, message, answers);
    }

    /// Send the answer to what the shown agent asked; answers to its questions show in the
    /// transcript as the user's.
    fn ai_send_answer(&mut self, allow: bool, message: Option<String>, answers: Vec<String>) {
        let (Some(ask), Session::Ready(id)) = (self.ai.ask.take(), &self.ai.session) else { return };
        let id = *id;
        let plan = ask.tool == "ExitPlanMode";
        let message = match message {
            None if !ask.questions.is_empty() => Some("The user chose not to answer.".to_string()),
            None if plan && !allow => Some("The user declined this plan. Don't carry it out, and don't hand in another one unasked: stop and wait for what they say next.".to_string()),
            m => m,
        };
        // The plan stays in the feed, folded, with what became of it and the comments sent.
        if let Some(p) = plan.then(|| self.ai.plans.get_mut(ask.step.as_ref().unwrap_or(&ask.request))).flatten() {
            p.comments.cancel();
            p.sel = None;
            p.open = false;
            p.outcome = Some(match (allow, &message) {
                (true, _) => Outcome::Approved,
                (false, Some(m)) if m.starts_with("The user declined") => Outcome::Declined,
                (false, _) => Outcome::Revise,
            });
        }
        if !answers.is_empty() {
            let one = ask.questions.len() == 1;
            let text = ask.questions.iter().zip(&answers).map(|(q, a)| if one { a.clone() } else { format!("{} → {a}", q.question) }).collect::<Vec<_>>().join("\n");
            self.ai.turns.push(Turn { who: Who::User, md: markdown::Content::new(), text, math: Default::default(), images: Default::default(), step: None });
        }
        #[cfg(test)]
        self.ai.answered.push((allow, answers.clone()));
        #[cfg(test)]
        {
            self.ai.told = message.clone();
        }
        self.notify(ClientNotify::AiAnswer { id, request: ask.request, allow, message, answers });
    }

    /// Send the plan the shown agent asks to have approved back for a revision: the comments
    /// on it (a draft with words is filed first) and what is typed as the reply to it all.
    /// Nothing to send: nothing happens.
    pub(super) fn ai_revise(&mut self) -> Task<Message> {
        let reply = self.ai.input.trim().to_string();
        let Some(p) = self.ai.asked_plan_mut() else { return Task::none() };
        p.save();
        if p.comments.is_empty() && reply.is_empty() {
            return Task::none();
        }
        let n = p.comments.len();
        let message = p.feedback(&reply);
        // The transcript says what the user sent, briefly; the plan's card keeps the comments.
        let said = match (n, reply.is_empty()) {
            (0, _) => reply.clone(),
            (n, true) => format!("Sent the plan back with {n} comment{}.", if n == 1 { "" } else { "s" }),
            (n, false) => format!("{reply}\n\n(and {n} comment{} on the plan)", if n == 1 { "" } else { "s" }),
        };
        self.ai.input.clear();
        self.ai.back = 0;
        self.ai.anchor = None;
        self.ai.turns.push(Turn { who: Who::User, md: markdown::Content::new(), text: said, math: Default::default(), images: Default::default(), step: None });
        self.ai_answer(false, Some(message));
        self.ai_scroll_to_end()
    }

    /// A key while the shown agent asks to have a plan approved. The plan is read and answered
    /// from the keyboard, one way for each thing:
    /// - ↑ / ↓ move a cursor over its rows, ⇧ takes in more rows, Esc lets go of them;
    /// - ↵ on picked rows opens a comment on them (or the one there, to change it); while one is
    ///   being written every key goes to it: ↵ files it, ⇧↵ starts a new line, ⇧↑ / ⇧↓ take in
    ///   more rows, Esc drops it;
    /// - with nothing picked, ↵ approves, or sends the plan back once there are comments or
    ///   words in the message box (a reply to the whole plan).
    ///
    /// Nothing here refuses the plan without saying why: Esc only lets go. `None`: the key isn't
    /// the plan's (typing goes to the message box).
    fn ai_plan_key(&mut self, chord: KeyChord, text: Option<&str>) -> Option<Task<Message>> {
        let held = chord.ctrl || chord.logo;
        let plain = !held && !chord.alt;
        let empty = self.ai.input.is_empty() && self.ai.images.is_empty();
        let command = self.keymap.lookup(&chord);
        let p = self.ai.asked_plan_mut()?;
        let vertical = match chord.key {
            Key::Up => Some(-1),
            Key::Down => Some(1),
            _ => None,
        };
        if let Some(d) = p.comments.draft.as_mut() {
            match chord.key {
                Key::Esc => p.cancel(),
                Key::Enter if chord.shift => d.insert("\n"),
                Key::Enter if plain => p.save(),
                Key::Backspace if held => d.backspace_line(),
                Key::Backspace if chord.alt => d.backspace_word(),
                Key::Backspace => d.backspace(),
                _ if vertical.is_some() && chord.shift && plain => p.step(vertical.unwrap(), true),
                _ => match command {
                    Some(Command::Paste) => return Some(iced::clipboard::read().map(|s| Message::Ai(Msg::Pasted(s)))),
                    Some(c) if super::app_wide(c) => return Some(self.exec(c)),
                    _ if !held => d.insert(&text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect::<String>()),
                    _ => {}
                },
            }
            return Some(Task::none());
        }
        match chord.key {
            // The arrows are the plan's while nothing is typed (with words, they move the caret).
            _ if vertical.is_some() && plain && empty => p.step(vertical.unwrap(), chord.shift),
            Key::Esc => p.sel = None,
            Key::Enter if plain && !chord.shift && empty && p.sel.is_some() => {
                p.enter();
            }
            Key::Enter if plain && !chord.shift && empty && p.has_feedback() => return Some(self.ai_revise()),
            _ => return None,
        }
        Some(Task::none())
    }

    /// Answers given from outside (`nib ctl ai answer`), one per question: an option's number
    /// or label, or words.
    pub(super) fn ai_answer_with(&mut self, given: &[String]) -> Result<(), String> {
        let Some(ask) = &self.ai.ask else { return Err("the shown agent isn't waiting for an answer".into()) };
        if ask.questions.len() != given.len() {
            return Err(format!("it asks {} question(s): give one answer for each", ask.questions.len()));
        }
        let answers = question::resolve(&ask.questions, given);
        self.ai_send_answer(true, None, answers);
        Ok(())
    }

    /// An option of the shown agent's questions was picked (a click, a number key). A single
    /// question with a single choice goes at once; otherwise the keys move on to the next
    /// question without an answer.
    fn ai_pick_answer(&mut self, q: usize, o: usize) -> Task<Message> {
        let chat: &mut Chat = &mut self.ai;
        let Some(ask) = &chat.ask else { return Task::none() };
        chat.form.pick(&ask.questions, q, o);
        let multi = ask.questions.get(q).is_some_and(|x| x.multi);
        if !multi {
            if ask.questions.len() == 1 {
                self.ai_answer(true, None);
                return self.ai_scroll_to_end();
            }
            self.ai.form.advance();
        }
        Task::none()
    }

    /// A key while the shown agent asks questions and nothing is typed: a number picks that
    /// option, ↑ / ↓ walk the options, Space picks the one they are on, Tab / Shift+Tab go to
    /// the next / previous question, Enter answers. `None`: the key isn't one of these.
    fn ai_question_key(&mut self, chord: KeyChord) -> Option<Task<Message>> {
        let chat: &mut Chat = &mut self.ai;
        let (ask, form) = (chat.ask.as_ref()?, &mut chat.form);
        let n = ask.questions.len();
        let (q, lit) = (form.q, form.lit);
        match chord.key {
            Key::Char(c @ '1'..='9') if !chord.shift => {
                let o = c as usize - '1' as usize;
                let there = ask.questions.get(q).is_some_and(|x| o < x.options.len());
                return there.then(|| self.ai_pick_answer(q, o));
            }
            Key::Char(' ') => return Some(self.ai_pick_answer(q, lit)),
            Key::Up => form.step(&ask.questions, -1),
            Key::Down => form.step(&ask.questions, 1),
            Key::Tab if n > 1 => form.focus(if chord.shift { (form.q + n - 1) % n } else { (form.q + 1) % n }),
            Key::Enter if !chord.shift => return Some(self.ai_question_enter(None)),
            _ => return None,
        }
        Some(Task::none())
    }

    /// Enter while the shown agent asks questions: what is typed answers the one the keys are
    /// on (else the option the arrows are on is picked, if none is); then on to the next one
    /// without an answer, or, all answered, they go.
    fn ai_question_enter(&mut self, typed: Option<String>) -> Task<Message> {
        let chat: &mut Chat = &mut self.ai;
        let Some(ask) = &chat.ask else { return Task::none() };
        let form = &mut chat.form;
        let q = form.q;
        match typed {
            Some(t) => form.say(&ask.questions, t),
            None if !form.answered(q) => form.pick(&ask.questions, q, form.lit),
            None => {}
        }
        if self.ai.form.complete() {
            self.ai_answer(true, None);
            return self.ai_scroll_to_end();
        }
        self.ai.form.advance();
        Task::none()
    }

    /// The shown agent is the one being followed.
    pub(super) fn ai_following_shown(&self) -> bool {
        self.ai.follow.as_ref().is_some_and(|f| f.chat == self.ai.key)
    }

    /// The shown agent said its job is done and the user hasn't answered.
    pub(super) fn ai_finished_shown(&self) -> bool {
        self.ai.finished.is_some()
    }

    pub(super) fn ai_waiting(&self) -> bool {
        self.ai.ask.is_some()
    }

    /// The shown agent's place in the list.
    pub(super) fn ai_current(&self) -> usize {
        self.ai.cur
    }

    /// The agents with a live session, in the rail's order: the session and what it is called
    /// (for the diff tab's per-agent scopes).
    pub(super) fn ai_agents(&self) -> Vec<(AiId, String)> {
        self.ai
            .chats
            .iter()
            .filter_map(|c| match c.session {
                Session::Ready(id) => Some((id, stage::agent_name(c)?)),
                _ => None,
            })
            .collect()
    }

    /// The session of the agent shown, once it is up.
    pub(super) fn ai_shown_session(&self) -> Option<AiId> {
        match self.ai.session {
            Session::Ready(id) => Some(id),
            _ => None,
        }
    }

    /// The agent with session `id` said its job is done, and the user has not answered yet.
    pub(super) fn ai_said_done(&self, id: AiId) -> bool {
        self.ai.chats.iter().any(|c| matches!(c.session, Session::Ready(s) if s == id) && c.finished.is_some())
    }

    /// Bring the agent with session `id` to the front of the agents tab. `false`: it is gone.
    pub(super) fn ai_show_session(&mut self, id: AiId) -> bool {
        let Some(i) = self.ai.chats.iter().position(|c| matches!(c.session, Session::Ready(s) if s == id)) else { return false };
        self.ai.open = true;
        self.ai.front = true;
        self.focus = Focus::Ai;
        self.ai_select(i);
        true
    }

    /// End the agent with session `id` (its work landed).
    pub(super) fn ai_end_agent(&mut self, id: AiId) -> Task<Message> {
        match self.ai.chats.iter().position(|c| matches!(c.session, Session::Ready(s) if s == id)) {
            Some(i) => self.ai_kill(i),
            None => Task::none(),
        }
    }

    /// Say `text` to the agent with session `id`, as the user would, and show it in its chat.
    pub(super) fn ai_tell(&mut self, id: AiId, text: String) {
        let Some(i) = self.ai.chats.iter().position(|c| matches!(c.session, Session::Ready(s) if s == id)) else { return };
        let c = &mut self.ai.chats[i];
        c.finished = None;
        c.accepting = false;
        c.busy = true;
        c.turns.push(Turn { who: Who::User, text: text.clone(), md: markdown::Content::new(), math: Default::default(), images: Default::default(), step: None });
        self.notify(ClientNotify::AiSend { id, text, attach: Vec::new(), images: Vec::new() });
    }

    /// The agents tab's name in the tab bar: how many are at work.
    pub(super) fn ai_tab_title(&self) -> String {
        match self.ai.chats.iter().filter(|c| c.busy).count() {
            0 => "✦ agents".to_string(),
            n => format!("✦ agents · {n} working"),
        }
    }

    pub(super) fn on_ai_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Toggle if self.ai.shown() => self.ai_close(),
            Msg::Toggle => return self.ai_open(),
            Msg::Close => self.ai_close(),
            Msg::OpenImage(url) => {
                if let Target::File(path) = preview::resolve(&self.ai_base(), &url) {
                    return self.open_path(path, true);
                }
            }
            Msg::NewChat => {
                self.focus = Focus::Ai;
                self.ai.queued = None;
                return match self.ai.agent {
                    Some(agent) => self.ai_new_chat(agent, self.ai.chosen.clone()),
                    None => self.ai_check(None),
                };
            }
            Msg::Switch if self.ai.picking => {
                self.ai.picking = false;
                self.ai.pull_focus = false;
                self.ai.confirm_delete = None;
            }
            Msg::Switch => return self.ai_pick(),
            Msg::Tab(agent) => {
                self.ai.tab = Some(agent);
                self.ai.pick_sel = None;
            }
            Msg::Use(agent, model) => {
                self.focus = Focus::Ai;
                self.ai.last = Some((agent, model.clone()));
                let save = self.ai_save_choice();
                return Task::batch([self.ai_new_chat(agent, model), save]);
            }
            Msg::AskModel(on) => {
                self.ai.pick_model = on;
                return self.ai_save_choice();
            }
            Msg::Saved(Some(why)) => return self.flash_warn(format!("couldn't save to config.toml: {why}")),
            Msg::Saved(None) => {}
            Msg::Setup(agent) => return self.ai_setup(agent, false),
            Msg::Update(agent) => return self.ai_update(agent),
            Msg::Warm(model, warm) => return self.ai_warm(model, warm),
            Msg::PullFocus => {
                self.ai.pull_focus = true;
                self.focus = Focus::Ai;
            }
            Msg::Pull => {
                let model = self.ai.pull_input.clone();
                return self.ai_pull(model);
            }
            Msg::Delete(model) => return self.ai_delete(model),
            Msg::Registry => super::viewers::open_external(OLLAMA_REGISTRY),
            Msg::Focus => {
                if self.focus != Focus::Ai {
                    self.ai.return_to = Some(self.focus).filter(|f| !matches!(f, Focus::Ai | Focus::Toolbar));
                    self.focus = Focus::Ai;
                }
            }
            Msg::Suggest(i) => {
                self.focus = Focus::Ai;
                return self.ai_choose(i, false);
            }
            Msg::Front => {
                self.ai.history.front = false;
                self.ai.front = true;
                self.focus = Focus::Ai;
            }
            Msg::Select(i) => self.ai_select(i),
            Msg::NewAgent => return self.ai_new_agent(),
            // In the history tab the kill shortcut deletes the selected conversation.
            Msg::Kill(_) if self.ai_history_front() => return self.ai_history_delete_selected(),
            Msg::Kill(i) => return self.ai_kill(i),
            Msg::Stop => self.ai_stop(),
            Msg::Fold => self.ai.fold = !self.ai.fold,
            Msg::Side(side) => self.ai.drawer = side,
            Msg::Panic => return self.ai_panic(),
            Msg::PanicMark(i) => {
                if let Some((rows, sel)) = &mut self.ai.panic {
                    if let Some(r) = rows.get_mut(i) {
                        r.1 = !r.1;
                        *sel = i;
                    }
                }
            }
            Msg::PanicRelease => return self.ai_release(),
            Msg::Clock => return self.ai_clock(),
            Msg::Mode(mode) => {
                self.ai.mode = mode;
                if let Session::Ready(id) = self.ai.session {
                    self.ai.told_mode = mode;
                    self.notify(ClientNotify::AiSetMode { id, mode });
                }
            }
            Msg::Effort(e) => {
                self.ai.effort = e.level();
                let save = self.ai_remember_tuning(self.ai.effort.clone(), self.ai.last_prompt_opt());
                // The assistant takes it when its session starts (again, for a first message).
                if self.ai_tuning_fixed() {
                    return Task::batch([save, self.flash_info("thinking level: from this agent's next session (clear starts one)")]);
                }
                return save;
            }
            Msg::Prompt(action) => {
                let edit = action.is_edit();
                self.ai.prompt.perform(action);
                if edit {
                    self.ai.prompt_save_seq += 1;
                    let seq = self.ai.prompt_save_seq;
                    return Task::perform(super::async_sleep(std::time::Duration::from_millis(400)), move |_| Message::Ai(Msg::SavePrompt(seq)));
                }
            }
            Msg::SavePrompt(seq) => {
                if seq == self.ai.prompt_save_seq {
                    let prompt = Some(self.ai.prompt.text().trim().to_string()).filter(|p| !p.is_empty());
                    return self.ai_remember_tuning(self.ai.last_effort.clone(), prompt);
                }
            }
            Msg::Answer(allow) => self.ai_answer(allow, None),
            Msg::Plan(key, m) => {
                let comments = matches!(m, PlanMsg::Row(_));
                if let Some(p) = self.ai.plans.get_mut(&key) {
                    p.update(m);
                }
                // The keys go to the comment being written.
                if comments {
                    self.focus = Focus::Ai;
                }
            }
            Msg::Revise => return self.ai_revise(),
            Msg::Pick(q, o) => return self.ai_pick_answer(q, o),
            Msg::Accept(yes) => return self.ai_accept(yes),
            Msg::AcceptDiff => return self.ai_accept_diff(),
            Msg::Clip(Clip::Text(s)) => {
                self.ai.insert(&s.replace("\r\n", "\n"));
                return self.ai_follow_caret();
            }
            Msg::Clip(Clip::Image(pic)) => {
                self.focus = Focus::Ai;
                self.ai.images.push(pic);
            }
            Msg::Clip(Clip::Nothing) => {}
            Msg::Follow => return self.ai_follow_toggle(),
            Msg::History => return self.ai_history_open(),
            Msg::MailOpen(n) => {
                if !self.ai.mail_open.remove(&n) {
                    self.ai.mail_open.insert(n);
                }
            }
            Msg::CopyTurn(i) => {
                let Some(t) = self.ai.turns.get(i) else { return Task::none() };
                return Task::batch([iced::clipboard::write(t.text.clone()), self.flash_info("copied")]);
            }
            Msg::HistoryClose => self.ai_history_close(),
            Msg::HistoryMail(on) => {
                self.focus = Focus::Ai;
                return self.ai_history_show_mail(on);
            }
            Msg::HistoryPick(i) => {
                self.focus = Focus::Ai;
                self.ai.history_pick(i);
            }
            Msg::HistoryOpen(i) => return self.ai_history_reopen(i, false),
            Msg::HistoryReplay(i) => return self.ai_history_reopen(i, true),
            Msg::HistoryDelete(i) => return self.ai_history_delete(i),
            Msg::ReplayTick => return self.ai_replay_tick(),
            Msg::DroppedPic(Ok(pic)) => {
                self.focus = Focus::Ai;
                self.ai.images.push(pic);
            }
            Msg::DroppedText(Ok((name, text))) => {
                self.focus = Focus::Ai;
                let gap = if self.ai.input[..self.ai.caret()].ends_with(|c: char| !c.is_whitespace()) { " " } else { "" };
                self.ai.insert(&format!("{gap}@{name} "));
                if let Some(text) = text {
                    self.ai.dropped.insert(name, text);
                }
            }
            Msg::DroppedPic(Err(e)) | Msg::DroppedText(Err(e)) => return self.flash_warn(e),
            Msg::DropImage(i) => {
                if i < self.ai.images.len() {
                    self.ai.images.remove(i);
                }
            }
            Msg::Pasted(s) => {
                if let Some(s) = s {
                    if self.ai.pull_focus && self.ai.picking {
                        self.ai.pull_input.push_str(s.trim());
                    } else if let Some(d) = self.ai.asked_plan_mut().and_then(|p| p.comments.draft.as_mut()) {
                        d.insert(&s);
                    } else {
                        self.ai.insert(&s.replace("\r\n", "\n"));
                    }
                }
            }
        }
        Task::none()
    }

    /// Show the panel with the keyboard in it; the first time, find out which assistants there
    /// are (the chat starts with a ready one).
    fn ai_open(&mut self) -> Task<Message> {
        self.ai.open = true;
        self.ai.front = true;
        self.ai.return_to = Some(self.focus).filter(|f| !matches!(f, Focus::Ai | Focus::Toolbar));
        self.focus = Focus::Ai;
        match self.ai.session {
            Session::None if self.ai.setup.is_none() => self.ai_check(None),
            _ => Task::none(),
        }
    }

    /// Open the assistant / model dropdown on the one in use.
    pub(super) fn ai_pick(&mut self) -> Task<Message> {
        self.ai.picking = true;
        self.ai.tab = None;
        self.ai.pick_sel = None;
        // What's installed (and its models) may have changed since.
        self.ai_check(None)
    }

    /// Remember the last pick and the dropdown switch in config.toml, off the UI thread.
    fn ai_save_choice(&self) -> Task<Message> {
        let last = self.ai.last.clone();
        let pick_model = self.ai.pick_model;
        Task::perform(
            async move {
                let pick = last.as_ref().map(|(a, m)| (a.id(), m.as_deref()));
                nib_editor::Config::save_ai_choice(pick, pick_model).err().map(|e| format!("{e:#}"))
            },
            |e| Message::Ai(Msg::Saved(e)),
        )
    }

    /// The thinking levels the shown agent's assistant takes (none: no dropdown).
    pub(super) fn ai_efforts(&self) -> Vec<String> {
        let Some(agent) = self.ai.agent else { return Vec::new() };
        if let Some(levels) = self.ai.chosen.as_ref().and_then(|id| self.ai_models(agent).iter().find(|m| &m.id == id)).map(|m| &m.efforts).filter(|e| !e.is_empty()) {
            return levels.clone();
        }
        agent.efforts().iter().map(|s| (*s).to_string()).collect()
    }

    /// The thinking level and system prompt the shown agent's next session gets with `agent`.
    /// A level this assistant doesn't have (one picked for another) is left to it.
    fn ai_tuning(&self, agent: AiAgent) -> (Option<String>, Option<String>) {
        let levels = if self.ai.agent == Some(agent) { self.ai_efforts() } else { agent.efforts().iter().map(|s| (*s).to_string()).collect() };
        let effort = self.ai.effort.clone().filter(|e| levels.contains(e));
        let prompt = Some(self.ai.prompt.text().trim().to_string()).filter(|p| !p.is_empty());
        (effort, prompt)
    }

    /// Something was said in the shown agent's session: its thinking level and system prompt
    /// can't change until the next one.
    pub(super) fn ai_tuning_fixed(&self) -> bool {
        matches!(self.ai.session, Session::Starting | Session::Ready(_)) && self.ai.turns.iter().any(|t| t.who == Who::User)
    }

    /// Replace the shown agent's system prompt.
    pub(super) fn ai_set_prompt(&mut self, text: &str) {
        self.ai.prompt = iced::widget::text_editor::Content::with_text(text);
    }

    /// An agent starts with this thinking level and system prompt, or they were just set: new
    /// agents start with them too, and so does the next nib (config.toml, off the UI thread,
    /// only when they changed).
    fn ai_remember_tuning(&mut self, effort: Option<String>, prompt: Option<String>) -> Task<Message> {
        let text = prompt.clone().unwrap_or_default();
        if self.ai.last_effort == effort && self.ai.last_prompt == text {
            return Task::none();
        }
        (self.ai.last_effort, self.ai.last_prompt) = (effort.clone(), text);
        Task::perform(
            async move { nib_editor::Config::save_ai_tuning(effort.as_deref(), prompt.as_deref()).err().map(|e| format!("{e:#}")) },
            |e| Message::Ai(Msg::Saved(e)),
        )
    }

    /// The assistant whose models the dropdown shows: the tab picked, else the one in use, else
    /// the first ready one.
    fn ai_shown_tab(&self) -> Option<AiAgent> {
        let agents = self.ai.agents.as_ref()?;
        Some(self.ai.tab.or(self.ai.agent).or_else(|| agents.iter().find(|i| i.ready()).map(|i| i.agent)).unwrap_or(AiAgent::ALL[0]))
    }

    /// The dropdown's model rows for `agent`: Default (`None`), then its models.
    fn ai_pick_rows(&self, agent: AiAgent) -> Vec<Option<String>> {
        let info = self.ai.agents.as_ref().and_then(|a| a.iter().find(|i| i.agent == agent));
        let models = info.map(|i| shown_models(i, self.ai.cloud_models)).unwrap_or_default();
        std::iter::once(None).chain(models.into_iter().map(|m| Some(m.id.clone()))).collect()
    }

    /// The row the arrow keys are on in the dropdown: the one moved to, else the model in use.
    fn ai_pick_cursor(&self, agent: AiAgent, rows: &[Option<String>]) -> usize {
        let current = || (self.ai.agent == Some(agent)).then(|| rows.iter().position(|r| *r == self.ai.chosen)).flatten();
        self.ai.pick_sel.or_else(current).unwrap_or(0).min(rows.len().saturating_sub(1))
    }

    /// Keys while the dropdown is open: ↑↓ a model, ←→ an assistant, Enter picks (and the keys
    /// go on to the message), Esc closes. Others fall through to the message box.
    fn ai_picker_key(&mut self, chord: KeyChord) -> Option<Task<Message>> {
        if chord.ctrl || chord.logo || chord.alt || chord.shift {
            return None;
        }
        let shown = self.ai_shown_tab()?;
        let rows = self.ai_pick_rows(shown);
        let at = self.ai_pick_cursor(shown, &rows);
        match chord.key {
            Key::Up => self.ai.pick_sel = Some(at.saturating_sub(1)),
            Key::Down => self.ai.pick_sel = Some((at + 1).min(rows.len().saturating_sub(1))),
            Key::Left | Key::Right => {
                let agents: Vec<AiAgent> = self.ai.agents.iter().flatten().map(|i| i.agent).collect();
                let i = agents.iter().position(|a| *a == shown).unwrap_or(0);
                let j = if chord.key == Key::Left { i.saturating_sub(1) } else { (i + 1).min(agents.len().saturating_sub(1)) };
                if let Some(a) = agents.get(j) {
                    self.ai.tab = Some(*a);
                    self.ai.pick_sel = None;
                }
            }
            Key::Enter => {
                let ready = self.ai.agents.iter().flatten().any(|i| i.agent == shown && i.ready());
                // Nothing to pick here: Enter sends the message, as it does with the dropdown closed.
                if !ready || self.ai.setup.is_some() || self.ai.update.is_some() {
                    return None;
                }
                let model = rows.get(at).cloned().flatten();
                return Some(self.on_ai_msg(Msg::Use(shown, model)));
            }
            Key::Esc => return Some(self.on_ai_msg(Msg::Switch)),
            _ => return None,
        }
        Some(Task::none())
    }

    /// Ask which assistants the daemon's machine has. `then`: one just set up, to start if ready.
    /// `checking` only while a request is really out: one that couldn't be sent (not connected)
    /// would otherwise leave the panel "looking" forever.
    pub(super) fn ai_check(&mut self, then: Option<AiAgent>) -> Task<Message> {
        let t = self.request(Request::AiAgents, Pending::AiAgents { then });
        self.ai.checking = self.pending.values().any(|p| matches!(p, Pending::AiAgents { .. }));
        t
    }

    /// The daemon went away: nothing outstanding will be answered. Say so instead of waiting.
    pub(super) fn ai_disconnected(&mut self) {
        self.ai.checking = false;
        self.ai.panic = None;
        for c in &mut self.ai.chats {
            if matches!(c.session, Session::Starting | Session::Ready(_)) {
                if c.busy {
                    c.error = Some("disconnected".into());
                }
                c.session = Session::Failed(format!("disconnected from {}", self.label));
            }
            c.busy = false;
            c.frozen = false;
            end_steps(c);
        }
    }

    pub(super) fn on_ai_agents(&mut self, then: Option<AiAgent>, result: Result<Response, RpcError>) -> Task<Message> {
        let ai = &mut self.ai;
        ai.checking = false;
        let agents = match result {
            Ok(Response::AiAgents(a)) => a,
            Ok(_) => return self.flash_warn("unexpected answer from nibd"),
            Err(e) => return self.flash_warn(format!("AI assistants: {}", e.msg)),
        };
        let ready = |a: AiAgent| agents.iter().any(|i| i.agent == a && i.ready());
        ai.setup_failed = then.filter(|a| !ready(*a));
        let running = matches!(ai.session, Session::Ready(_) | Session::Starting);
        // Just set up: chat with it, wherever the keyboard went meanwhile.
        let pick = match then.filter(|a| ready(*a)) {
            Some(a) => Some(a),
            None if running => None,
            None => ai.agent.filter(|a| ready(*a)).or_else(|| agents.iter().find(|i| i.ready()).map(|i| i.agent)),
        };
        let none_ready = !agents.iter().any(|i| i.ready());
        ai.agents = Some(agents);
        if none_ready {
            ai.picking = true;
        }
        match pick {
            Some(agent) if then.is_some() || !running => {
                if then.is_some() {
                    self.ai.open = true;
                    self.focus = Focus::Ai;
                }
                // The same one again keeps its model, if it still has it.
                let listed = |m: &String| {
                    let models = self.ai_models(agent);
                    models.is_empty() || models.iter().any(|x| &x.id == m)
                };
                let model = if self.ai.agent == Some(agent) { self.ai.chosen.clone().filter(listed) } else { None };
                // Started on its own (the panel just opened): a dropdown the user opened
                // meanwhile stays open, on the tab they were looking at.
                let (picking, tab) = (self.ai.picking && then.is_none(), self.ai.tab);
                let t = self.ai_new_chat(agent, model);
                if picking {
                    (self.ai.picking, self.ai.tab) = (true, tab);
                }
                t
            }
            _ => Task::none(),
        }
    }

    /// End the chat there is and start one with `agent` running `model`. A message still waiting
    /// for an assistant (sent before one was ready) goes to this one.
    fn ai_new_chat(&mut self, agent: AiAgent, model: Option<String>) -> Task<Message> {
        self.ai_end_session();
        let ai = &mut self.ai;
        ai.turns.clear();
        // A cleared conversation is a new one, not the past one it was reopened from.
        ai.past = None;
        if let Some(q) = ai.queued.clone() {
            ai.turns.push(Turn { who: Who::User, text: q.clone(), md: markdown::Content::new(), math: Default::default(), images: Default::default(), step: None });
        }
        ai.busy = ai.queued.is_some();
        ai.compacting = false;
        ai.error = None;
        ai.picking = false;
        ai.pull_focus = false;
        ai.confirm_delete = None;
        ai.tab = None;
        ai.setup_failed = None;
        ai.chosen = model;
        // Another assistant's levels are its own: one this one hasn't goes back to its default.
        if ai.effort.as_deref().is_some_and(|e| !agent.efforts().contains(&e)) {
            ai.effort = None;
        }
        self.ai_start(agent)
    }

    /// A terminal that installs and / or signs in `agent`; the chat starts when it's done.
    /// `login`: sign in even if it looks signed in (`/login`).
    fn ai_setup(&mut self, agent: AiAgent, login: bool) -> Task<Message> {
        if let Some((key, _)) = self.ai.setup {
            if self.terms.iter().any(|t| t.key == key) {
                self.terms_visible = true;
                self.set_focus(Focus::Terminal(key));
                return self.flash_info("finish the setup that's running first");
            }
        }
        self.ai.setup_failed = None;
        let (key, task) = self.open_terminal(|cols, rows| Request::AiSetup { agent, cols, rows, login });
        self.ai.setup = Some((key, agent));
        task
    }

    /// A terminal that updates `agent`; the list is asked for again when it's closed.
    fn ai_update(&mut self, agent: AiAgent) -> Task<Message> {
        for (key, what) in [self.ai.setup.map(|(k, _)| (k, "setup")), self.ai.update.map(|(k, _)| (k, "update"))].into_iter().flatten() {
            if self.terms.iter().any(|t| t.key == key) {
                self.terms_visible = true;
                self.set_focus(Focus::Terminal(key));
                return self.flash_info(format!("finish the {what} that's running first"));
            }
        }
        let (key, task) = self.open_terminal(|cols, rows| Request::AiUpdate { agent, cols, rows });
        self.ai.update = Some((key, agent));
        task
    }

    /// Load / unload a local model. It can take a while: the emoji shows it's under way.
    pub(super) fn ai_warm(&mut self, model: String, warm: bool) -> Task<Message> {
        if !self.ai.warming.insert(model.clone()) {
            return Task::none();
        }
        let t = self.request(Request::AiWarm { model: model.clone(), warm, gpu: None }, Pending::AiWarm { model: model.clone(), warm });
        if !self.pending.values().any(|p| matches!(p, Pending::AiWarm { model: m, .. } if *m == model)) {
            self.ai.warming.remove(&model);
        }
        t
    }

    /// Pull Ollama model `model`; the dropdown shows how far it is.
    pub(super) fn ai_pull(&mut self, model: String) -> Task<Message> {
        let model = model.trim().to_string();
        if model.is_empty() {
            return self.flash_info("type a model to pull, like qwen3:8b");
        }
        if let Some(p) = &self.ai.pulling {
            return self.flash_info(format!("one pull at a time: {} is pulling", p.model));
        }
        let t = self.request(Request::AiPull { model: model.clone() }, Pending::AiPull { model: model.clone() });
        if self.pending.values().any(|p| matches!(p, Pending::AiPull { .. })) {
            self.ai.pulling = Some(Pull { model, status: "starting".into(), completed: 0, total: 0 });
            self.ai.pull_input.clear();
            self.ai.pull_focus = false;
        }
        t
    }

    /// One of Ollama's progress lines for the pull under way.
    pub(super) fn on_ai_pull_progress(&mut self, line: &str) {
        let (Some(p), Ok(v)) = (self.ai.pulling.as_mut(), serde_json::from_str::<Value>(line)) else { return };
        if let Some(status) = v.get("status").and_then(Value::as_str) {
            p.status = status.to_string();
        }
        p.total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        p.completed = v.get("completed").and_then(Value::as_u64).unwrap_or(0);
    }

    pub(super) fn on_ai_pulled(&mut self, model: String, result: Result<Response, RpcError>) -> Task<Message> {
        self.ai.pulling = None;
        match result {
            Ok(_) => {
                let t = self.flash_info(format!("pulled {model}"));
                Task::batch([t, self.ai_check(None)])
            }
            Err(e) => self.flash_warn(format!("pulling {model}: {}", e.msg)),
        }
    }

    /// Delete Ollama model `model` (`name:tag`): the first time only asks (the 🗑 turns into
    /// "Delete?"), the second deletes it.
    pub(super) fn ai_delete(&mut self, model: String) -> Task<Message> {
        if self.ai.deleting.contains(&model) {
            return Task::none();
        }
        if self.ai.confirm_delete.as_ref() != Some(&model) {
            self.ai.confirm_delete = Some(model);
            return Task::none();
        }
        self.ai_delete_now(model)
    }

    /// Delete it without asking (`nib ctl ai rm`).
    pub(super) fn ai_delete_now(&mut self, model: String) -> Task<Message> {
        self.ai.confirm_delete = None;
        let t = self.request(Request::AiDelete { model: model.clone() }, Pending::AiDelete { model: model.clone() });
        if self.pending.values().any(|p| matches!(p, Pending::AiDelete { model: m } if *m == model)) {
            self.ai.deleting.insert(model);
        }
        t
    }

    pub(super) fn on_ai_deleted(&mut self, model: String, result: Result<Response, RpcError>) -> Task<Message> {
        self.ai.deleting.remove(&model);
        match result {
            Ok(_) => {
                let t = self.flash_info(format!("deleted {model}"));
                Task::batch([t, self.ai_check(None)])
            }
            Err(e) => self.flash_warn(format!("deleting {model}: {}", e.msg)),
        }
    }

    pub(super) fn on_ai_warmed(&mut self, model: String, warm: bool, result: Result<Response, RpcError>) -> Task<Message> {
        self.ai.warming.remove(&model);
        match result {
            Ok(_) => {
                let models = self.ai.agents.iter_mut().flatten().flat_map(|i| i.models.iter_mut());
                for m in models.filter(|m| m.id == model) {
                    m.warm = Some(warm);
                }
                let short = model.split_once('/').map_or(model.as_str(), |(_, m)| m);
                self.flash_info(if warm { format!("{short} is warm: replies start in seconds") } else { format!("{short} is cold: unloaded") })
            }
            Err(e) => self.flash_warn(format!("{}: {}", if warm { "warming" } else { "cooling" }, e.msg)),
        }
    }

    /// Terminal `key` went away: if it was setting up an assistant, see whether that worked; if it
    /// was updating one, ask again what's there.
    pub(super) fn ai_term_closed(&mut self, key: u64) -> Task<Message> {
        if self.ai.update.is_some_and(|(k, _)| k == key) {
            self.ai.update = None;
            return self.ai_check(None);
        }
        match self.ai.setup {
            Some((k, agent)) if k == key => {
                self.ai.setup = None;
                self.ai_check(Some(agent))
            }
            _ => Task::none(),
        }
    }

    /// For `nib ctl state`.
    pub(super) fn ai_state_json(&self) -> Value {
        let ai = &self.ai;
        let session = match &ai.session {
            Session::None => json!("none"),
            Session::Starting => json!("starting"),
            Session::Ready(_) => json!("ready"),
            Session::Failed(why) => json!({ "failed": why }),
        };
        let agents = ai.agents.as_ref().map(|a| {
            a.iter()
                .map(|i| {
                    let shown = shown_models(i, ai.cloud_models);
                    let models: Vec<&str> = shown.iter().map(|m| m.id.as_str()).collect();
                    let warm: Vec<&str> = shown.iter().filter(|m| m.warm == Some(true)).map(|m| m.id.as_str()).collect();
                    json!({
                        "warm_models": warm,
                        "gpus": i.gpus.iter().map(|g| json!({ "index": g.index, "name": g.name, "memory_mb": g.memory_mb, "kind": g.kind })).collect::<Vec<_>>(),
                        "agent": i.agent.name(),
                        "installed": i.installed,
                        "signed_in": i.signed_in,
                        "can_install": i.can_install,
                        "models": models,
                        "ollama": i.ollama.as_ref().map(|o| json!({
                            "installed": o.installed,
                            "disk_bytes": o.disk_bytes,
                            "models": o.models.iter().map(|m| json!({ "name": m.name, "bytes": m.bytes, "warm": m.warm })).collect::<Vec<_>>(),
                        })),
                    })
                })
                .collect::<Vec<_>>()
        });
        // The dropdown: whether new agents open it, the last pick, and the row the arrow keys are on.
        let cursor = self.ai_shown_tab().filter(|_| ai.picking).map(|a| {
            let rows = self.ai_pick_rows(a);
            json!({ "agent": a.name(), "model": rows.get(self.ai_pick_cursor(a, &rows)).cloned().flatten() })
        });
        let pick = json!({
            "on_new_agent": ai.pick_model,
            "last": ai.last.as_ref().map(|(a, m)| json!({ "agent": a.name(), "model": m })),
            "cursor": cursor,
        });
        let mut state = json!({
            "agent": ai.agent.map(AiAgent::name),
            "session": session,
            "model": ai.model,
            "chosen_model": ai.chosen,
            "agents": agents,
            "checking": ai.checking,
            "picking": ai.picking,
            "setup": ai.setup.map(|(_, a)| a.name()),
            "update": ai.update.map(|(_, a)| a.name()),
            "warming": ai.warming.iter().collect::<Vec<_>>(),
            "pulling": ai.pulling.as_ref().map(|p| json!({ "model": p.model, "status": p.status, "completed": p.completed, "total": p.total })),
            "deleting": ai.deleting.iter().collect::<Vec<_>>(),
            "turns": ai.turns.len(),
            "messages": ai.turns.iter().filter(|t| t.who != Who::Step).map(|t| json!({ "from": match t.who { Who::User => "user", Who::Assistant => "assistant", Who::Note | Who::Step => "nib", Who::Mail => "mail", Who::Done => "finished" }, "text": t.text })).collect::<Vec<_>>(),
            "steps": ai.turns.iter().filter_map(|t| t.step.as_ref()).map(stage::step_json).collect::<Vec<_>>(),
            "shown": ai.shown(),
            "history": self.ai_history_json(),
            "follow": ai.follow.as_ref().map(|f| json!({ "chat": ai.chats.iter().position(|c| c.key == f.chat), "tab": f.tab, "queued": self.ai_follow_queued() })),
            "mode": ai.mode.name(),
            "plan": ai.asked_plan().and_then(|k| ai.plans.get(k)).map(Plan::json),
            "ask": ai.ask.as_ref().map(|a| json!({ "tool": a.tool, "what": a.what, "step": a.step, "questions": a.questions.iter().enumerate().map(|(i, q)| json!({
                "header": q.header,
                "question": q.question,
                "multi": q.multi,
                "options": q.options.iter().map(|o| json!({ "label": o.label, "description": o.description, "preview": o.preview })).collect::<Vec<_>>(),
                "picked": ai.form.picks.get(i),
                "own": ai.form.own.get(i),
            })).collect::<Vec<_>>() })),
            "accepting": ai.accepting,
            "images": ai.images.iter().map(|p| json!({ "width": p.width, "height": p.height, "bytes": p.png.len() })).collect::<Vec<_>>(),
            "current": ai.cur,
            "fold": ai.fold,
            "side": ai.drawer.map_or("none", Side::name),
            "effort": ai.effort,
            "efforts": self.ai_efforts(),
            "prompt": ai.prompt.text().trim_end(),
            "panic": ai.panic.as_ref().map(|(rows, _)| rows.iter().map(|(key, marked)| json!({ "chat": ai.chats.iter().position(|c| c.key == *key), "marked": marked })).collect::<Vec<_>>()),
            "chats": ai.chats.iter().map(|c| json!({
                "agent": c.agent.map(AiAgent::name),
                "model": c.chosen.clone().or_else(|| c.model.clone()),
                "state": stage::state_of(c).0,
                "name": stage::agent_name(c),
                "mode": c.mode.name(),
                "waiting": c.ask.is_some(),
                "finished": c.finished.as_ref().map(|f| json!({ "asked": f.asked, "found": f.found, "did": f.did })),
                "busy": c.busy,
                "frozen": c.frozen,
                "turns": c.turns.len(),
                "steps": c.turns.iter().filter(|t| t.step.is_some()).count(),
            })).collect::<Vec<_>>(),
            "input": ai.input,
            "caret": ai.input[..ai.caret()].chars().count(),
            "menu": self.ai_menu_items().1.iter().map(|s| match s {
                compose::Suggest::Cmd(c) => format!("/{}", c.name),
                compose::Suggest::Skill(s) => format!("/{}", s.name),
                compose::Suggest::Path { path, dir } => format!("@{path}{}", if *dir { "/" } else { "" }),
            }).collect::<Vec<_>>(),
            "compacting": ai.compacting,
            "skills": self.ai_skills().iter().map(|s| json!({ "name": s.name, "group": s.group.label(), "path": s.path })).collect::<Vec<_>>(),
            "busy": ai.busy,
            "error": ai.error,
            "usage": ai.usage.as_ref().map(|u| json!({
                "context_tokens": u.context_tokens,
                "context_window": u.context_window,
                "context_fill": context_fill(u),
                "limits": u.limits.iter().map(|l| json!({ "window_minutes": l.window_minutes, "used": l.used, "resets_at": l.resets_at })).collect::<Vec<_>>(),
                "cost_usd": u.cost_usd,
                "cost_per_hour": self.ai_cost_per_hour(),
                "tokens_per_sec": u.tokens_per_sec,
            })),
        });
        // Outside the macro: one more key there is past `json!`'s recursion limit.
        state["mailbox"] = ai.mailbox.iter().map(|m| json!({ "from": m.from, "to": m.to, "text": m.text })).collect::<Value>();
        state["pick"] = pick;
        state
    }

    /// The close shortcut (⌘W) in the agents area: the history tab closes; on the agents, the
    /// one shown is killed, and a lone blank one closes the area.
    pub(super) fn ai_close_key(&mut self) -> Task<Message> {
        if self.ai_history_front() {
            self.ai_history_close();
            return Task::none();
        }
        let lone_blank = self.ai.chats.len() == 1 && self.ai.chats[0].blank();
        if !self.ai.shown() || lone_blank {
            self.ai_close();
            return Task::none();
        }
        self.ai_kill(self.ai.cur)
    }

    /// Hide the panel. The session lives on, so reopening continues the conversation.
    pub(super) fn ai_close(&mut self) {
        self.ai.open = false;
        self.ai.front = false;
        self.ai_history_close();
        if self.focus == Focus::Ai {
            self.ai_leave();
        }
    }

    /// The keyboard goes back to where it came from.
    fn ai_leave(&mut self) {
        // Back to the file that was in front, if there is one; the agents keep working.
        self.ai.front = self.tabs.is_empty();
        self.focus = match self.ai.return_to.take() {
            Some(Focus::Terminal(k)) if self.terms_visible && self.terms.iter().any(|t| t.key == k) => Focus::Terminal(k),
            Some(Focus::Tree) if self.tree.open => Focus::Tree,
            Some(Focus::Git) if self.git_ui.open => Focus::Git,
            _ => Focus::Main,
        };
    }

    fn ai_start(&mut self, agent: AiAgent) -> Task<Message> {
        self.ai.session = Session::Starting;
        self.ai.usage = None;
        self.ai.since = Some(Instant::now());
        self.ai.agent = Some(agent);
        self.ai.model = None;
        let model = self.ai.chosen.clone();
        // What the `/` menu offers this assistant (asked each chat: skills come and go).
        self.ai.local_skills = compose::local_skills();
        let skills = self.request(Request::AiSkills { agent }, Pending::AiSkills { agent });
        // Every assistant works as an agent, within the mode picked.
        let _ = AiAgent::Claude;
        let (mode, chat) = (self.ai.mode, self.ai.key);
        self.ai.told_mode = mode;
        let (effort, prompt) = self.ai_tuning(agent);
        self.ai.told_tuning = (effort.clone(), prompt.clone());
        let remember = self.ai_remember_tuning(effort.clone(), prompt.clone());
        let open = Request::AiOpen { agent, model, tools: true, mode, resume: self.ai.past.clone(), effort, prompt };
        let t = Task::batch([self.request(open, Pending::AiOpen { chat }), skills, remember]);
        if !self.pending.values().any(|p| matches!(p, Pending::AiOpen { chat: c } if *c == chat)) {
            self.ai.session = Session::Failed(format!("not connected to {}", self.label));
            self.ai.busy = false;
        }
        t
    }

    fn ai_end_session(&mut self) {
        if let Session::Ready(id) = self.ai.session {
            self.notify(ClientNotify::AiClose { id });
        }
        self.ai.session = Session::None;
        self.ai.name = None;
        self.ai.busy = false;
        self.ai.frozen = false;
        self.ai.stopping = false;
        self.ai.resume = false;
    }

    pub(super) fn on_ai_skills(&mut self, agent: AiAgent, result: Result<Response, RpcError>) {
        match result {
            Ok(Response::AiSkills(s)) => {
                for c in self.ai.chats.iter_mut().filter(|c| c.agent == Some(agent)) {
                    c.skills = s.clone();
                }
            }
            Ok(_) => {}
            // An older daemon or none: the menu has the rest.
            Err(e) => eprintln!("nib: skills: {}", e.msg),
        }
    }

    /// `AiOpen` was answered for the chat with key `chat`.
    pub(super) fn on_ai_opened(&mut self, chat: u64, result: Result<Response, RpcError>) -> Task<Message> {
        let Some(i) = self.ai.chats.iter().position(|c| c.key == chat) else {
            // Killed in the meantime.
            if let Ok(Response::AiOpened { id }) = result {
                self.notify(ClientNotify::AiClose { id });
            }
            return Task::none();
        };
        self.ai_as(i, |a| a.ai_opened(result))
    }

    /// Run `f` with chat `i` as the current one (the code for one chat reads `self.ai`).
    fn ai_as(&mut self, i: usize, f: impl FnOnce(&mut Self) -> Task<Message>) -> Task<Message> {
        let shown = self.ai.chats[self.ai.cur].key;
        self.ai.cur = i;
        let t = f(self);
        self.ai.cur = self.ai.chats.iter().position(|c| c.key == shown).unwrap_or(0);
        t
    }

    fn ai_opened(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        // Answered after New chat replaced it: that one is pending now.
        if !matches!(self.ai.session, Session::Starting) {
            if let Ok(Response::AiOpened { id }) = result {
                self.notify(ClientNotify::AiClose { id });
            }
            return Task::none();
        }
        match result {
            Ok(Response::AiOpened { id }) => {
                self.ai.session = Session::Ready(id);
                if self.ai.mode != self.ai.told_mode {
                    let mode = self.ai.mode;
                    self.ai.told_mode = mode;
                    self.notify(ClientNotify::AiSetMode { id, mode });
                }
                if let Some(text) = self.ai.queued.take() {
                    self.ai_send_now(id, text);
                }
            }
            Ok(_) => self.ai.session = Session::Failed("unexpected answer from nibd".into()),
            Err(e) => {
                self.ai.session = Session::Failed(e.msg);
                self.ai.busy = false;
                // Missing or signed out: show what there is and how to fix it.
                if matches!(e.kind, ErrorKind::NotFound | ErrorKind::PermissionDenied) {
                    self.ai.picking = true;
                    return self.ai_check(None);
                }
                self.ai.queued = None;
            }
        }
        Task::none()
    }

    pub(super) fn on_ai_event(&mut self, id: AiId, event: AiEvent) -> Task<Message> {
        match self.ai.chats.iter().position(|c| matches!(c.session, Session::Ready(s) if s == id)) {
            // What a followed agent just did moves the editor, once its event is in.
            Some(i) => {
                let finished = matches!(event, AiEvent::Finished(_));
                let t = self.ai_as(i, |a| a.ai_event(id, event));
                // Said by the agent being looked at: ask now (otherwise once it is tabbed to).
                let scroll = if finished && i == self.ai.cur && self.ai.front && self.focus == Focus::Ai {
                    self.ai.accepting = true;
                    self.ai_scroll_to_end()
                } else {
                    Task::none()
                };
                Task::batch([t, scroll, self.ai_follow_apply()])
            }
            None => Task::none(),
        }
    }

    /// What a reply's relative paths are relative to: a file in the project's root, where the
    /// agents run.
    fn ai_base(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.root).join("_")
    }

    /// Asks the daemon for the pictures the shown chat's last reply refers to that it hasn't yet.
    /// Each reply keeps its own copy, so a plot an agent redraws shows anew in the next reply.
    fn ai_fetch_images(&mut self) -> Task<Message> {
        let base = self.ai_base();
        let chat = self.ai.key;
        let Some(turn) = self.ai.turns.last_mut().filter(|t| t.who == Who::Assistant) else { return Task::none() };
        let mut wanted = Vec::new();
        for url in turn.md.images() {
            if turn.images.contains_key(url.as_str()) || url.starts_with(math::SCHEME) {
                continue;
            }
            turn.images.insert(url.clone(), None);
            if let Target::File(path) = preview::resolve(&base, url) {
                if PreviewImage::wanted(&path) {
                    wanted.push((url.clone(), path));
                }
            }
        }
        Task::batch(wanted.into_iter().map(|(url, path)| self.request(Request::ReadFile { path }, Pending::AiImage { chat, url })))
    }

    /// A picture asked for by `ai_fetch_images` arrived.
    pub(super) fn ai_image(&mut self, chat: u64, url: String, data: Vec<u8>) {
        let Some(c) = self.ai.chats.iter_mut().find(|c| c.key == chat) else { return };
        let Some(turn) = c.turns.iter_mut().rev().find(|t| matches!(t.images.get(&url), Some(None))) else { return };
        let image = PreviewImage::decode(&url, data);
        turn.images.insert(url, image);
    }

    fn ai_event(&mut self, id: AiId, event: AiEvent) -> Task<Message> {
        let login_hint = self.ai_login_hint();
        let root = self.root.clone();
        let ai = &mut self.ai;
        match event {
            AiEvent::Started { model } => ai.model = Some(model),
            AiEvent::Text(t) => {
                // A message sent while it worked (a steer) gets its own reply after the turn's end.
                ai.busy = true;
                if !ai.turns.last().is_some_and(|t| t.who == Who::Assistant) {
                    ai.turns.push(Turn { who: Who::Assistant, text: String::new(), md: markdown::Content::new(), math: Default::default(), images: Default::default(), step: None });
                }
                let turn = ai.turns.last_mut().expect("pushed above");
                turn.text.push_str(&t);
                if math::maybe(&turn.text) {
                    // A formula can span deltas, so with math in the reply it is parsed whole
                    // each time (typeset formulas are cached).
                    let p = math::prepare(&turn.text);
                    turn.md = markdown::Content::parse(&links::autolink(&p.md));
                    turn.math = p.formulas;
                } else if turn.text.contains("://") {
                    // So is a reply with a URL: it is linked once whole, and can span deltas too.
                    turn.md = markdown::Content::parse(&links::autolink(&turn.text));
                } else {
                    turn.md.push_str(&t);
                }
                return self.ai_fetch_images();
            }
            AiEvent::Step(s) => {
                ai.busy = true;
                if let Some(text) = plan_of(&s.kind) {
                    // Asked about later, if at all (a replayed conversation): shown folded. A step
                    // sent again never replaces the plan, nor what the user did in it.
                    ai.plans.entry(s.id.clone()).or_insert_with(|| {
                        let mut plan = Plan::new(&text);
                        plan.open = false;
                        plan
                    });
                }
                // The agent reading a picture: it is shown under the step (read on the daemon's
                // side, where the agent runs), keyed by the path the step names.
                let picture = match &s.kind {
                    AiStepKind::Read { path, .. } if PreviewImage::wanted(path) => Some(path.clone()),
                    _ => None,
                };
                let images = picture.iter().map(|p| (p.clone(), None)).collect();
                ai.turns.push(Turn {
                    who: Who::Step,
                    text: String::new(),
                    md: markdown::Content::new(),
                    math: Default::default(),
                    images,
                    step: Some(Step { id: s.id, parent: s.parent, acts: act::of(&s.kind, &root), kind: s.kind, started: Instant::now(), done: None }),
                });
                if let Some(step) = self.ai.chats[self.ai.cur].turns.last().and_then(|t| t.step.as_ref()).map(step_copy) {
                    self.ai_follow_step(&step);
                }
                let chat = self.ai.key;
                let fetch = match picture {
                    Some(path) => self.request(Request::ReadFile { path: path.clone() }, Pending::AiImage { chat, url: path }),
                    None => Task::none(),
                };
                return Task::batch([fetch, self.ai_arm_clock()]);
            }
            AiEvent::StepDone(d) => {
                let step = ai.turns.iter_mut().rev().filter_map(|t| t.step.as_mut()).find(|s| s.id == d.id).map(|step| {
                    step.done = Some(StepEnd { ok: d.ok, output: d.output, line: d.line, took: step.started.elapsed() });
                    step_copy(step)
                });
                if let Some(step) = step {
                    self.ai_follow_done(&step);
                    return self.ai_file_changed(&step);
                }
            }
            AiEvent::Name(n) => ai.name = Some(n),
            AiEvent::Work(w) => return self.diff_work(id, w),
            AiEvent::Mail(m) => {
                let text = if m.incoming { format!("from {}: {}", m.from, m.text) } else { format!("to {}: {}", m.to, m.text) };
                ai.turns.push(Turn { who: Who::Mail, text, md: markdown::Content::new(), math: Default::default(), images: Default::default(), step: None });
                // Once in the list for everyone: the sender's copy, or nib's own word.
                if !m.incoming || m.from == "nib" {
                    ai.mailbox.push(m);
                    ai.mail_count += 1;
                    let extra = ai.mailbox.len().saturating_sub(200);
                    ai.mailbox.drain(..extra);
                    let first = ai.mail_count - ai.mailbox.len() as u64;
                    ai.mail_open.retain(|&n| n >= first);
                }
            }
            AiEvent::Ask(a) => {
                ai.busy = true;
                ai.form = question::Form::new(&a.questions);
                // A plan: kept under its step, and fresh (the step's copy may be clipped).
                if a.tool == "ExitPlanMode" {
                    let key = a.step.clone().unwrap_or_else(|| a.request.clone());
                    if ai.plans.get(&key).is_none_or(|p| p.text != a.what || p.outcome.is_some()) {
                        ai.plans.insert(key, Plan::new(&a.what));
                    }
                }
                ai.ask = Some(a);
            }
            AiEvent::Finished(f) => {
                ai.turns.push(Turn { who: Who::Done, text: f.text(), md: markdown::Content::new(), math: Default::default(), images: Default::default(), step: None });
                ai.finished = Some(f);
            }
            AiEvent::Mode(m) => {
                ai.mode = m;
                ai.told_mode = m;
            }
            AiEvent::Suspended { on, error } => {
                match error {
                    None => ai.frozen = on,
                    // This machine can't freeze it: stop it instead, and say "continue" on release.
                    Some(_) if on && ai.busy => {
                        ai.frozen = false;
                        ai.resume = true;
                        ai.stopping = true;
                        self.notify(ClientNotify::AiInterrupt { id });
                    }
                    Some(_) => ai.frozen = false,
                }
            }
            AiEvent::TurnDone { error } => {
                ai.busy = false;
                ai.compacting = false;
                ai.ask = None;
                end_steps(ai);
                // Stopped by the user: not an error.
                ai.error = if std::mem::take(&mut ai.stopping) { None } else { error };
                self.ai_follow_turn_done();
                let ai = &mut self.ai;
                if ai.error.as_deref().is_some_and(needs_login) {
                    ai.error = Some(login_hint);
                }
            }
            AiEvent::Compacted { before, after } => {
                let size = match (before, after) {
                    (Some(b), Some(a)) => format!(": {} → {} tokens", short_count(b), short_count(a)),
                    (Some(b), None) => format!(" from {} tokens", short_count(b)),
                    _ => String::new(),
                };
                self.ai_note(format!("Compacted{size}. {} goes on from a summary of the chat so far.", self.ai_name()));
            }
            AiEvent::Usage(u) => ai.usage = Some(u),
            AiEvent::Exit { msg } => {
                ai.busy = false;
                ai.frozen = false;
                ai.ask = None;
                end_steps(ai);
                let name = ai.agent.map_or("The assistant", AiAgent::name);
                let msg = msg.unwrap_or_else(|| format!("{name} exited."));
                ai.session = Session::Failed(if needs_login(&msg) { login_hint } else { msg });
                self.notify(ClientNotify::AiClose { id });
            }
        }
        Task::none()
    }

    fn ai_submit(&mut self) -> Task<Message> {
        let text = self.ai.input.trim().to_string();
        if text.is_empty() && self.ai.images.is_empty() {
            return Task::none();
        }
        // It is waiting for leave: what is typed is the answer (a question's), or a no with what
        // to do instead.
        if self.ai.ask.as_ref().is_some_and(|a| !a.questions.is_empty()) {
            self.ai.input.clear();
            return self.ai_question_enter(Some(text));
        }
        if self.ai.asked_plan().is_some() {
            return self.ai_revise();
        }
        if self.ai.ask.is_some() {
            self.ai.input.clear();
            self.ai.turns.push(Turn { who: Who::User, md: markdown::Content::new(), text: text.clone(), math: Default::default(), images: Default::default(), step: None });
            self.ai_answer(false, Some(text));
            return self.ai_scroll_to_end();
        }
        let text = match self.ai_slash(&text) {
            compose::Slash::Run(t) => return t,
            compose::Slash::Send(t) => t,
            compose::Slash::Text => text,
        };
        // Claude Code takes a message while it works and reads it after its current step. The
        // others answer one message per run.
        let steer = self.ai.busy && self.ai.agent == Some(AiAgent::Claude) && matches!(self.ai.session, Session::Ready(_)) && !self.ai.compacting;
        if self.ai.busy && !steer {
            return self.flash_info(format!("{} is still replying", self.ai_name()));
        }
        // Nothing to send it to yet: the text stays where it is.
        let Some(agent) = self.ai.agent.or_else(|| self.ai_first_ready()) else {
            self.ai.picking = true;
            return if self.ai.checking || self.ai.setup.is_some() {
                self.flash_info("no assistant is ready yet")
            } else {
                Task::batch([self.flash_info("set up an assistant first"), self.ai_check(None)])
            };
        };
        let gifs = self.ai_gif_mentions(&text).map(|e| self.flash_warn(format!("GIF not attached: {e}")));
        self.ai.input.clear();
        self.ai.error = None;
        let shown = match self.ai.images.len() {
            0 => text.clone(),
            1 => format!("{text}\n[1 image]").trim().to_string(),
            n => format!("{text}\n[{n} images]").trim().to_string(),
        };
        self.ai.turns.push(Turn { who: Who::User, md: markdown::Content::new(), text: shown, math: Default::default(), images: Default::default(), step: None });
        self.ai.busy = true;
        // Told to keep going: it isn't done any more.
        self.ai.finished = None;
        // The thinking level or system prompt changed after the session started, before
        // anything was said in it: a new one starts with them, for this message.
        let first = self.ai.turns.iter().filter(|t| t.who == Who::User).count() == 1;
        if first && matches!(self.ai.session, Session::Ready(_)) && self.ai.told_tuning != self.ai_tuning(agent) {
            self.ai_end_session();
            self.ai.busy = true;
        }
        let started = match self.ai.session {
            Session::Ready(id) => {
                self.ai_send_now(id, text);
                Task::none()
            }
            Session::Starting => {
                self.ai.queued = Some(text);
                Task::none()
            }
            // Not running (it failed or exited): start a fresh one for this message.
            Session::None | Session::Failed(_) => {
                self.ai.queued = Some(text);
                self.ai_start(agent)
            }
        };
        Task::batch([started, self.ai_scroll_to_end(), gifs.unwrap_or_else(Task::none)])
    }

    fn ai_first_ready(&self) -> Option<AiAgent> {
        self.ai.agents.as_ref()?.iter().find(|i| i.ready()).map(|i| i.agent)
    }

    fn ai_name(&self) -> &'static str {
        self.ai.agent.map_or("AI", AiAgent::name)
    }

    fn ai_send_now(&mut self, id: AiId, text: String) {
        let (text, attach) = self.ai_outgoing(&text);
        let images = std::mem::take(&mut self.ai.images).into_iter().map(|p| AiImage { media_type: "image/png".into(), data: p.png }).collect();
        self.notify(ClientNotify::AiSend { id, text, attach, images });
    }

    /// The shown chat's key and what each of its turns says (Find, `search.rs`).
    pub(in crate::app) fn ai_transcript(&self) -> (u64, Vec<&str>) {
        (self.ai.key, self.ai.turns.iter().map(|t| t.text.as_str()).collect())
    }

    /// Scroll the transcript so turn `turn` is about in view: its share of the text decides
    /// how far down (turns are drawn at different heights, so it's a guess).
    pub(in crate::app) fn ai_scroll_to_turn(&self, turn: usize) -> Task<Message> {
        let len = |t: &Turn| t.text.len().max(40);
        let total: usize = self.ai.turns.iter().map(len).sum();
        let before: usize = self.ai.turns.iter().take(turn).map(len).sum();
        let share = if total == 0 { 1.0 } else { before as f32 / total as f32 };
        // Pinned to the bottom, offsets count up from the end.
        let y = (1.0 - share).clamp(0.0, 1.0);
        operate(operation::scrollable::snap_to(WidgetId::from(TRANSCRIPT_ID), operation::scrollable::RelativeOffset { x: None, y: Some(y) }))
    }

    fn ai_scroll_to_end(&self) -> Task<Message> {
        operate(operation::scrollable::snap_to(WidgetId::from(TRANSCRIPT_ID), operation::scrollable::RelativeOffset { x: None, y: Some(0.0) }))
    }

    pub(super) fn ai_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        if self.ai.panic.is_some() {
            // Panic itself (to let nothing through by accident, nothing else is app-wide here).
            return self.ai_panic_key(chord.key);
        }
        if self.ai.accepting {
            return self.ai_accept_key(chord);
        }
        // ⌘D: this agent's work in the diff tab, like its pull request (Esc there comes back).
        let see = if cfg!(target_os = "macos") { chord.logo && !chord.ctrl } else { chord.ctrl };
        if see && chord.key == Key::Char('d') && !chord.shift && !chord.alt && !self.ai_history_front() {
            return match self.ai_shown_session() {
                Some(id) => self.diff_show(nib_proto::DiffScope::Agent(id)),
                // Reopened from history, or not started: nib knows its work once it runs.
                None => self.flash_info("this agent is not running yet: send it a message, then its work shows here"),
            };
        }
        if self.ai_history_front() {
            return self.ai_history_key(chord, text);
        }
        // The field is only there while the dropdown is open: closed any way (a model picked, a
        // click beside it), the keys go back to the message.
        if !self.ai.picking {
            self.ai.pull_focus = false;
        }
        if self.ai.pull_focus {
            // The dropdown's pull field: one line, Enter pulls, Esc gives the keys back.
            match chord.key {
                Key::Esc => self.ai.pull_focus = false,
                Key::Enter => return self.on_ai_msg(Msg::Pull),
                Key::Backspace if held || chord.alt => self.ai.pull_input.clear(),
                Key::Backspace => {
                    self.ai.pull_input.pop();
                }
                _ => match self.keymap.lookup(&chord) {
                    Some(Command::Paste) => return iced::clipboard::read().map(|s| Message::Ai(Msg::Pasted(s))),
                    Some(c) if super::app_wide(c) => return self.exec(c),
                    _ if !held => self.ai.pull_input.extend(text.unwrap_or_default().chars().filter(|c| !c.is_control() && !c.is_whitespace())),
                    _ => {}
                },
            }
            return Task::none();
        }
        if self.ai.picking {
            if let Some(t) = self.ai_picker_key(chord) {
                return t;
            }
        }
        if let Some(t) = self.ai_plan_key(chord, text.as_deref()) {
            return t;
        }
        if let Some(t) = self.ai_menu_key(chord) {
            return t;
        }
        // The transcript is anchored to its bottom, so its offsets count up from there: up is +.
        let scroll = |dy: f32| {
            let offset = operation::scrollable::AbsoluteOffset { x: 0.0, y: -dy };
            operate(operation::scrollable::scroll_by(WidgetId::from(TRANSCRIPT_ID), offset))
        };
        // Questions, with nothing typed: the keys pick the answers.
        if self.ai.input.is_empty() && self.ai.images.is_empty() && !held && !chord.alt && self.ai.ask.as_ref().is_some_and(|a| !a.questions.is_empty()) {
            if let Some(t) = self.ai_question_key(chord) {
                return t;
            }
        }
        match chord.key {
            // Waiting for leave: Esc says no, Enter on an empty box says yes.
            Key::Esc if self.ai.ask.is_some() => self.ai_answer(false, None),
            Key::Enter if self.ai.input.is_empty() && self.ai.images.is_empty() && self.ai.ask.as_ref().is_some_and(|a| a.questions.is_empty()) && !held && !chord.alt && !chord.shift => self.ai_answer(true, None),
            Key::Esc if self.ai.selection().is_some() => self.ai.anchor = None,
            Key::Esc => self.ai_leave(),
            // With nothing typed, Tab walks the agents (with text it would be too easy to leave a
            // half-written message by accident; the menu's Tab completes, above).
            Key::Tab if self.ai.input.is_empty() && !held && !chord.alt => self.ai_cycle(chord.shift),
            Key::Enter if chord.shift => self.ai.insert("\n"),
            Key::Enter if !held && !chord.alt => return self.ai_submit(),
            Key::PageUp if !chord.shift => return scroll(-300.0),
            Key::PageDown if !chord.shift => return scroll(300.0),
            _ => {
                let (s, i, sel) = (self.ai.input.as_str(), self.ai.caret(), self.ai.selection());
                let cmd = self.keymap.lookup(&chord).or_else(|| {
                    // Ctrl(+Shift)+←/→ on macOS isn't bound (the editor has ⌥ for words), but in a
                    // message box people reach for it the way they would elsewhere: a word.
                    let arrow = matches!(chord.key, Key::Left | Key::Right);
                    (cfg!(target_os = "macos") && arrow && chord.ctrl && !chord.logo && !chord.alt)
                        .then(|| self.keymap.lookup(&KeyChord { ctrl: false, alt: true, ..chord }))
                        .flatten()
                });
                // Where a move key takes the caret, and whether it drags the selection along.
                let to = match cmd {
                    // With something selected, ← / → land on its edge.
                    Some(Command::MoveLeft) => Some((sel.map_or_else(|| caret::prev_char(s, i), |(a, _)| a), false)),
                    Some(Command::MoveRight) => Some((sel.map_or_else(|| caret::next_char(s, i), |(_, b)| b), false)),
                    Some(Command::MoveUp) => Some((caret::up(s, i), false)),
                    Some(Command::MoveDown) => Some((caret::down(s, i), false)),
                    Some(Command::MoveWordLeft) => Some((caret::word_left(s, i), false)),
                    Some(Command::MoveWordRight) => Some((caret::word_right(s, i), false)),
                    Some(Command::MoveLineStart) => Some((caret::line_start(s, i), false)),
                    Some(Command::MoveLineEnd) => Some((caret::line_end(s, i), false)),
                    Some(Command::MoveDocStart) => Some((0, false)),
                    Some(Command::MoveDocEnd) => Some((s.len(), false)),
                    Some(Command::SelectLeft) => Some((caret::prev_char(s, i), true)),
                    Some(Command::SelectRight) => Some((caret::next_char(s, i), true)),
                    Some(Command::SelectUp) => Some((caret::up(s, i), true)),
                    Some(Command::SelectDown) => Some((caret::down(s, i), true)),
                    Some(Command::SelectWordLeft) => Some((caret::word_left(s, i), true)),
                    Some(Command::SelectWordRight) => Some((caret::word_right(s, i), true)),
                    Some(Command::SelectLineStart) => Some((caret::line_start(s, i), true)),
                    Some(Command::SelectLineEnd) => Some((caret::line_end(s, i), true)),
                    Some(Command::SelectDocStart | Command::SelectPageUp) => Some((0, true)),
                    Some(Command::SelectDocEnd | Command::SelectPageDown) => Some((s.len(), true)),
                    _ => None,
                };
                if let Some((to, extend)) = to {
                    self.ai.move_to(to, extend);
                    return Task::none();
                }
                // Where a delete key takes out text to.
                let del = match chord.key {
                    // Cmd+Backspace: to the start of the line, like a text field on macOS.
                    Key::Backspace if chord.logo => Some(caret::line_start(s, i)),
                    // Alt/Ctrl+Backspace: the word before the caret, like a shell.
                    Key::Backspace if held || chord.alt => Some(caret::word_left(s, i)),
                    Key::Backspace => Some(caret::prev_char(s, i)),
                    Key::Delete if held || chord.alt => Some(caret::word_right(s, i)),
                    Key::Delete => Some(caret::next_char(s, i)),
                    _ => None,
                };
                if let Some(to) = del {
                    self.ai.delete_to(to);
                    return Task::none();
                }
                match cmd {
                    Some(Command::SelectAll) => self.ai.select_all(),
                    Some(Command::Copy) => return self.ai.selected().map_or_else(Task::none, |t| iced::clipboard::write(t.to_string())),
                    Some(Command::Cut) => {
                        let Some(t) = self.ai.selected().map(str::to_string) else { return Task::none() };
                        self.ai.delete_selection();
                        return iced::clipboard::write(t);
                    }
                    Some(Command::Paste) => return Task::perform(async { read_clipboard() }, |c| Message::Ai(Msg::Clip(c))),
                    Some(c) if super::app_wide(c) => return self.exec(c),
                    _ if !held => {
                        let typed: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                        self.ai.insert(&typed);
                    }
                    _ => {}
                }
            }
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    /// What the chat costs the user per hour, at API prices, since it started. `None` in its
    /// first minute (too soon to say) or when there's no cost to tell.
    fn ai_cost_per_hour(&self) -> Option<f64> {
        let cost = self.ai.usage.as_ref()?.cost_usd?;
        let hours = self.ai.since?.elapsed().as_secs_f64() / 3600.0;
        (hours >= 1.0 / 60.0).then(|| cost / hours)
    }

    /// The strip above the message box: how full the context is, and on the right the provider's
    /// usage limit (its shortest window) or the spend per hour, or a local model's speed.
    fn ai_meter(&self) -> Option<Element<'_, Message>> {
        let u = self.ai.usage.as_ref()?;
        let th = &self.theme;
        let (dim, accent, warn, track) = (th.dim, th.accent, th.warn, th.divider);
        let mut line = row![].spacing(8).align_y(Alignment::Center);
        if let Some(tokens) = u.context_tokens {
            match context_fill(u) {
                Some(fill) => {
                    let bar_color = if fill >= 0.8 { warn } else { accent };
                    let bar = progress_bar(0.0..=1.0, fill as f32).length(90).girth(5).style(move |_| progress_bar::Style {
                        background: track.into(),
                        bar: bar_color.into(),
                        border: Border { radius: 3.0.into(), ..Default::default() },
                    });
                    let window = u.context_window.unwrap_or_default();
                    line = line
                        .push(text("Context").size(11).color(dim))
                        .push(bar)
                        .push(text(format!("{:.0}% · {} / {}", fill * 100.0, short_count(tokens), short_count(window))).size(11).color(dim));
                }
                None => line = line.push(text(format!("Context {} tokens", short_count(tokens))).size(11).color(dim)),
            }
        }
        line = line.push(space::horizontal());
        let right = if let Some(tps) = u.tokens_per_sec {
            Some((format!("{tps:.1} tok/s"), dim))
        } else if let Some(l) = u.limits.first() {
            Some((limit_label(l), if l.used >= 0.8 { warn } else { dim }))
        } else {
            u.cost_usd.map(|c| match self.ai_cost_per_hour() {
                Some(rate) => (format!("${c:.2} · ${rate:.2}/h"), dim),
                None => (format!("${c:.2}"), dim),
            })
        };
        if let Some((s, color)) = right {
            line = line.push(text(s).size(11).color(color).wrapping(text::Wrapping::None));
        }
        Some(line.into())
    }

    /// What to say when the assistant answers that it isn't signed in.
    fn ai_login_hint(&self) -> String {
        format!("{} isn't signed in on {}. Type /login to sign in.", self.ai_name(), self.ai_place())
    }

    fn ai_place(&self) -> String {
        if self.label == "local" { "this machine".to_string() } else { self.label.clone() }
    }

    fn ai_intro(&self) -> String {
        let place = self.ai_place();
        match self.ai.agent {
            Some(AiAgent::Claude) => format!("Claude Code, working on {place} with your own sign-in. It reads, edits and runs commands in this project in its auto mode: what it does shows here step by step, and what it runs in its terminal beside this. / for skills and commands, @ for files."),
            Some(a) => format!("{}, working on {place} with your own sign-in. It answers one message per run: in auto mode it edits and runs commands in this project, in plan or manual it only reads, and it can't be asked or steered mid-run. / for skills and commands, @ for files.", a.name()),
            None => format!("Looking for coding assistants on {place}…"),
        }
    }

    /// The models `agent` has, as the daemon listed them.
    fn ai_models(&self, agent: AiAgent) -> &[AiModel] {
        self.ai.agents.as_ref().and_then(|a| a.iter().find(|i| i.agent == agent)).map_or(&[], |i| i.models.as_slice())
    }

    /// The model the chat runs: the one picked, else what the assistant says it runs.
    fn ai_model_label(&self) -> String {
        let ai = &self.ai;
        match (&ai.chosen, ai.agent) {
            (Some(id), Some(agent)) => self.ai_models(agent).iter().find(|m| &m.id == id).map_or(id.clone(), |m| m.name.clone()),
            _ => ai.model.clone().unwrap_or_else(|| "default model".into()),
        }
    }

    /// The dropdown: the assistants on the daemon's machine (model providers) across the top,
    /// and below, the models of the one picked there, or what it takes to chat with it.
    fn ai_picker(&self) -> Element<'_, Message> {
        let ai = &self.ai;
        let th = &self.theme;
        let (dim, fg, accent, sel_bg) = (th.dim, th.foreground, th.accent, th.selection);
        let place = self.ai_place();
        let Some(agents) = &ai.agents else {
            let what = if ai.checking {
                format!("Looking for coding assistants on {place}…")
            } else {
                format!("Not connected to {place}: nib will look for coding assistants once it is.")
            };
            return text(what).size(12).color(dim).into();
        };
        let mut col = column![].spacing(10);
        let busy = ai.setup.is_some() || ai.update.is_some();
        if let Some((_, a)) = ai.setup {
            col = col.push(text(format!("Setting up {} in the terminal below — follow it there. The chat starts when it's done.", a.name())).size(12).color(dim));
        } else if let Some((_, a)) = ai.update {
            col = col.push(text(format!("Updating {} in the terminal below. Press Enter there when it's done.", a.name())).size(12).color(dim));
        } else if !agents.iter().any(|i| i.ready()) {
            col = col.push(
                text(format!("No coding assistant is ready on {place} yet. nib can install one with its vendor's installer and sign you in, right here:"))
                    .size(12)
                    .color(dim),
            );
        }
        if let Some(a) = ai.setup_failed {
            col = col.push(text(format!("{} isn't ready yet — the setup didn't finish.", a.name())).size(12).color(th.warn));
        }

        // The providers: the one shown is the tab picked, else the one in use, else the first
        // ready one.
        let shown = self.ai_shown_tab().unwrap_or(AiAgent::ALL[0]);
        let mut tabs = row![].spacing(4);
        for i in agents {
            let on = i.agent == shown;
            // Ready ones read like the rest of the text, with a lit dot; the others are dimmer,
            // with a hollow one, but still easy to read: they can be installed from here.
            let (color, dot, dot_color) = if i.ready() { (fg, "●", super::tree::GREEN) } else { (dim, "○", dim) };
            let color = if on { fg } else { color };
            let label = row![text(dot).size(8).color(dot_color), text(i.agent.name()).size(11).color(color)].spacing(5).align_y(Alignment::Center);
            let tab = button(label).padding([3, 8]).on_press(Message::Ai(Msg::Tab(i.agent))).style(
                move |_: &iced::Theme, status: button::Status| button::Style {
                    background: (on || matches!(status, button::Status::Hovered)).then(|| sel_bg.into()),
                    border: Border { color: if on { accent } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                },
            );
            tabs = tabs.push(tab);
        }
        col = col.push(tabs);
        let Some(info) = agents.iter().find(|i| i.agent == shown) else {
            return col.into();
        };

        let action = |label: &'static str, msg: Msg| {
            let b = button(text(label).size(11).color(accent)).padding([2, 10]).style(move |_: &iced::Theme, _: button::Status| button::Style {
                border: Border { color: accent, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            });
            if busy { b } else { b.on_press(Message::Ai(msg)) }
        };
        let in_use = ai.agent == Some(shown) && matches!(ai.session, Session::Ready(_) | Session::Starting);
        // OpenCode comes with Ollama: one without the other is set up with the same button.
        let ollama = info.ollama.as_ref();
        let no_ollama = ollama.is_some_and(|o| !o.installed);
        let (status, fix): (&str, Option<(&'static str, Msg)>) = match info {
            i if i.installed && no_ollama && i.can_install => (if in_use { "in use · Ollama not installed" } else { "Ollama not installed" }, Some(("Install Ollama", Msg::Setup(shown)))),
            _ if in_use => ("in use", None),
            i if i.ready() => (if i.signed_in.is_some() { "ready" } else { "installed" }, None),
            i if i.installed => ("signed out", Some(("Sign in", Msg::Setup(shown)))),
            i if i.can_install => ("not installed", Some(("Install", Msg::Setup(shown)))),
            _ => ("not installed", None),
        };
        let mut about = row![text(format!("{} · {status}", shown.vendor())).size(11).color(dim), space::horizontal()]
            .spacing(6)
            .align_y(Alignment::Center);
        if info.installed {
            let update = button(text("Update").size(11).color(fg)).padding([2, 10]).style(move |_: &iced::Theme, status: button::Status| button::Style {
                background: matches!(status, button::Status::Hovered).then(|| sel_bg.into()),
                border: Border { color: th.divider, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            });
            about = about.push(if busy { update } else { update.on_press(Message::Ai(Msg::Update(shown))) });
        }
        if let Some((label, msg)) = fix {
            about = about.push(action(label, msg));
        }
        col = col.push(about);
        // Ollama: its accelerator, what it holds, and pulling another model.
        if let Some(o) = ollama.filter(|o| o.installed && info.installed) {
            col = col.push(self.ai_ollama(o, &info.gpus, busy));
        }

        if info.ready() {
            // "Default" first: whatever it's set up with.
            let current = |id: Option<&str>| in_use && ai.chosen.as_deref() == id;
            let listed = shown_models(info, ai.cloud_models);
            // The row the arrow keys are on: lit and outlined, like a hovered one.
            let cursor = self.ai_pick_cursor(shown, &self.ai_pick_rows(shown));
            // Room for the warm / cold switch on every row when any has one, so names line up.
            let switches = listed.iter().any(|m| m.warm.is_some());
            const SWITCH: f32 = 50.0;
            let model_row = |row_i: usize, name: String, id: Option<&AiModel>| {
                let on = current(id.map(|m| m.id.as_str()));
                let kb = row_i == cursor;
                let mut line = row![text(if on { "●" } else { " " }).size(10).color(accent).width(12), text(name).size(12).color(fg)]
                    .spacing(6)
                    .align_y(Alignment::Center);
                if let Some(m) = id.filter(|m| m.name != m.id) {
                    line = line.push(space::horizontal()).push(text(m.id.clone()).size(11).color(dim));
                }
                let msg = Msg::Use(shown, id.map(|m| m.id.clone()));
                let b = button(line).width(Length::Fill).padding([3, 6]).style(move |_: &iced::Theme, status: button::Status| button::Style {
                    background: (on || kb || matches!(status, button::Status::Hovered)).then(|| sel_bg.into()),
                    border: Border { color: if kb { accent } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                });
                let b = if busy { b } else { b.on_press(Message::Ai(msg)) };
                // A downloaded model: its size, and 🗑 to delete it (asks once first).
                let tail = id.and_then(|m| ollama_model(ollama, &m.id)).map(|om| {
                    let name = om.name.clone();
                    let (asking, deleting) = (ai.confirm_delete.as_ref() == Some(&name), ai.deleting.contains(&name));
                    let (label, color) = match (deleting, asking) {
                        (true, _) => ("⏳", dim),
                        (_, true) => ("Delete?", th.warn),
                        _ => ("🗑", dim),
                    };
                    let warn = th.warn;
                    let del = button(text(label).size(11).color(color)).padding([2, 6]).style(move |_: &iced::Theme, status: button::Status| button::Style {
                        background: matches!(status, button::Status::Hovered).then(|| sel_bg.into()),
                        border: Border { color: if asking { warn } else { Color::TRANSPARENT }, width: 1.0, radius: 4.0.into() },
                        ..Default::default()
                    });
                    let del = if busy || deleting { del } else { del.on_press(Message::Ai(Msg::Delete(name))) };
                    row![text(crate::ai_cli::human_bytes(om.bytes)).size(11).color(dim), del].spacing(4).align_y(Alignment::Center)
                });
                let tail: Element<'_, Message> = match tail {
                    Some(t) => t.into(),
                    None => space().width(0).into(),
                };
                // A local model's state: ❄️ cold | 🔥 warm, the current one lit. Clicking the
                // other one switches to it; the one switched to shows ⏳ until it's done.
                let Some(warm) = id.and_then(|m| m.warm) else {
                    let r = if switches { row![space().width(SWITCH), b, tail] } else { row![b, tail] };
                    return Element::from(r.spacing(4).align_y(Alignment::Center));
                };
                let id = id.map(|m| m.id.clone()).unwrap_or_default();
                let toggling = ai.warming.contains(&id);
                // ❄ has no colour glyph in every font: it takes the text colour, so make it icy.
                let ice = Color::from_rgb8(0x8e, 0xcb, 0xff);
                let side = |is_warm: bool| {
                    let lit = warm == is_warm;
                    let icon = match (is_warm, toggling && !lit) {
                        (_, true) => "⏳",
                        (true, _) => "🔥",
                        (false, _) => "❄️",
                    };
                    let color = if lit || toggling { ice } else { dim };
                    let seg = button(text(icon).size(13).color(color).center()).width(SWITCH / 2.0).padding([2, 0]).style(
                        move |_: &iced::Theme, status: button::Status| button::Style {
                            background: (lit || (!toggling && matches!(status, button::Status::Hovered))).then(|| sel_bg.into()),
                            border: Border { color: if lit { accent } else { Color::TRANSPARENT }, width: 1.0, radius: 9.0.into() },
                            ..Default::default()
                        },
                    );
                    if lit || toggling { seg } else { seg.on_press(Message::Ai(Msg::Warm(id.clone(), is_warm))) }
                };
                let switch = container(row![side(false), side(true)]).style(move |_| container::Style {
                    border: Border { color: th.divider, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                });
                row![switch, b, tail].spacing(4).align_y(Alignment::Center).into()
            };
            let mut models = column![model_row(0, "Default".into(), None)].spacing(1);
            for (i, m) in listed.iter().copied().enumerate() {
                models = models.push(model_row(i + 1, m.name.clone(), Some(m)));
            }
            col = col.push(models);
            if switches {
                col = col.push(
                    text("🔥 warm: loaded and primed for OpenCode · ❄️ cold: loads on the first message")
                        .size(11)
                        .color(dim),
                );
            }
            for m in listed.iter().filter(|m| ai.warming.contains(&m.id)) {
                let short = m.id.split_once('/').map_or(m.id.as_str(), |(_, n)| n);
                let what = if m.warm == Some(true) {
                    format!("⏳ Cooling {short}: unloading it.")
                } else {
                    format!("⏳ Warming {short}: loading it and reading OpenCode's instructions once, so your first message is quick. A big model takes a minute or two.")
                };
                col = col.push(text(what).size(11).color(fg));
            }
            let note = match shown {
                AiAgent::Grok => Some("nib can't list Grok's models: Default runs the one it's set up with.".to_string()),
                AiAgent::OpenCode if info.models.is_empty() => {
                    Some("OpenCode listed no models: set up a provider for it (such as a local Ollama one), then reopen this.".to_string())
                }
                AiAgent::OpenCode if listed.is_empty() && ollama.is_some_and(|o| o.installed) => {
                    Some("No local models yet: pull one above.".to_string())
                }
                AiAgent::OpenCode if listed.is_empty() => Some("No local models yet: install Ollama, then pull one.".to_string()),
                _ => None,
            };
            if let Some(n) = note {
                col = col.push(text(n).size(11).color(dim));
            }
            col = col.push(text("Picking a model starts a new chat. ↑↓ model · ←→ assistant · Enter pick · Esc close").size(11).color(dim));
        } else if shown == AiAgent::OpenCode && info.can_install && !busy {
            col = col.push(
                text("Install runs OpenCode's and Ollama's own installers on that machine (Ollama may ask for your password in the terminal). Then download a model here.")
                    .size(11)
                    .color(dim),
            );
        } else if info.can_install && !info.installed && !busy {
            col = col.push(text("Install runs the vendor's curl installer on that machine, then its sign-in: open the link it shows on any device.").size(11).color(dim));
        } else if shown == AiAgent::OpenCode && !info.installed {
            col = col.push(
                text("OpenCode chats with local models through Ollama. nib can't install them here: get them from opencode.ai and ollama.com.")
                    .size(11)
                    .color(dim),
            );
        }
        // Whether new agents ask: off, they start with the last model picked.
        let ask = ai.pick_model;
        let toggle = button(
            row![text(if ask { "☑" } else { "☐" }).size(12).color(if ask { accent } else { dim }), text("Open this for every new agent (off: start with the last model)").size(11).color(dim)]
                .spacing(6)
                .align_y(Alignment::Center),
        )
        .padding([2, 0])
        .on_press(Message::Ai(Msg::AskModel(!ask)))
        .style(|_: &iced::Theme, _| button::Style::default());
        col = col.push(toggle);
        let panel = th.panel;
        container(col).padding([10, 12]).width(Length::Fill).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: dim, width: 1.0, radius: 6.0.into() },
            shadow: iced::Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.6), offset: iced::Vector::new(0.0, 4.0), blur_radius: 14.0 },
            ..Default::default()
        })
        .into()
    }
}

impl App {
    /// The OpenCode tab's Ollama part: the accelerator it runs on (a warning when that's the
    /// CPU), what its models take on disk, and pulling another, with the pull under way.
    fn ai_ollama<'a>(&'a self, o: &'a OllamaInfo, gpus: &[nib_proto::AiGpu], busy: bool) -> Element<'a, Message> {
        let (ai, th) = (&self.ai, &self.theme);
        let (dim, fg, accent, sel_bg, bg) = (th.dim, th.foreground, th.accent, th.selection, th.background);
        let (accel, accel_color) = match gpus {
            [gpu] => (gpu.name.clone(), dim),
            [_, _, ..] => {
                let names: Vec<String> = gpus.iter().map(|g| format!("{} {}", g.index, g.name)).collect();
                (format!("{} GPUs: {}", gpus.len(), names.join(", ")), dim)
            }
            [] => ("⚠ no GPU: CPU only, slow".to_string(), th.warn),
        };
        let n = o.models.len();
        let held = format!("{n} model{} · {}", if n == 1 { "" } else { "s" }, crate::ai_cli::human_bytes(o.disk_bytes));
        let head = row![
            text("Ollama").size(11).color(fg),
            text(accel).size(11).color(accel_color).width(Length::Fill),
            text(held).size(11).color(dim)
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let focused = ai.pull_focus;
        let shown = match (ai.pull_input.is_empty(), focused) {
            (true, false) => text("qwen3:8b").size(12).color(dim),
            (_, true) => text(format!("{}▏", ai.pull_input)).size(12).color(fg),
            (false, false) => text(ai.pull_input.clone()).size(12).color(fg),
        };
        let field = button(shown).width(Length::Fill).padding([4, 8]).on_press(Message::Ai(Msg::PullFocus)).style(
            move |_: &iced::Theme, status: button::Status| button::Style {
                background: Some(bg.into()),
                border: Border {
                    color: if focused || matches!(status, button::Status::Hovered) { accent } else { dim },
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            },
        );
        let can = !busy && ai.pulling.is_none() && !ai.pull_input.trim().is_empty();
        let pull = button(text("Pull").size(11).color(if can { accent } else { dim })).padding([4, 12]).style(
            move |_: &iced::Theme, status: button::Status| button::Style {
                background: (can && matches!(status, button::Status::Hovered)).then(|| sel_bg.into()),
                border: Border { color: if can { accent } else { dim }, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            },
        );
        let pull = if can { pull.on_press(Message::Ai(Msg::Pull)) } else { pull };
        let search = button(text("ollama.com ↗").size(11).color(accent)).padding([4, 4]).on_press(Message::Ai(Msg::Registry)).style(
            move |_: &iced::Theme, status: button::Status| button::Style {
                background: matches!(status, button::Status::Hovered).then(|| sel_bg.into()),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            },
        );
        let mut col = column![head, row![field, pull, search].spacing(6).align_y(Alignment::Center)].spacing(6);
        if let Some(p) = &ai.pulling {
            let how_far = if p.total > 0 {
                format!(" · {} / {}", crate::ai_cli::human_bytes(p.completed), crate::ai_cli::human_bytes(p.total))
            } else {
                String::new()
            };
            col = col.push(text(format!("{}: {}{how_far}", p.model, p.status)).size(11).color(fg));
            let fill = if p.total > 0 { p.completed as f32 / p.total as f32 } else { 0.0 };
            let track = th.divider;
            col = col.push(progress_bar(0.0..=1.0, fill).girth(4).style(move |_| progress_bar::Style {
                background: track.into(),
                bar: accent.into(),
                border: Border { radius: 2.0.into(), ..Default::default() },
            }));
        }
        col.into()
    }
}

/// The Ollama model behind OpenCode id `id` (`ollama/qwen3:8b`), when it's downloaded.
fn ollama_model<'a>(o: Option<&'a OllamaInfo>, id: &str) -> Option<&'a OllamaModel> {
    let model = id.split_once('/').map_or(id, |(_, m)| m);
    let tagged = model.rsplit('/').next().is_some_and(|last| last.contains(':'));
    let name = if tagged { model.to_string() } else { format!("{model}:latest") };
    o?.models.iter().find(|m| m.name == name)
}

/// How full the context is, 0 to 1: `None` without both counts.
fn context_fill(u: &AiUsage) -> Option<f64> {
    let (used, window) = (u.context_tokens?, u.context_window.filter(|w| *w > 0)?);
    Some((used as f64 / window as f64).min(1.0))
}

/// `4525` -> `4.5k`, `258400` -> `258k`, `1000000` -> `1M`.
fn short_count(n: u64) -> String {
    let (v, unit) = match n {
        n if n >= 1_000_000 => (n as f64 / 1e6, "M"),
        n if n >= 1_000 => (n as f64 / 1e3, "k"),
        n => return n.to_string(),
    };
    let s = if v < 10.0 { format!("{v:.1}") } else { format!("{v:.0}") };
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// `5h limit 25% · 2h 14m left`.
fn limit_label(l: &AiLimit) -> String {
    let window = match l.window_minutes {
        m if m % (24 * 60) == 0 => format!("{}d", m / (24 * 60)),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    };
    let mut s = format!("{window} limit {:.0}%", l.used * 100.0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    if let Some(left) = l.resets_at.map(|at| at - now).filter(|left| *left > 0) {
        let (d, h, m) = (left / 86400, left % 86400 / 3600, left % 3600 / 60);
        let when = if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else { format!("{}m", m.max(1)) };
        s.push_str(&format!(" · {when} left"));
    }
    s
}

/// The models the dropdown lists for `info`. OpenCode is the one for your own models: unless
/// `[ai] cloud_models` is on, only those downloaded to the machine. The others are cloud
/// services only, so all of theirs.
fn shown_models(info: &AiAgentInfo, cloud_models: bool) -> Vec<&AiModel> {
    info.models.iter().filter(|m| m.local || cloud_models || info.agent != AiAgent::OpenCode).collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;
    use nib_proto::{AiOption, AiQuestion, ReqId, ServerMsg, ServerNotify};

    fn key(a: &mut App, s: &str) {
        let _ = a.update(Message::Key { chord: KeyChord::parse(s).unwrap(), text: None, repeat: false });
        let _ = a.update(Message::KeyUp(KeyChord::parse(s).unwrap().key));
    }
    fn type_str(a: &mut App, s: &str) {
        for c in s.chars() {
            let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(c)), text: Some(c.to_string()), repeat: false });
            let _ = a.update(Message::KeyUp(Key::Char(c)));
        }
    }
    fn opening(a: &App) -> Option<ReqId> {
        a.pending.iter().find_map(|(id, p)| matches!(p, Pending::AiOpen { .. }).then_some(*id))
    }
    fn event(a: &mut App, id: AiId, event: AiEvent) {
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::AiEvent { id, event }));
    }

    fn checking(a: &App) -> Option<(ReqId, Option<AiAgent>)> {
        a.pending.iter().find_map(|(id, p)| match p {
            Pending::AiAgents { then } => Some((*id, *then)),
            _ => None,
        })
    }
    fn info(agent: AiAgent, installed: bool, signed_in: Option<bool>) -> AiAgentInfo {
        AiAgentInfo { agent, installed, signed_in, can_install: true, models: Vec::new(), gpus: Vec::new(), ollama: None }
    }
    /// Answer the outstanding `AiAgents`.
    fn agents(a: &mut App, list: Vec<AiAgentInfo>) {
        let (req, _) = checking(a).expect("asking which assistants there are");
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiAgents(list)) });
    }
    fn none_ready() -> Vec<AiAgentInfo> {
        vec![info(AiAgent::Claude, false, None), info(AiAgent::Codex, true, Some(false)), info(AiAgent::Grok, false, None)]
    }

    #[test]
    fn a_message_typed_while_starting_goes_out_once_ready() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        assert_eq!(a.focus, Focus::Ai);
        assert!(opening(&a).is_none(), "first, which assistants are there");
        agents(&mut a, vec![info(AiAgent::Claude, false, None), info(AiAgent::Codex, true, Some(true)), info(AiAgent::Grok, true, Some(true))]);
        let req = opening(&a).expect("the first ready one starts");
        assert_eq!(a.ai.agent, Some(AiAgent::Codex));
        assert!(!a.ai.picking);
        type_str(&mut a, "hi");
        key(&mut a, "shift+enter");
        type_str(&mut a, "there");
        key(&mut a, "enter");
        assert_eq!(a.ai.queued.as_deref(), Some("hi\nthere"));
        assert!(a.ai.input.is_empty() && a.ai.busy);
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 7 }) });
        assert!(matches!(a.ai.session, Session::Ready(7)) && a.ai.queued.is_none());

        event(&mut a, 7, AiEvent::Text("Hel".into()));
        event(&mut a, 7, AiEvent::Text("lo".into()));
        event(&mut a, 3, AiEvent::Text("another session".into()));
        event(&mut a, 7, AiEvent::TurnDone { error: None });
        let texts: Vec<_> = a.ai.turns.iter().map(|t| (t.who == Who::User, t.text.as_str())).collect();
        assert_eq!(texts, [(true, "hi\nthere"), (false, "Hello")]);
        assert!(!a.ai.busy);

        key(&mut a, "esc");
        assert!(a.ai.shown() && a.focus == Focus::Ai, "with no file to go back to, the agents tab stays in front");
        let _ = a.update(Message::Control(Command::ToggleAi));
        assert!(!a.ai.open && a.focus == Focus::Main, "closing the tab gives the keys back; the session lives on");
        assert!(matches!(a.ai.session, Session::Ready(7)));
    }

    /// A chat with Claude Code, ready as session 7.
    fn chatting() -> App {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, vec![info(AiAgent::Claude, true, Some(true))]);
        let req = opening(&a).expect("starts");
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 7 }) });
        // Not the skills of wherever the tests run.
        a.ai.local_skills.clear();
        a
    }

    fn step(id: &str, kind: AiStepKind) -> AiEvent {
        AiEvent::Step(nib_proto::AiStep { id: id.into(), parent: None, kind })
    }
    fn done(id: &str, ok: bool, output: &str) -> AiEvent {
        AiEvent::StepDone(nib_proto::AiStepDone { id: id.into(), ok, output: output.into(), line: None })
    }
    fn command(c: &str) -> AiStepKind {
        AiStepKind::Command { command: c.into(), description: None, timeout_ms: None, background: false }
    }
    fn say(a: &mut App, s: &str) {
        type_str(a, s);
        key(a, "enter");
    }

    #[test]
    fn what_an_agent_does_shows_as_steps_with_a_clock_for_waits() {
        let mut a = chatting();
        say(&mut a, "go");
        assert_eq!(stage::state_of(&a.ai).0, "thinking");
        event(&mut a, 7, step("t1", AiStepKind::Edit { path: "/tmp/src/a.rs".into(), old: "beta".into(), new: "gamma".into() }));
        assert_eq!(stage::state_of(&a.ai).0, "editing a.rs");
        event(&mut a, 7, AiEvent::StepDone(nib_proto::AiStepDone { id: "t1".into(), ok: true, output: "updated".into(), line: Some(7) }));
        event(&mut a, 7, step("t2", command("sleep 30")));
        assert_eq!(stage::state_of(&a.ai).0, "in its terminal");
        assert!(a.ai.ticking, "a running command starts the clock");
        let s = a.ai_state_json();
        assert_eq!(s["steps"][0]["kind"], "edit");
        assert_eq!((&s["steps"][0]["running"], &s["steps"][0]["line"]), (&json!(false), &json!(7)));
        assert_eq!(s["steps"][1]["what"]["wait"], json!({ "seconds": 30.0, "timeout": false, "counts_down": true }));
        assert_eq!(s["steps"][1]["clock"], "00:30");
        assert_eq!(s["chats"][0]["state"], "in its terminal");
        // Text after a step is a new paragraph of the reply, after the step.
        event(&mut a, 7, done("t2", true, ""));
        event(&mut a, 7, AiEvent::Text("Done.".into()));
        event(&mut a, 7, AiEvent::TurnDone { error: None });
        let order: Vec<Who> = a.ai.turns.iter().map(|t| t.who).collect();
        assert_eq!(order, [Who::User, Who::Step, Who::Step, Who::Assistant]);
        assert_eq!(stage::state_of(&a.ai).0, "idle");
        let _ = a.update(Message::Ai(Msg::Clock));
        assert!(!a.ai.ticking, "nothing runs: the clock stops");
    }

    #[test]
    fn a_message_to_a_working_agent_steers_it_and_stop_is_not_an_error() {
        let mut a = chatting();
        say(&mut a, "go");
        event(&mut a, 7, step("t1", command("cargo test")));
        say(&mut a, "skip the slow tests");
        assert_eq!(a.ai.turns.iter().filter(|t| t.who == Who::User).count(), 2, "Claude Code takes it mid-turn");
        assert!(a.ai.busy);
        let _ = a.update(Message::Ai(Msg::Stop));
        assert!(a.ai.stopping);
        event(&mut a, 7, AiEvent::TurnDone { error: Some("error_during_execution".into()) });
        assert!(a.ai.error.is_none() && !a.ai.busy);
        let s = a.ai_state_json();
        assert_eq!((&s["steps"][0]["running"], &s["steps"][0]["ok"]), (&json!(false), &json!(false)), "the step it was in is over");
    }

    #[test]
    fn a_turns_copy_button_copies_it() {
        let mut a = chatting();
        say(&mut a, "first");
        let _ = a.update(Message::Ai(Msg::CopyTurn(5)));
        assert!(a.flash.is_none(), "no such turn: nothing happens");
        let _ = a.update(Message::Ai(Msg::CopyTurn(0)));
        assert_eq!(a.flash.as_ref().map(|f| f.0.as_str()), Some("copied"));
    }

    #[test]
    fn several_agents_each_get_their_own_events() {
        let mut a = chatting();
        say(&mut a, "first");
        let _ = a.update(Message::Control(Command::NewAgent));
        assert_eq!((a.ai.chats.len(), a.ai.cur), (2, 1));
        assert!(a.ai.picking && a.ai.turns.is_empty(), "a blank agent, asking who it is");
        let _ = a.update(Message::Control(Command::NewAgent));
        assert_eq!(a.ai.chats.len(), 2, "a blank one is reused, not piled up");
        let _ = a.update(Message::Ai(Msg::Use(AiAgent::Claude, None)));
        let req = opening(&a).expect("the new one starts");
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 9 }) });
        assert!(matches!(a.ai.chats[1].session, Session::Ready(9)) && matches!(a.ai.chats[0].session, Session::Ready(7)));
        // The first agent answers while the second is shown.
        event(&mut a, 7, AiEvent::Text("from the first".into()));
        assert_eq!(a.ai.cur, 1);
        assert!(a.ai.turns.is_empty());
        assert_eq!(a.ai.chats[0].turns.last().map(|t| t.text.as_str()), Some("from the first"));
        assert_eq!(a.ai_tab_title(), "✦ agents · 1 working");

        // Tab walks the agents while nothing is typed; with text it doesn't.
        key(&mut a, "tab");
        assert_eq!(a.ai.cur, 0);
        key(&mut a, "shift+tab");
        assert_eq!(a.ai.cur, 1);
        type_str(&mut a, "x");
        key(&mut a, "tab");
        assert_eq!(a.ai.cur, 1);
        assert_eq!(a.ai.input, "x", "the draft stays with its agent");
        let _ = a.update(Message::Control(Command::NextAgent));
        assert!(a.ai.cur == 0 && a.ai.input.is_empty());

        // Killing one leaves the other; killing the last leaves a blank one.
        let _ = a.update(Message::Control(Command::KillAgent));
        assert_eq!(a.ai.chats.len(), 1);
        assert!(matches!(a.ai.session, Session::Ready(9)) && a.ai.input == "x");
        let _ = a.update(Message::Control(Command::KillAgent));
        assert!(a.ai.chats.len() == 1 && matches!(a.ai.session, Session::None) && a.ai.turns.is_empty());
    }

    #[test]
    fn a_finished_agent_is_accepted_with_cmd_enter_or_kept_with_any_other_key() {
        let done = || AiEvent::Finished(nib_proto::AiFinished { asked: "Fix the sum".into(), found: None, did: "Fixed calc.py".into() });
        let accept = if cfg!(target_os = "macos") { "cmd+enter" } else { "ctrl+enter" };
        let mut a = chatting();
        say(&mut a, "first");
        let _ = a.update(Message::Control(Command::NewAgent));
        let _ = a.update(Message::Ai(Msg::Use(AiAgent::Claude, None)));
        let req = opening(&a).unwrap();
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 9 }) });
        say(&mut a, "second");
        // The first finishes while the second is shown: marked, not asked yet.
        event(&mut a, 7, done());
        assert!(a.ai.chats[0].finished.is_some() && !a.ai.accepting);
        assert_eq!(a.ai.chats[0].turns.last().map(|t| (t.who, t.text.as_str())), Some((Who::Done, "Asked: Fix the sum\nDone: Fixed calc.py")));
        // Tabbed to: asked. Any other key goes back to the chat, and is not typed.
        key(&mut a, "tab");
        assert!(a.ai.cur == 0 && a.ai.accepting);
        type_str(&mut a, "x");
        assert!(!a.ai.accepting && a.ai.input.is_empty() && a.ai.finished.is_some());
        // Asked again next time; ⌘↵ accepts and ends it.
        key(&mut a, "tab");
        key(&mut a, "tab");
        assert!(a.ai.cur == 0 && a.ai.accepting);
        key(&mut a, accept);
        assert_eq!(a.ai.chats.len(), 1);
        assert!(matches!(a.ai.session, Session::Ready(9)));
        // Finishing while shown asks at once; writing to it again means it isn't done.
        event(&mut a, 9, done());
        assert!(a.ai.accepting);
        key(&mut a, "esc");
        assert!(!a.ai.accepting && a.ai.chats.len() == 1);
        say(&mut a, "one more thing");
        assert!(a.ai.finished.is_none());
    }

    #[test]
    fn cmd_d_in_an_agents_chat_shows_its_work_and_esc_comes_back() {
        let mut a = chatting();
        say(&mut a, "add a sum");
        key(&mut a, if cfg!(target_os = "macos") { "cmd+d" } else { "ctrl+d" });
        assert!(a.diff_active() && a.focus == Focus::Main);
        assert_eq!(a.diff_json()["scope"], "agent:7");
        key(&mut a, "esc");
        assert!(a.ai.shown() && a.focus == Focus::Ai && a.ai.input.is_empty());
    }

    #[test]
    fn an_agent_on_its_own_branch_is_seen_in_the_diff_tab_and_landed_on_accept() {
        let (accept, see) = if cfg!(target_os = "macos") { ("cmd+enter", "cmd+d") } else { ("ctrl+enter", "ctrl+d") };
        let done = || AiEvent::Finished(nib_proto::AiFinished { asked: "Add a sum".into(), found: None, did: "Added sum()\nand a test".into() });
        let landing = |a: &App| a.pending.iter().find_map(|(id, p)| matches!(p, Pending::WorkLand { id: 7 }).then_some(*id));
        let mut a = chatting();
        say(&mut a, "add a sum");
        let work = nib_proto::AiWork { branch: Some("nib/feat/sum".into()), dir: Some("/w/sum".into()), base: Some("main".into()), plan: None, review: None };
        event(&mut a, 7, AiEvent::Work(work));
        event(&mut a, 7, done());
        assert!(a.ai.accepting);
        // ⌘D: the question is put away and its work shows, like a pull request.
        key(&mut a, see);
        assert!(!a.ai.accepting && a.diff_active() && a.ai.finished.is_some());
        assert_eq!(a.diff_json()["scope"], "agent:7");
        // ⌘↵ there accepts: the branch is landed first, and the agent stays until that worked.
        key(&mut a, accept);
        let req = landing(&a).expect("asked to land");
        assert_eq!(a.ai.chats.len(), 1);
        // It could not be: nothing ended, and the agent is told why, in its chat.
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::GitRan { ok: false, output: "CONFLICT (content): a.rs".into() }) });
        assert!(a.ai.shown() && a.focus == Focus::Ai && a.ai.finished.is_none() && a.ai.busy);
        let last = a.ai.turns.last().unwrap();
        assert!(last.who == Who::User && last.text.contains("CONFLICT") && last.text.contains("nib-work sync"), "{}", last.text);
        // It fixes that and says done again; accepted from the dialog, it lands and is ended.
        event(&mut a, 7, AiEvent::TurnDone { error: None });
        event(&mut a, 7, done());
        assert!(a.ai.accepting);
        key(&mut a, accept);
        let req = landing(&a).expect("asked to land again");
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::GitRan { ok: true, output: "abc1234 Added sum() and a test\nlanded nib/feat/sum on main".into() }) });
        assert!(!a.ai.chats.iter().any(|c| matches!(c.session, Session::Ready(7))), "the agent is ended");
        assert!(a.diff_branch(7).is_none());
    }

    #[test]
    fn panic_pauses_every_agent_and_stops_the_marked_ones() {
        let mut a = chatting();
        let _ = a.update(Message::Control(Command::Panic));
        assert!(a.ai.panic.is_none(), "nobody is working");
        say(&mut a, "first");
        let _ = a.update(Message::Control(Command::NewAgent));
        let _ = a.update(Message::Ai(Msg::Use(AiAgent::Claude, None)));
        let req = opening(&a).unwrap();
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 9 }) });
        say(&mut a, "second");
        event(&mut a, 9, step("t1", command("cargo build")));

        let _ = a.update(Message::Control(Command::Panic));
        assert_eq!(a.ai.panic.as_ref().map(|(rows, _)| rows.clone()), Some(vec![(0, true), (1, true)]), "all marked to begin with");
        assert!(a.ai.chats.iter().all(|c| c.frozen));
        assert_eq!(stage::state_of(&a.ai).0, "paused");
        type_str(&mut a, "x");
        key(&mut a, "esc");
        assert!(a.ai.panic.is_some() && a.ai.input.is_empty(), "only Enter lets go; nothing is typed meanwhile");
        // Keep the second (row 1), stop the first.
        key(&mut a, "down");
        let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(' ')), text: Some(" ".into()), repeat: false });
        key(&mut a, "enter");
        assert!(a.ai.panic.is_none() && a.ai.chats.iter().all(|c| !c.frozen));
        assert!(a.ai.chats[0].stopping && !a.ai.chats[1].stopping);
        event(&mut a, 7, AiEvent::TurnDone { error: Some("error_during_execution".into()) });
        assert!(a.ai.chats[0].error.is_none() && !a.ai.chats[0].busy && a.ai.chats[1].busy);
    }

    #[test]
    fn where_agents_cannot_be_frozen_panic_interrupts_and_says_continue() {
        let mut a = chatting();
        say(&mut a, "go");
        let _ = a.update(Message::Ai(Msg::Panic));
        event(&mut a, 7, AiEvent::Suspended { on: true, error: Some("this machine can't suspend processes".into()) });
        assert!(!a.ai.frozen && a.ai.resume && a.ai.stopping);
        event(&mut a, 7, AiEvent::TurnDone { error: Some("error_during_execution".into()) });
        assert!(a.ai.error.is_none() && !a.ai.busy);
        // Unmarked: it is told to carry on.
        let _ = a.update(Message::Ai(Msg::PanicMark(0)));
        let _ = a.update(Message::Ai(Msg::PanicRelease));
        assert!(a.ai.busy && !a.ai.resume);
        assert_eq!(a.ai.turns.last().map(|t| t.who), Some(Who::Note));
    }

    fn ask(tool: &str, question: bool) -> AiEvent {
        AiEvent::Ask(AiAsk { request: "r1".into(), step: None, tool: tool.into(), what: "rm -rf build".into(), questions: if question { questions(&[("Rename it?", false)]) } else { vec![] } })
    }

    fn questions(qs: &[(&str, bool)]) -> Vec<AiQuestion> {
        let option = |l: &str| AiOption { label: l.into(), description: String::new(), preview: None };
        qs.iter().map(|&(q, multi)| AiQuestion { header: String::new(), question: q.into(), options: vec![option("Yes"), option("No"), option("Maybe")], multi }).collect()
    }

    /// What went to the daemon last, if it was an answer: allowed, and the answers.
    fn last_answer(a: &App) -> Option<(bool, Vec<String>)> {
        a.ai.answered.last().cloned()
    }

    #[test]
    fn a_plan_is_approved_or_sent_back_with_a_reply_or_comments() {
        const PLAN: &str = "# Cache\n\nWhy: slow.\n\n1. Add a cache\n2. Use it";
        let mut a = chatting();
        say(&mut a, "plan it");
        let input = json!({ "plan": PLAN }).to_string();
        let ask_plan = |a: &mut App, n: u32| {
            let id = format!("t{n}");
            event(a, 7, step(&id, AiStepKind::Other { tool: "ExitPlanMode".into(), input: input.clone() }));
            event(a, 7, AiEvent::Ask(AiAsk { request: format!("r{n}"), step: Some(id), tool: "ExitPlanMode".into(), what: PLAN.into(), questions: vec![] }));
        };
        // A: Enter approves.
        ask_plan(&mut a, 1);
        assert_eq!(a.ai_state_json()["plan"]["rows"], json!([[1, 1], [3, 3], [5, 5], [6, 6]]));
        key(&mut a, "enter");
        assert_eq!(last_answer(&a), Some((true, vec![])));
        assert_eq!(a.ai.plans["t1"].outcome, Some(Outcome::Approved));
        // Esc never answers: it only lets go of the rows picked.
        ask_plan(&mut a, 2);
        key(&mut a, "down");
        key(&mut a, "esc");
        assert!(a.ai.ask.is_some() && a.ai.plans["t2"].sel.is_none());
        key(&mut a, "esc");
        assert!(a.ai.ask.is_some(), "nothing is refused without a reason");
        say(&mut a, "drop it, fix the bug first");
        assert_eq!(a.ai.plans["t2"].outcome, Some(Outcome::Revise));
        // C: words send it back for a revision.
        ask_plan(&mut a, 3);
        say(&mut a, "split step 2");
        let told = a.ai.told.clone().unwrap();
        assert!(told.contains("split step 2") && told.contains("ExitPlanMode"), "{told}");
        assert_eq!(a.ai.plans["t3"].outcome, Some(Outcome::Revise));
        // D: comments from the keyboard on lines 3-5, then Enter sends them.
        ask_plan(&mut a, 4);
        key(&mut a, "down");
        key(&mut a, "down");
        key(&mut a, "shift+down");
        key(&mut a, "enter");
        type_str(&mut a, "why a cache?");
        key(&mut a, "shift+enter");
        type_str(&mut a, "measure first");
        assert!(a.ai.ask.is_some() && a.ai.input.is_empty(), "the comment takes the keys, not the message box");
        key(&mut a, "enter");
        assert_eq!(a.ai_state_json()["plan"]["comments"][0]["start_line"], 3);
        assert_eq!(a.ai_state_json()["plan"]["comments"][0]["end_line"], 5);
        assert!(a.ai.ask.is_some(), "filing a comment doesn't answer");
        key(&mut a, "enter");
        let told = a.ai.told.clone().unwrap();
        assert!(told.contains("On lines 3–5:\n> Why: slow.\n>\n> 1. Add a cache\nwhy a cache?\nmeasure first"), "{told}");
        assert_eq!(a.ai.plans["t4"].outcome, Some(Outcome::Revise));
        assert_eq!(a.ai.turns.last().map(|t| t.text.as_str()), Some("Sent the plan back with 1 comment."));
    }

    #[test]
    fn an_agent_that_asks_leave_waits_for_enter_esc_or_words() {
        let mut a = chatting();
        let _ = a.update(Message::Ai(Msg::Mode(AiMode::Manual)));
        assert_eq!(a.ai_state_json()["mode"], "manual");
        say(&mut a, "clean up");
        event(&mut a, 7, ask("Bash", false));
        assert_eq!(stage::state_of(&a.ai).0, "waiting for you");
        key(&mut a, "enter");
        assert!(a.ai.ask.is_none() && a.ai.busy, "Enter on an empty box allows; it works on");

        event(&mut a, 7, ask("Bash", false));
        say(&mut a, "use cargo clean instead");
        assert!(a.ai.ask.is_none());
        assert_eq!(a.ai.turns.last().map(|t| t.text.as_str()), Some("use cargo clean instead"), "words refuse, and say what to do");

        event(&mut a, 7, ask("Bash", false));
        key(&mut a, "esc");
        assert!(a.ai.ask.is_none() && a.ai.shown(), "Esc refuses; it doesn't leave the tab");

        // A question is answered: Enter picks the option the arrows are on.
        event(&mut a, 7, ask("AskUserQuestion", true));
        key(&mut a, "down");
        key(&mut a, "enter");
        assert!(a.ai.ask.is_none());
        assert_eq!(last_answer(&a), Some((true, vec!["No".into()])));
        // A number picks, and a single one goes at once.
        event(&mut a, 7, ask("AskUserQuestion", true));
        key(&mut a, "3");
        assert_eq!(last_answer(&a), Some((true, vec!["Maybe".into()])));
        // Words answer it too.
        event(&mut a, 7, ask("AskUserQuestion", true));
        say(&mut a, "only the module");
        assert_eq!(last_answer(&a), Some((true, vec!["only the module".into()])));
        assert_eq!(a.ai.turns.last().map(|t| t.text.as_str()), Some("only the module"));
        // Esc skips it.
        event(&mut a, 7, ask("AskUserQuestion", true));
        key(&mut a, "esc");
        assert_eq!(last_answer(&a), Some((false, vec![])));
        // Several: each is answered before they go; a multiple choice takes several picks.
        let several = AiAsk { request: "r2".into(), step: None, tool: "AskUserQuestion".into(), what: String::new(), questions: questions(&[("Rename?", false), ("Where?", true)]) };
        event(&mut a, 7, AiEvent::Ask(several));
        key(&mut a, "2");
        assert_eq!(a.ai.form.q, 1, "on to the next question");
        key(&mut a, "1");
        key(&mut a, "3");
        assert!(a.ai.ask.is_some(), "a multiple choice waits for Enter");
        key(&mut a, "enter");
        assert_eq!(last_answer(&a), Some((true, vec!["No".into(), "Yes, Maybe".into()])));
        assert_eq!(a.ai.turns.last().map(|t| t.text.as_str()), Some("Rename? → No\nWhere? → Yes, Maybe"));

        // The harness says which mode it is in (a plan approved, a switch confirmed).
        event(&mut a, 7, AiEvent::Mode(AiMode::AcceptEdits));
        assert_eq!(a.ai.mode, AiMode::AcceptEdits);
        event(&mut a, 7, ask("Bash", false));
        event(&mut a, 7, AiEvent::TurnDone { error: None });
        assert!(a.ai.ask.is_none(), "a turn that ended asks nothing");
    }

    #[test]
    fn a_pasted_picture_goes_with_the_next_message() {
        let mut a = chatting();
        let pic = Pic::from_rgba(image::RgbaImage::from_pixel(3000, 1500, image::Rgba([1, 2, 3, 255]))).unwrap();
        assert_eq!((pic.width, pic.height), (2000, 1000), "big ones are scaled down");
        let _ = a.update(Message::Ai(Msg::Clip(Clip::Image(pic.clone()))));
        let _ = a.update(Message::Ai(Msg::Clip(Clip::Image(pic))));
        let _ = a.update(Message::Ai(Msg::DropImage(0)));
        assert_eq!(a.ai_state_json()["images"].as_array().map(Vec::len), Some(1));
        key(&mut a, "enter");
        assert!(a.ai.images.is_empty() && a.ai.busy, "a picture alone is a message");
        assert_eq!(a.ai.turns.last().map(|t| t.text.as_str()), Some("[1 image]"));
        let _ = a.update(Message::Ai(Msg::Clip(Clip::Text("pasted\r\ntext".into()))));
        assert_eq!(a.ai.input, "pasted\ntext");
    }

    #[test]
    fn a_picture_the_agent_reads_is_fetched_for_its_step() {
        let mut a = chatting();
        say(&mut a, "go");
        event(&mut a, 7, step("r1", AiStepKind::Read { path: "/tmp/shot.png".into(), offset: None, limit: None }));
        let asked = a.pending.values().find_map(|p| match p {
            Pending::AiImage { url, .. } => Some(url.clone()),
            _ => None,
        });
        assert_eq!(asked.as_deref(), Some("/tmp/shot.png"));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 3, image::Rgba([9, 9, 9, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        a.ai_image(a.ai.key, "/tmp/shot.png".into(), png);
        assert!(matches!(a.ai.turns.last().and_then(|t| t.images.get("/tmp/shot.png")), Some(Some(_))));
        // A source file is not a picture: nothing is fetched for it.
        event(&mut a, 7, step("r2", AiStepKind::Read { path: "/tmp/a.rs".into(), offset: None, limit: None }));
        assert!(a.ai.turns.last().is_some_and(|t| t.images.is_empty()));
    }

    #[test]
    fn following_an_agent_moves_the_editor_with_it() {
        let mut a = chatting();
        say(&mut a, "go");
        let _ = a.update(Message::Ai(Msg::Follow));
        assert_eq!(a.ai_state_json()["follow"]["chat"], 0);
        // It reads a file: that file comes forward at the line.
        event(&mut a, 7, step("t1", AiStepKind::Read { path: "/tmp/src/a.rs".into(), offset: Some(40), limit: None }));
        let opening = a.pending.values().find_map(|p| match p {
            Pending::Open { path, goto, .. } => Some((path.clone(), goto.map(|g| g.line))),
            _ => None,
        });
        assert_eq!(opening, Some(("/tmp/src/a.rs".to_string(), Some(39))));
        assert!(!a.ai.shown(), "the file is in front, not the agents tab");
        // It runs a command: queued, then back to the agents tab, where its terminal is (the
        // file never opened here: given up after a while).
        event(&mut a, 7, step("t2", command("cargo test")));
        assert!(!a.ai.shown(), "shown in turn, not at once");
        let _ = a.ai_follow_advance(Duration::from_secs(5));
        assert!(a.ai.shown() && a.focus == Focus::Ai);
        // Panic from the agents tab marks everyone; while watching a file, only the followed one.
        event(&mut a, 7, step("t3", AiStepKind::Read { path: "/tmp/src/b.rs".into(), offset: None, limit: None }));
        let _ = a.ai_follow_advance(Duration::from_secs(1));
        assert!(!a.ai.shown());
        let _ = a.update(Message::Control(Command::Panic));
        assert_eq!(a.ai.panic.as_ref().map(|(rows, _)| rows.clone()), Some(vec![(0, true)]));
        key(&mut a, "enter");
        // Typing in the editor ends it; the button does too.
        let _ = a.update(Message::Ai(Msg::Follow));
        let _ = a.ai_follow_interrupted();
        assert!(a.ai.follow.is_none());
        let _ = a.update(Message::Ai(Msg::Follow));
        let _ = a.update(Message::Ai(Msg::Follow));
        assert!(a.ai.follow.is_none());
    }

    #[test]
    fn a_followed_edit_is_typed_into_the_tab_then_read_from_disk() {
        let mut a = chatting();
        say(&mut a, "go");
        let _ = a.update(Message::Ai(Msg::Follow));
        event(&mut a, 7, step("e1", AiStepKind::Edit { path: "/tmp/src/a.rs".into(), old: "beta".into(), new: "gamma ray".into() }));
        let open = a.pending.iter().find_map(|(id, p)| matches!(p, Pending::Open { .. }).then_some(*id)).expect("opens the file");
        // Answered after the edit landed: the show puts the old text back to type the new.
        let _ = a.on_server(ServerMsg::Response { id: open, result: Ok(Response::File { path: "/tmp/src/a.rs".into(), data: b"alpha\ngamma ray\n".to_vec(), mtime_ns: 5 }) });
        event(&mut a, 7, done("e1", true, ""));
        let text = |a: &App| a.tabs[a.active].doc.text.to_string();
        let mut seen = Vec::new();
        for _ in 0..200 {
            let _ = a.ai_follow_advance(Duration::from_millis(40));
            if seen.last() != Some(&text(&a)) {
                seen.push(text(&a));
            }
        }
        assert!(seen.iter().any(|t| t == "alpha\nbeta\n"), "the old text is put back first: {seen:?}");
        assert!(seen.iter().any(|t| t == "alpha\ngam\n" || t.starts_with("alpha\nga") && t.len() < "alpha\ngamma ray\n".len()), "typed bit by bit: {seen:?}");
        assert_eq!(text(&a), "alpha\ngamma ray\n");
        assert!(!a.tabs[a.active].doc.is_modified(), "never a dirty tab");
        assert!(a.pending.values().any(|p| matches!(p, Pending::Reload { .. })), "read again from disk once shown");
        assert!(!a.ai_follow_playing());
    }

    fn past(id: &str, title: &str) -> nib_proto::AiPast {
        nib_proto::AiPast {
            id: id.into(),
            agent: AiAgent::Claude,
            model: Some("claude-haiku-5-5".into()),
            name: Some("fix-sum-calc".into()),
            title: title.into(),
            started: 0,
            updated: 0,
            messages: 1,
            steps: 1,
            files: vec!["calc.py".into()],
            last: "Fixed.".into(),
            hit: None,
        }
    }
    fn pending(a: &App, want: impl Fn(&Pending) -> bool) -> Option<ReqId> {
        a.pending.iter().find_map(|(id, p)| want(p).then_some(*id))
    }
    fn answer(a: &mut App, id: ReqId, r: Response) {
        let _ = a.on_server(ServerMsg::Response { id, result: Ok(r) });
    }

    #[test]
    fn history_lists_searches_reopens_continues_and_deletes() {
        let mut a = chatting();
        let _ = a.update(Message::Control(Command::AgentHistory));
        assert!(a.ai_history_front() && a.ai.shown());
        let req = pending(&a, |p| matches!(p, Pending::AiHistory)).expect("asks for the list");
        answer(&mut a, req, Response::AiHistory(vec![past("h1", "Fix the sum"), past("h2", "Write tests")]));
        assert_eq!(a.ai_state_json()["history"]["items"].as_array().map(Vec::len), Some(2));
        // Typing searches (the daemon does the looking); the message box gets none of it.
        type_str(&mut a, "sum");
        assert_eq!(a.ai_state_json()["history"]["query"], "sum");
        assert!(a.ai.input.is_empty());
        let req = pending(&a, |p| matches!(p, Pending::AiHistory)).unwrap();
        answer(&mut a, req, Response::AiHistory(vec![past("h1", "Fix the sum")]));

        // Enter reopens it as an agent: everything that happened, nothing running.
        key(&mut a, "enter");
        let req = pending(&a, |p| matches!(p, Pending::AiHistoryLoad { replay: false })).unwrap();
        let entries = vec![
            nib_proto::AiPastEntry::User("Fix the sum".into()),
            nib_proto::AiPastEntry::Step(nib_proto::AiStep { id: "t1".into(), parent: None, kind: command("python3 calc.py") }),
            nib_proto::AiPastEntry::StepDone { done: nib_proto::AiStepDone { id: "t1".into(), ok: true, output: "5".into(), line: None }, seconds: 2.0 },
            nib_proto::AiPastEntry::Text("Fixed.".into()),
        ];
        answer(&mut a, req, Response::AiPastLoaded { past: past("h1", "Fix the sum"), entries });
        assert!(!a.ai_history_front() && a.ai.shown(), "the agents are in front again");
        assert_eq!(a.ai.chats.len(), 2);
        let order: Vec<Who> = a.ai.turns.iter().map(|t| t.who).collect();
        assert_eq!(order, [Who::User, Who::Step, Who::Assistant]);
        assert!(matches!(a.ai.session, Session::None) && !a.ai.busy);
        assert_eq!(a.ai.name.as_deref(), Some("fix-sum-calc"));
        let s = a.ai_state_json();
        assert_eq!((&s["steps"][0]["ok"], &s["steps"][0]["output"]), (&json!(true), &json!("5")));

        // A message continues it: the session that starts resumes the past one.
        say(&mut a, "and now subtract");
        let resumed = a.pending.values().any(|p| matches!(p, Pending::AiOpen { .. }));
        assert!(resumed && a.ai.past.as_deref() == Some("h1"));

        // Reopening it again goes to the agent that has it. Delete asks first.
        let _ = a.update(Message::Control(Command::AgentHistory));
        let req = pending(&a, |p| matches!(p, Pending::AiHistory)).unwrap();
        answer(&mut a, req, Response::AiHistory(vec![past("h1", "Fix the sum"), past("h2", "Write tests")]));
        let _ = a.update(Message::Ai(Msg::HistoryOpen(0)));
        assert!(pending(&a, |p| matches!(p, Pending::AiHistoryLoad { .. })).is_none() && a.ai.chats.len() == 2 && !a.ai_history_front());
        let _ = a.update(Message::Control(Command::AgentHistory));
        let _ = a.update(Message::Ai(Msg::HistoryDelete(1)));
        assert!(pending(&a, |p| matches!(p, Pending::AiHistoryDelete)).is_none(), "the first press only asks");
        let _ = a.update(Message::Ai(Msg::HistoryDelete(1)));
        assert!(pending(&a, |p| matches!(p, Pending::AiHistoryDelete)).is_some());
        assert_eq!(a.ai_state_json()["history"]["items"].as_array().map(Vec::len), Some(1));
        // Tab: the mailbox's messages, searched the same way; Tab again: the conversations.
        key(&mut a, "tab");
        let req = pending(&a, |p| matches!(p, Pending::AiMailHistory)).expect("asks for the letters");
        let letter = nib_proto::AiLetter { from: "fix-sum".into(), to: "all".into(), text: "changing calc.py".into(), at: 0 };
        answer(&mut a, req, Response::AiMailHistory(vec![letter]));
        let h = &a.ai_state_json()["history"];
        assert_eq!((&h["view"], h["letters"].as_array().map(Vec::len)), (&json!("mailbox"), Some(1)));
        let deletes = |a: &App| a.pending.values().filter(|p| matches!(p, Pending::AiHistoryDelete)).count();
        let before = deletes(&a);
        let _ = a.update(Message::Control(Command::KillAgent));
        let _ = a.update(Message::Control(Command::KillAgent));
        assert_eq!(deletes(&a), before, "nothing to delete in the mailbox");
        key(&mut a, "tab");
        assert!(pending(&a, |p| matches!(p, Pending::AiHistory)).is_some());
        assert_eq!(a.ai_state_json()["history"]["view"], json!("conversations"));
        key(&mut a, "esc");
        assert!(!a.ai_history_front() && a.ai.shown(), "Esc goes back to the agents");

        // ⌘W on the agents kills the one shown; on a lone blank one it closes the area.
        let n = a.ai.chats.len();
        let _ = a.update(Message::Control(Command::CloseTab));
        assert_eq!(a.ai.chats.len(), n - 1);
        while a.ai.chats.len() > 1 || !a.ai.chats[0].blank() {
            let _ = a.update(Message::Control(Command::CloseTab));
        }
        assert!(a.ai.shown());
        let _ = a.update(Message::Control(Command::CloseTab));
        assert!(!a.ai.shown(), "a lone blank agent: the area closes");
    }

    #[test]
    fn slash_commands_run_from_the_menu_or_typed_out() {
        let mut a = chatting();
        key(&mut a, "enter");
        type_str(&mut a, "/compact");
        key(&mut a, "enter");
        assert!(!a.ai.compacting && a.ai.input.is_empty(), "nothing said yet: nothing to compact");

        type_str(&mut a, "hi");
        key(&mut a, "enter");
        event(&mut a, 7, AiEvent::Text("Hello".into()));
        event(&mut a, 7, AiEvent::TurnDone { error: None });

        type_str(&mut a, "/co");
        assert_eq!(a.ai_state_json()["menu"], json!(["/compact"]));
        key(&mut a, "tab");
        assert_eq!(a.ai.input, "/compact ", "tab completes, leaving room for a focus");
        type_str(&mut a, "the parser");
        assert_eq!(a.ai_state_json()["menu"], json!([]));
        key(&mut a, "enter");
        assert!(a.ai.compacting && a.ai.busy && a.ai.input.is_empty());
        event(&mut a, 7, AiEvent::Compacted { before: Some(45_000), after: Some(900) });
        event(&mut a, 7, AiEvent::TurnDone { error: None });
        assert!(!a.ai.compacting && !a.ai.busy);
        let note = a.ai.turns.last().unwrap();
        assert_eq!((note.who, note.text.as_str()), (Who::Note, "Compacted: 45k → 900 tokens. Claude Code goes on from a summary of the chat so far."));

        // Text after a note is a new reply, not more of the note.
        type_str(&mut a, "and?");
        key(&mut a, "enter");
        event(&mut a, 7, AiEvent::Text("More".into()));
        assert_eq!(a.ai.turns.last().unwrap().who, Who::Assistant);
        event(&mut a, 7, AiEvent::TurnDone { error: None });

        // Unknown commands stay to be fixed; a path is just text.
        type_str(&mut a, "/c");
        key(&mut a, "esc");
        assert_eq!(a.focus, Focus::Ai, "esc closes the menu first");
        assert_eq!(a.ai_state_json()["menu"], json!([]));
        type_str(&mut a, "l");
        assert_eq!(a.ai_state_json()["menu"], json!(["/clear"]), "and it's back with the next key");
        key(&mut a, "ctrl+backspace");
        type_str(&mut a, "/nope");
        key(&mut a, "enter");
        assert_eq!(a.ai.input, "/nope");
        key(&mut a, "ctrl+backspace");
        type_str(&mut a, "/etc/hosts is odd");
        key(&mut a, "enter");
        assert_eq!(a.ai.turns.last().unwrap().text, "/etc/hosts is odd");
        event(&mut a, 7, AiEvent::TurnDone { error: None });

        // Enter on a command in the menu runs it.
        type_str(&mut a, "/cl");
        key(&mut a, "enter");
        assert!(a.ai.turns.is_empty() && matches!(a.ai.session, Session::Starting), "/clear starts a new chat");
    }

    #[test]
    fn usage_shows_until_a_new_chat_starts() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, vec![info(AiAgent::Claude, true, Some(true))]);
        let req = opening(&a).unwrap();
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 3 }) });
        assert!(a.ai_meter().is_none(), "nothing to show before a reply");
        let limit = AiLimit { window_minutes: 300, used: 0.25, resets_at: None };
        let usage = AiUsage { context_tokens: Some(50_000), context_window: Some(200_000), limits: vec![limit.clone()], ..Default::default() };
        event(&mut a, 3, AiEvent::Usage(usage));
        assert!(a.ai_meter().is_some());
        let state = a.ai_state_json();
        assert_eq!(state["usage"]["context_fill"], 0.25);
        assert_eq!(limit_label(&limit), "5h limit 25%");
        let _ = a.update(Message::Ai(Msg::NewChat));
        assert!(a.ai.usage.is_none(), "a new chat starts empty");
    }

    #[test]
    fn counts_and_limits_read_short() {
        assert_eq!([short_count(950), short_count(4525), short_count(258_400), short_count(1_000_000)], ["950", "4.5k", "258k", "1M"]);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let week = AiLimit { window_minutes: 10080, used: 0.19, resets_at: Some(now + 2 * 86400 + 3 * 3600 + 30) };
        assert_eq!(limit_label(&week), "7d limit 19% · 2d 3h left");
    }

    #[test]
    fn the_dropdown_opened_while_looking_stays_open_when_a_chat_starts() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        let _ = a.update(Message::Ai(Msg::Switch));
        let _ = a.update(Message::Ai(Msg::Tab(AiAgent::OpenCode)));
        // Both checks (opening the panel, opening the dropdown) are answered.
        while checking(&a).is_some() {
            agents(&mut a, vec![info(AiAgent::Claude, true, Some(true)), info(AiAgent::OpenCode, true, None)]);
        }
        assert!(opening(&a).is_some(), "the chat still starts");
        assert_eq!((a.ai.picking, a.ai.tab), (true, Some(AiAgent::OpenCode)));
    }

    #[test]
    fn picking_a_model_starts_a_chat_with_it_and_new_chat_keeps_it() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        let mut codex = info(AiAgent::Codex, true, Some(true));
        codex.models = vec![AiModel { id: "gpt-6-sol".into(), name: "GPT-6-Sol".into(), efforts: Vec::new(), local: false, warm: None }];
        agents(&mut a, vec![info(AiAgent::Claude, false, None), codex]);
        assert_eq!((a.ai.agent, a.ai.chosen.as_deref()), (Some(AiAgent::Codex), None), "the one it's set up with");
        assert_eq!(a.ai_model_label(), "default model");

        let _ = a.update(Message::Ai(Msg::Switch));
        assert!(a.ai.picking && a.ai.tab.is_none(), "the dropdown opens on the one in use");
        let _ = a.update(Message::Ai(Msg::Tab(AiAgent::Claude)));
        assert_eq!(a.ai.tab, Some(AiAgent::Claude));
        let _ = a.update(Message::Ai(Msg::Use(AiAgent::Codex, Some("gpt-6-sol".into()))));
        assert!(!a.ai.picking && a.ai.tab.is_none(), "picking closes it");
        assert_eq!(a.ai.chosen.as_deref(), Some("gpt-6-sol"));
        assert_eq!(a.ai_model_label(), "GPT-6-Sol");

        let _ = a.update(Message::Ai(Msg::NewChat));
        assert_eq!((a.ai.agent, a.ai.chosen.as_deref()), (Some(AiAgent::Codex), Some("gpt-6-sol")));
        assert_eq!(a.ai_state_json()["chosen_model"], "gpt-6-sol");
    }

    #[test]
    fn the_dropdown_works_from_the_keyboard() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        let m = |id: &str| AiModel { id: id.into(), name: id.into(), efforts: Vec::new(), local: false, warm: None };
        let mut claude = info(AiAgent::Claude, true, Some(true));
        claude.models = vec![m("opus"), m("sonnet")];
        let mut codex = info(AiAgent::Codex, true, Some(true));
        codex.models = vec![m("gpt-6-sol")];
        agents(&mut a, vec![claude, codex]);
        let _ = a.update(Message::Ai(Msg::Use(AiAgent::Claude, Some("opus".into()))));
        assert_eq!(a.ai.last, Some((AiAgent::Claude, Some("opus".into()))), "the pick is remembered");

        let _ = a.update(Message::Control(Command::NewAgent));
        assert!(a.ai.picking);
        let cursor = |a: &App| a.ai_state_json()["pick"]["cursor"].clone();
        assert_eq!(cursor(&a), json!({ "agent": "Claude Code", "model": "opus" }), "on the model in use");
        key(&mut a, "down");
        assert_eq!(cursor(&a)["model"], "sonnet");
        key(&mut a, "down");
        assert_eq!(cursor(&a)["model"], "sonnet", "stops at the last");
        key(&mut a, "right");
        assert_eq!(cursor(&a), json!({ "agent": "Codex", "model": null }), "another assistant: from Default");
        key(&mut a, "down");
        key(&mut a, "enter");
        assert!(!a.ai.picking && a.focus == Focus::Ai, "picked; the keys go to the message");
        assert_eq!((a.ai.agent, a.ai.chosen.as_deref()), (Some(AiAgent::Codex), Some("gpt-6-sol")));
        type_str(&mut a, "hi");
        assert_eq!(a.ai.input, "hi");

        // Off: a new agent starts with the last pick, no dropdown.
        let _ = a.update(Message::Ai(Msg::AskModel(false)));
        let _ = a.update(Message::Control(Command::NewAgent));
        assert!(!a.ai.picking);
        assert_eq!((a.ai.agent, a.ai.chosen.as_deref()), (Some(AiAgent::Codex), Some("gpt-6-sol")));
    }

    #[test]
    fn thinking_and_prompt_are_remembered_as_set_and_follow_config() {
        use iced::widget::text_editor::{Action, Edit};
        let mut a = chatting();
        let _ = a.update(Message::Ai(Msg::Effort(Effort::Level("high".into()))));
        assert_eq!(a.ai.last_effort.as_deref(), Some("high"), "remembered without starting an agent");
        for ch in "Be terse.".chars() {
            let _ = a.update(Message::Ai(Msg::Prompt(Action::Edit(Edit::Insert(ch)))));
        }
        assert_eq!(a.ai.last_prompt, "", "saved once the typing stops");
        let seq = a.ai.prompt_save_seq;
        let _ = a.update(Message::Ai(Msg::SavePrompt(seq - 1)));
        assert_eq!(a.ai.last_prompt, "", "a stale tick does nothing");
        let _ = a.update(Message::Ai(Msg::SavePrompt(seq)));
        assert_eq!(a.ai.last_prompt, "Be terse.");

        // config.toml edited: an agent that hasn't talked takes the new prompt, but keeps its
        // own thinking level; new agents start with both.
        a.ai.retune(Some("low".into()), Some("Be kind.".into()));
        assert_eq!((a.ai.effort.as_deref(), a.ai.prompt.text().trim()), (Some("high"), "Be kind."));
        assert_eq!(a.ai.last_effort.as_deref(), Some("low"));
        assert_eq!(a.ai.prompt_save_seq, seq + 1, "a pending save is dropped");
        assert_eq!(Side::Prompt.name(), "system");
    }

    #[test]
    fn each_agent_has_its_own_thinking_level_and_new_ones_start_with_the_last() {
        let mut a = chatting();
        let _ = a.update(Message::Ai(Msg::Effort(Effort::Level("high".into()))));
        type_str(&mut a, "hi");
        key(&mut a, "enter");
        let first = a.ai.cur;
        let _ = a.update(Message::Control(Command::NewAgent));
        assert_ne!(a.ai.cur, first, "a second agent");
        assert_eq!(a.ai.effort.as_deref(), Some("high"), "starts with the last level used");
        let _ = a.update(Message::Ai(Msg::Effort(Effort::Level("low".into()))));
        assert_eq!(a.ai.chats[first].effort.as_deref(), Some("high"), "the first keeps its own");
        type_str(&mut a, "hi");
        key(&mut a, "enter");
        let _ = a.update(Message::Control(Command::NewAgent));
        assert_eq!(a.ai.effort.as_deref(), Some("low"), "the next starts with the last one set");
    }

    #[test]
    fn thinking_and_prompt_set_before_the_first_message_restart_the_session() {
        let mut a = chatting();
        assert_eq!(a.ai_efforts(), AiAgent::Claude.efforts());
        let _ = a.update(Message::Ai(Msg::Effort(Effort::Level("high".into()))));
        a.ai_set_prompt("Be terse.");
        assert!(!a.ai_tuning_fixed(), "nothing said yet");
        type_str(&mut a, "hi");
        key(&mut a, "enter");
        let req = opening(&a).expect("a new session, with them");
        assert_eq!(a.ai.told_tuning, (Some("high".into()), Some("Be terse.".into())));
        assert_eq!(a.ai.queued.as_deref(), Some("hi"));
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::AiOpened { id: 8 }) });
        assert!(matches!(a.ai.session, Session::Ready(8)));
        // Under way now: a change waits for the next session.
        assert!(a.ai_tuning_fixed());
        let _ = a.update(Message::Ai(Msg::Effort(Effort::Level("low".into()))));
        assert!(opening(&a).is_none() && matches!(a.ai.session, Session::Ready(8)));
        // Another assistant doesn't get a level it hasn't got.
        a.ai.effort = Some("minimal".into());
        assert_eq!(a.ai_tuning(AiAgent::Codex).0, None);
    }

    #[test]
    fn codex_thinking_levels_follow_the_selected_model() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        let mut codex = info(AiAgent::Codex, true, Some(true));
        codex.models = vec![AiModel { id: "gpt-6-luna".into(), name: "Luna".into(), efforts: ["low", "medium", "high", "xhigh", "max"].map(str::to_string).to_vec(), local: false, warm: None }];
        agents(&mut a, vec![codex]);
        a.ai.chosen = Some("gpt-6-luna".into());
        assert!(!a.ai_efforts().contains(&"ultra".into()));
        a.ai.effort = Some("ultra".into());
        assert_eq!(a.ai_tuning(AiAgent::Codex).0, None);
    }

    #[test]
    fn the_first_agent_starts_with_the_model_picked_last_time() {
        let cfg = nib_editor::config::AiConfig { agent: Some("codex".into()), model: Some("gpt-6-sol".into()), ..Default::default() };
        let p = Panel::new(cfg);
        assert_eq!((p.agent, p.chosen.as_deref(), p.pick_model), (Some(AiAgent::Codex), Some("gpt-6-sol"), true));
    }

    #[test]
    fn opencode_lists_downloaded_models_unless_cloud_ones_are_asked_for() {
        let m = |id: &str, local| AiModel { id: id.into(), name: id.into(), efforts: Vec::new(), local, warm: None };
        let mut oc = info(AiAgent::OpenCode, true, None);
        oc.models = vec![m("opencode/big-pickle", false), m("ollama/qwen3.8:27b-mlx", true)];
        let ids = |v: Vec<&AiModel>| v.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(shown_models(&oc, false)), ["ollama/qwen3.8:27b-mlx"]);
        assert_eq!(ids(shown_models(&oc, true)).len(), 2);
        let mut claude = info(AiAgent::Claude, true, Some(true));
        claude.models = vec![m("opus", false)];
        assert_eq!(ids(shown_models(&claude, false)), ["opus"], "cloud-only assistants keep theirs");
    }

    #[test]
    fn a_local_model_switches_between_warm_and_cold() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        let mut oc = info(AiAgent::OpenCode, true, None);
        oc.models = vec![AiModel { id: "ollama/qwen3:8b".into(), name: "ollama/qwen3:8b".into(), efforts: Vec::new(), local: true, warm: Some(false) }];
        agents(&mut a, vec![oc]);
        let _ = a.update(Message::Ai(Msg::Warm("ollama/qwen3:8b".into(), true)));
        let req = a.pending.iter().find_map(|(id, p)| matches!(p, Pending::AiWarm { warm: true, .. }).then_some(*id)).expect("asked to warm it");
        assert!(a.ai.warming.contains("ollama/qwen3:8b"), "⏳ while it loads");
        let _ = a.update(Message::Ai(Msg::Warm("ollama/qwen3:8b".into(), true)));
        assert_eq!(a.pending.values().filter(|p| matches!(p, Pending::AiWarm { .. })).count(), 1, "one at a time");
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::Ok) });
        assert!(a.ai.warming.is_empty());
        assert_eq!(a.ai_state_json()["agents"][0]["warm_models"], json!(["ollama/qwen3:8b"]));
    }

    #[test]
    fn picking_a_model_gives_the_keys_back_from_the_pull_field() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        let mut oc = info(AiAgent::OpenCode, true, None);
        oc.models = vec![AiModel { id: "ollama/qwen3:8b".into(), name: "ollama/qwen3:8b".into(), efforts: Vec::new(), local: true, warm: Some(false) }];
        agents(&mut a, vec![oc]);
        let _ = a.update(Message::Ai(Msg::Switch));
        let _ = a.update(Message::Ai(Msg::PullFocus));
        let _ = a.update(Message::Ai(Msg::Use(AiAgent::OpenCode, Some("ollama/qwen3:8b".into()))));
        type_str(&mut a, "hi");
        assert_eq!((a.ai.input.as_str(), a.ai.pull_input.as_str()), ("hi", ""));
    }

    #[test]
    fn ollama_models_download_with_progress_and_delete_on_the_second_click() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, vec![info(AiAgent::OpenCode, true, None)]);
        // Typed into the download field once it's clicked, not into the message.
        a.ai.picking = true;
        let _ = a.update(Message::Ai(Msg::PullFocus));
        type_str(&mut a, "qwen3:8b");
        assert_eq!((a.ai.pull_input.as_str(), a.ai.input.as_str()), ("qwen3:8b", ""));
        key(&mut a, "Enter");
        let req = a.pending.iter().find_map(|(id, p)| matches!(p, Pending::AiPull { .. }).then_some(*id)).expect("asked to download it");
        assert!(a.ai.pull_input.is_empty() && !a.ai.pull_focus);
        let line = r#"{"status":"pulling 3a4f","total":200,"completed":50}"#;
        let _ = a.on_server(ServerMsg::Notify(nib_proto::ServerNotify::AiProgress { req, line: line.into() }));
        assert_eq!(a.ai_state_json()["pulling"], json!({ "model": "qwen3:8b", "status": "pulling 3a4f", "completed": 50, "total": 200 }));
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::Ok) });
        assert!(a.ai.pulling.is_none());
        assert!(a.pending.values().any(|p| matches!(p, Pending::AiAgents { .. })), "the list is asked for again");

        let _ = a.update(Message::Ai(Msg::Delete("qwen3:8b".into())));
        assert!(!a.pending.values().any(|p| matches!(p, Pending::AiDelete { .. })), "the first click only asks");
        assert_eq!(a.ai.confirm_delete.as_deref(), Some("qwen3:8b"));
        let _ = a.update(Message::Ai(Msg::Delete("qwen3:8b".into())));
        assert!(a.pending.values().any(|p| matches!(p, Pending::AiDelete { model } if model == "qwen3:8b")));
        assert!(a.ai.deleting.contains("qwen3:8b"));
    }

    #[test]
    fn update_runs_in_a_terminal_and_asks_again_when_closed() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, vec![info(AiAgent::Claude, true, Some(true))]);
        let _ = a.update(Message::Ai(Msg::Update(AiAgent::Claude)));
        let term = a.ai.update.map(|(k, _)| k).expect("an update terminal");
        assert_eq!(a.focus, Focus::Terminal(term));
        let req = a.pending.iter().find_map(|(id, p)| matches!(p, Pending::PtyOpen { term: t } if *t == term).then_some(*id)).unwrap();
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::PtyOpened { id: 41 }) });
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: 41, code: Some(0) }));
        assert!(a.ai.update.is_none());
        assert_eq!(checking(&a).map(|(_, t)| t), Some(None), "what's installed is asked again");
    }

    #[test]
    fn with_none_ready_it_offers_setup_and_chats_once_that_finishes() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, none_ready());
        assert!(a.ai.picking && opening(&a).is_none(), "nothing to start: the list shows");
        type_str(&mut a, "hello");
        key(&mut a, "enter");
        assert_eq!(a.ai.input, "hello", "kept until there's someone to send it to");
        agents(&mut a, none_ready());

        let _ = a.update(Message::Ai(Msg::Setup(AiAgent::Claude)));
        let term = a.ai.setup.map(|(k, _)| k).expect("a setup terminal");
        assert_eq!(a.focus, Focus::Terminal(term), "the keyboard goes to the installer");
        let req = a.pending.iter().find_map(|(id, p)| matches!(p, Pending::PtyOpen { term: t } if *t == term).then_some(*id)).unwrap();
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::PtyOpened { id: 40 }) });
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: 40, code: Some(0) }));
        assert!(a.ai.setup.is_none() && a.terms.is_empty());
        assert_eq!(checking(&a).map(|(_, t)| t), Some(Some(AiAgent::Claude)));
        let mut list = none_ready();
        list[0] = info(AiAgent::Claude, true, Some(true));
        agents(&mut a, list);
        assert!(opening(&a).is_some(), "set up: the chat starts");
        assert_eq!((a.ai.agent, a.focus, a.ai.picking), (Some(AiAgent::Claude), Focus::Ai, false));
    }

    #[test]
    fn a_setup_that_did_not_finish_says_so() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, none_ready());
        let _ = a.update(Message::Ai(Msg::Setup(AiAgent::Codex)));
        let req = a.pending.iter().find_map(|(id, p)| matches!(p, Pending::PtyOpen { .. }).then_some(*id)).unwrap();
        let _ = a.on_server(ServerMsg::Response { id: req, result: Ok(Response::PtyOpened { id: 41 }) });
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: 41, code: Some(1) }));
        agents(&mut a, none_ready());
        assert_eq!(a.ai.setup_failed, Some(AiAgent::Codex));
        assert!(a.ai.picking && opening(&a).is_none());
    }

    #[test]
    fn not_signed_in_shows_the_list_and_sending_retries() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        agents(&mut a, vec![info(AiAgent::Claude, true, None)]);
        let req = opening(&a).unwrap();
        let err = RpcError::new(nib_proto::ErrorKind::PermissionDenied, "Claude Code isn't signed in");
        let _ = a.on_server(ServerMsg::Response { id: req, result: Err(err) });
        assert!(matches!(&a.ai.session, Session::Failed(m) if m.contains("signed in")));
        assert!(a.ai.picking && checking(&a).is_some(), "it looks again, to offer signing in");
        agents(&mut a, vec![info(AiAgent::Claude, true, Some(false))]);
        assert!(opening(&a).is_none());
        type_str(&mut a, "again");
        key(&mut a, "enter");
        assert!(opening(&a).is_some(), "sending starts a new session");
        assert_eq!(a.ai.queued.as_deref(), Some("again"));
    }
}
