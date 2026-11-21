//! Git, through the daemon's `git`: the `repo::branch · user · last commit` segment of the control
//! strip, and the Git panel (toolbar button): a row of operations, how much is uncommitted, and
//! the history at the bottom. Which files changed, and staging them, is the file tree's. The panel is driven with the arrow keys, Enter and Esc.

use super::{async_sleep, App, Focus, Message, Pending};
use iced::advanced::widget::{operate, operation, Id as WidgetId};
use iced::widget::text::Span;
use iced::widget::{button, column, container, rich_text, row, scrollable, space, span, text};
use iced::{Alignment, Border, Color, Element, Font, Length, Padding, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::{Command, Document};
use nib_proto::{GitChange, GitCommit, GitInfo, GitOp, Request, Response, RpcError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often git is asked again (commits and checkouts happen outside nib).
const POLL: Duration = Duration::from_secs(5);
/// Commits listed in the panel.
const HISTORY: u32 = 200;
const HISTORY_ID: &str = "git-history";
/// Height of one history row, for scrolling by keyboard.
const ROW: f32 = 20.0;
const WIDTH: f32 = 440.0;

/// The operations in the panel, in the order ←/→ walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pull,
    Push,
    Fetch,
    Commit,
    Stash,
    StashPop,
}

const ACTIONS: [Action; 6] =
    [Action::Pull, Action::Push, Action::Fetch, Action::Commit, Action::Stash, Action::StashPop];

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Pull => "Pull",
            Action::Push => "Push",
            Action::Fetch => "Fetch",
            Action::Commit => "Commit",
            Action::Stash => "Stash",
            Action::StashPop => "Stash pop",
        }
    }

    /// As typed in a shell.
    fn command(self) -> &'static str {
        match self {
            Action::Pull => "git pull",
            Action::Push => "git push",
            Action::Fetch => "git fetch",
            Action::Commit => "git commit",
            Action::Stash => "git stash",
            Action::StashPop => "git stash pop",
        }
    }

    /// Can rewrite files in the work tree, so open tabs may be out of date afterwards.
    fn rewrites_files(self) -> bool {
        matches!(self, Action::Pull | Action::Stash | Action::StashPop)
    }
}

/// What the last operation printed.
struct Ran {
    action: Action,
    ok: bool,
    output: String,
}

#[derive(Default)]
pub struct Panel {
    pub open: bool,
    /// Index into `ACTIONS`.
    selected: usize,
    /// Where focus goes back to when the panel closes.
    return_to: Option<Focus>,
    commits: Vec<GitCommit>,
    changes: Vec<GitChange>,
    running: Option<Action>,
    last: Option<Ran>,
    /// The commit message being typed (the Commit box is open).
    message: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Toggle,
    Close,
    /// An operation's button was clicked.
    Press(usize),
    Pasted(Option<String>),
}

impl App {
    /// Ask the daemon for the project's git state; the answer schedules the next ask.
    pub(super) fn git_refresh(&mut self) -> Task<Message> {
        Task::batch([self.git_ask(true), self.tree_refresh()])
    }

    /// Show the panel with its commit message box open (the file tree's `m`). Closing the panel
    /// gives the keyboard back to where this was asked from.
    pub(super) fn git_commit_box(&mut self) -> Task<Message> {
        let from = Some(self.focus).filter(|f| !matches!(f, Focus::Git | Focus::Toolbar));
        let asked = if self.git_ui.open { Task::none() } else { self.git_open() };
        let ui = &mut self.git_ui;
        ui.return_to = from.or(ui.return_to);
        ui.selected = ACTIONS.iter().position(|a| *a == Action::Commit).unwrap_or(0);
        ui.message = Some(String::new());
        self.focus = Focus::Git;
        asked
    }

    fn git_ask(&mut self, poll: bool) -> Task<Message> {
        if self.disconnected {
            return Task::none();
        }
        let details = if self.git_ui.open {
            self.request(Request::GitDetails { log_limit: HISTORY }, Pending::GitDetails)
        } else {
            Task::none()
        };
        Task::batch([self.request(Request::GitInfo, Pending::Git { poll }), details])
    }

    pub(super) fn on_git(&mut self, poll: bool, result: Result<Response, RpcError>) -> Task<Message> {
        // A failed query leaves the last known state on screen.
        if let Ok(Response::Git(info)) = result {
            self.git = Some(info);
        }
        if !poll {
            return Task::none();
        }
        let epoch = self.epoch;
        Task::perform(async_sleep(POLL), move |_| Message::GitPoll(epoch))
    }

    pub(super) fn on_git_details(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        if let Ok(Response::GitDetails { commits, changes }) = result {
            self.git_ui.commits = commits;
            self.git_ui.changes = changes;
        }
        Task::none()
    }

    /// The text to show and whether it is a warning. `None`: not a repository (or not known yet).
    pub(super) fn git_status(&self) -> Option<(String, bool)> {
        match self.git.as_ref()? {
            GitInfo::NotRepo => None,
            GitInfo::NoGit if self.label == "local" => Some(("git not found".into(), true)),
            GitInfo::NoGit => Some((format!("git not found on {}", self.label), true)),
            GitInfo::Repo(r) => {
                let user = match (&r.user_name, &r.user_email) {
                    (Some(n), Some(e)) => format!("{n} <{e}>"),
                    (Some(n), None) => n.clone(),
                    (None, Some(e)) => format!("<{e}>"),
                    (None, None) => "git user not set".into(),
                };
                let commit = match &r.last_commit {
                    Some(c) => {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                        format!("{} {} ({})", c.hash, ellipsize(&c.subject, 50), ago(now - c.time))
                    }
                    None => "no commits yet".into(),
                };
                let branch = r.branch.as_deref().unwrap_or("(detached)");
                Some((format!("{}::{}  ·  {user}  ·  {commit}", r.name, branch), false))
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // panel: state
    // -----------------------------------------------------------------------------------------

    pub(super) fn on_git_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Toggle if self.git_ui.open => self.git_close(),
            Msg::Toggle => return self.git_open(),
            Msg::Close => self.git_close(),
            Msg::Press(i) => {
                self.git_ui.selected = i.min(ACTIONS.len() - 1);
                self.git_ui.message = None;
                self.focus = Focus::Git;
                return self.git_activate();
            }
            Msg::Pasted(s) => {
                if let (Some(s), Some(m)) = (s, &mut self.git_ui.message) {
                    // One line: the box is for the subject.
                    m.push_str(s.trim().split(['\n', '\r']).next().unwrap_or(""));
                }
            }
        }
        Task::none()
    }

    /// Show the panel (in the settings panel's place) with the keyboard on its first operation.
    fn git_open(&mut self) -> Task<Message> {
        self.ssh_close();
        self.settings.menu = false;
        self.settings.page = None;
        self.settings.recording = None;
        let ui = &mut self.git_ui;
        ui.open = true;
        ui.selected = 0;
        ui.message = None;
        ui.return_to = Some(self.focus).filter(|f| !matches!(f, Focus::Git | Focus::Toolbar));
        self.focus = Focus::Git;
        self.git_ask(false)
    }

    /// Hide the panel; the keyboard goes back to where it was.
    pub(super) fn git_close(&mut self) {
        self.git_ui.open = false;
        self.git_ui.message = None;
        if self.focus != Focus::Git {
            return;
        }
        self.focus = match self.git_ui.return_to.take() {
            Some(Focus::Terminal(k)) if self.terms_visible && self.terms.iter().any(|t| t.key == k) => Focus::Terminal(k),
            Some(Focus::Tree) if self.tree.open => Focus::Tree,
            _ => Focus::Main,
        };
    }

    /// Enter on the selected operation.
    fn git_activate(&mut self) -> Task<Message> {
        match &self.git {
            Some(GitInfo::Repo(_)) => {}
            Some(GitInfo::NoGit) => return self.flash_warn("git not found"),
            _ => return self.flash_info("not a git repository"),
        }
        if let Some(a) = self.git_ui.running {
            return self.flash_info(format!("{} is still running", a.command()));
        }
        let action = ACTIONS[self.git_ui.selected.min(ACTIONS.len() - 1)];
        let op = match action {
            Action::Commit if self.git_ui.changes.is_empty() => return self.flash_info("nothing to commit"),
            Action::Commit => {
                self.git_ui.message = Some(String::new());
                return Task::none();
            }
            Action::Pull => GitOp::Pull,
            Action::Push => GitOp::Push,
            Action::Fetch => GitOp::Fetch,
            Action::Stash => GitOp::Stash,
            Action::StashPop => GitOp::StashPop,
        };
        self.git_run(action, op)
    }

    fn git_run(&mut self, action: Action, op: GitOp) -> Task<Message> {
        self.git_ui.running = Some(action);
        self.git_ui.last = None;
        self.request(Request::GitRun { op }, Pending::GitRun { action })
    }

    pub(super) fn on_git_ran(&mut self, action: Action, result: Result<Response, RpcError>) -> Task<Message> {
        let (ok, output) = match result {
            Ok(Response::GitRan { ok, output }) => (ok, output),
            Ok(other) => (false, format!("unexpected response: {other:?}")),
            Err(e) => (false, e.to_string()),
        };
        self.git_ui.running = None;
        self.git_ui.last = Some(Ran { action, ok, output });
        let flash = if ok {
            self.flash_info(format!("{}: done", action.command()))
        } else {
            self.flash_warn(format!("{} failed — see the Git panel", action.command()))
        };
        // Even a failed pull or stash pop may have rewritten some files.
        let reload = if action.rewrites_files() { self.reload_clean_tabs() } else { Task::none() };
        Task::batch([flash, self.git_ask(false), self.tree_refresh(), reload])
    }

    /// Re-read every open file that has no unsaved edits (git may have rewritten it). Tabs with
    /// edits are left alone: saving them reports the conflict.
    fn reload_clean_tabs(&mut self) -> Task<Message> {
        let wanted: Vec<(u64, String)> = self
            .tabs
            .iter()
            .filter(|t| !t.local && t.media.is_none() && !t.doc.is_modified())
            .filter_map(|t| Some((t.id, t.doc.path.as_ref()?.to_string_lossy().into_owned())))
            .collect();
        Task::batch(wanted.into_iter().map(|(tab, path)| self.request(Request::ReadFile { path }, Pending::Reload { tab })))
    }

    pub(super) fn on_reload(&mut self, tab: u64, data: Vec<u8>, mtime_ns: u128) {
        let Some(i) = self.tabs.iter().position(|t| t.id == tab) else { return };
        let t = &self.tabs[i];
        if t.doc.is_modified() || t.doc.disk_mtime_ns == Some(mtime_ns) {
            return;
        }
        let mut doc = Document::from_bytes(&data, t.doc.path.clone());
        doc.tab_width = self.tab_width;
        doc.disk_mtime_ns = Some(mtime_ns);
        let last_line = doc.text.len_lines().saturating_sub(1);
        let line = t.doc.cursor_line_col().0.min(last_line);
        doc.set_cursor(doc.text.line_to_char(line), false);
        // The language server goes back to the file on disk until the next edit.
        self.lsp_close(i);
        let t = &mut self.tabs[i];
        t.doc = doc;
        t.top = t.top.min(last_line);
        t.table = None;
        t.preview = None;
        t.svg = None;
        t.lsp_synced = None;
    }

    // -----------------------------------------------------------------------------------------
    // panel: keys
    // -----------------------------------------------------------------------------------------

    pub(super) fn git_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        if let Some(m) = &mut self.git_ui.message {
            match chord.key {
                Key::Esc => self.git_ui.message = None,
                Key::Enter => {
                    let message = m.trim().to_string();
                    if message.is_empty() {
                        return self.flash_info("type a commit message first");
                    }
                    self.git_ui.message = None;
                    return self.git_run(Action::Commit, GitOp::Commit { message });
                }
                Key::Backspace if held || chord.alt => m.clear(),
                Key::Backspace => {
                    m.pop();
                }
                _ if self.keymap.lookup(&chord) == Some(Command::Paste) => {
                    return iced::clipboard::read().map(|s| Message::Git(Msg::Pasted(s)));
                }
                _ if !held => m.extend(text.unwrap_or_default().chars().filter(|c| !c.is_control())),
                _ => {}
            }
            return Task::none();
        }
        let plain = !(held || chord.alt);
        let scroll = |rows: f32| {
            let offset = operation::scrollable::AbsoluteOffset { x: 0.0, y: rows * ROW };
            operate(operation::scrollable::scroll_by(WidgetId::from(HISTORY_ID), offset))
        };
        match chord.key {
            Key::Left if plain => self.git_ui.selected = self.git_ui.selected.saturating_sub(1),
            Key::Right if plain => self.git_ui.selected = (self.git_ui.selected + 1).min(ACTIONS.len() - 1),
            Key::Home if plain => self.git_ui.selected = 0,
            Key::End if plain => self.git_ui.selected = ACTIONS.len() - 1,
            Key::Up if plain => return scroll(-3.0),
            Key::Down if plain => return scroll(3.0),
            Key::PageUp if plain => return scroll(-12.0),
            Key::PageDown if plain => return scroll(12.0),
            Key::Enter | Key::Char(' ') if plain => return self.git_activate(),
            Key::Esc => self.git_close(),
            _ => {
                if let Some(c) = self.keymap.lookup(&chord).filter(|c| super::app_wide(*c)) {
                    return self.exec(c);
                }
            }
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // panel: view
    // -----------------------------------------------------------------------------------------

    pub(super) fn git_label<'a>(&self, s: impl Into<String>) -> Element<'a, Message> {
        container(text(s.into().to_uppercase()).size(10).color(self.theme.dim))
            .padding(Padding { top: 12.0, bottom: 4.0, left: 0.0, right: 0.0 })
            .into()
    }

    /// What Commit will take, as the daemon decides it: the staged files, or everything.
    fn commit_scope(&self) -> String {
        let (all, staged) = (self.git_ui.changes.len(), self.git_ui.changes.iter().filter(|c| c.staged).count());
        let files = |n: usize| if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        match staged {
            0 => format!("nothing is staged: commits all changes ({})", files(all)),
            n => format!("commits what is staged ({})", files(n)),
        }
    }

    pub(super) fn git_panel(&self) -> Option<Element<'_, Message>> {
        let ui = &self.git_ui;
        if !ui.open {
            return None;
        }
        let th = &self.theme;
        let (accent, dim, fg, panel) = (th.accent, th.dim, th.foreground, th.panel);
        let focused = self.focus == Focus::Git;

        let title = match &self.git {
            Some(GitInfo::Repo(r)) => format!("{}::{}", r.name, r.branch.as_deref().unwrap_or("(detached)")),
            _ => String::new(),
        };
        let close = button(text("✕").size(12).color(dim))
            .on_press(Message::Git(Msg::Close))
            .padding([1, 6])
            .style(|_, _| button::Style::default());
        let header = row![text("Git").size(12).color(fg), text(title).size(12).color(dim), space::horizontal(), close]
            .spacing(10)
            .align_y(Alignment::Center)
            .padding(Padding { left: 14.0, right: 8.0, top: 8.0, bottom: 6.0 });

        let body: Element<'_, Message> = match &self.git {
            Some(GitInfo::Repo(_)) => self.git_repo_body(),
            other => {
                let (msg, color) = match other {
                    Some(GitInfo::NoGit) if self.label == "local" => ("git not found on this machine.".to_string(), th.warn),
                    Some(GitInfo::NoGit) => (format!("git not found on {}.", self.label), th.warn),
                    Some(_) => ("This folder is not a git repository.".to_string(), dim),
                    None => ("Asking git…".to_string(), dim),
                };
                column![text(msg).size(12).color(color), self.hint("esc", "close")]
                    .spacing(12)
                    .padding([10, 14])
                    .height(Length::Fill)
                    .into()
            }
        };
        let halo = if focused { accent } else { Color::TRANSPARENT };
        Some(
            container(column![header, self.hrule(), body])
                .padding(2)
                .width(WIDTH)
                .height(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(panel.into()),
                    border: Border { color: halo, width: 2.0, radius: 3.0.into() },
                    ..Default::default()
                })
                .into(),
        )
    }

    fn git_repo_body(&self) -> Element<'_, Message> {
        let ui = &self.git_ui;
        let th = &self.theme;
        let (accent, dim, fg, sel_bg, divider) = (th.accent, th.dim, th.foreground, th.selection, th.divider);
        let focused = self.focus == Focus::Git;
        let mono = Font::MONOSPACE;

        let actions = row(ACTIONS.iter().enumerate().map(|(i, a)| {
            let selected = i == ui.selected;
            let running = ui.running == Some(*a);
            let label = if running { format!("{}…", a.label()) } else { a.label().to_string() };
            let border = match (selected, focused) {
                (true, true) => accent,
                (true, false) => dim,
                _ => divider,
            };
            button(text(label).size(12).color(if running { dim } else { fg }))
                .on_press(Message::Git(Msg::Press(i)))
                .padding([4, 10])
                .style(move |_, st| button::Style {
                    background: (selected && focused || matches!(st, button::Status::Hovered)).then(|| sel_bg.into()),
                    border: Border { color: border, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .into()
        }))
        .spacing(6);

        let mut top = column![actions].spacing(8);
        if let Some(m) = &ui.message {
            // The caret is a glyph inside the text so it follows the last character when it wraps.
            let typed: [Span<'_, (), Font>; 2] = [span(m.as_str()).color(fg), span("▏").color(accent)];
            let field = container(rich_text(typed).size(12).width(Length::Fill))
                .padding([5, 8])
                .width(Length::Fill)
                .clip(true)
                .style(move |_| container::Style {
                    border: Border { color: accent, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                });
            top = top
                .push(text(format!("Commit message — {}", self.commit_scope())).size(11).color(dim))
                .push(field)
                .push(row![self.hint("↵", "commit"), self.hint("esc", "cancel")].spacing(18));
        } else {
            top = top.push(
                row![self.hint("←→", "choose"), self.hint("↵", "run"), self.hint("↑↓", "history"), self.hint("esc", "close")]
                    .spacing(14),
            );
        }
        if let Some(a) = ui.running {
            top = top.push(text(format!("{} — running…", a.command())).size(12).color(dim));
        }
        if let Some(r) = &ui.last {
            let (head, color) = if r.ok { (format!("{} — done", r.action.command()), fg) } else { (format!("{} — failed", r.action.command()), th.warn) };
            top = top.push(text(head).size(12).color(color));
            if !r.output.is_empty() {
                let out = text(r.output.as_str()).size(11).font(mono).color(dim);
                top = top.push(container(scrollable(container(out).width(Length::Fill).padding(Padding { right: 10.0, ..Padding::ZERO })).style(crate::theme::scrollbar)).max_height(150));
            }
        }

        // The files themselves are in the file tree (its changes and staged lenses); here, the count.
        let staged = ui.changes.iter().filter(|c| c.staged).count();
        let summary = match ui.changes.len() {
            0 => "Nothing to commit, working tree clean.".to_string(),
            n => format!("{n} changed · {staged} staged — pick files in the file tree (c, s; space stages)"),
        };
        let changes = column![self.git_label("Changes"), text(summary).size(11).color(dim)];

        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let history: Element<'_, Message> = if ui.commits.is_empty() {
            container(text("No commits yet.").size(11).color(dim)).height(Length::Fill).into()
        } else {
            let rows = ui.commits.iter().map(|c| {
                let subject = container(text(c.subject.as_str()).size(12).color(fg).wrapping(text::Wrapping::None))
                    .width(Length::Fill)
                    .clip(true);
                row![
                    text(c.hash.as_str()).size(11).font(mono).color(accent),
                    subject,
                    text(format!("{} · {}", ellipsize(&c.author, 16), ago(now - c.time))).size(11).color(dim),
                ]
                .spacing(10)
                .height(ROW)
                .align_y(Alignment::Center)
                .padding(Padding { right: 12.0, ..Padding::ZERO })
                .into()
            });
            scrollable(column(rows).width(Length::Fill)).style(crate::theme::scrollbar).id(HISTORY_ID).height(Length::Fill).into()
        };

        column![top, changes, self.git_label("History"), history]
            .padding(Padding { left: 14.0, right: 6.0, top: 10.0, bottom: 8.0 })
            .height(Length::Fill)
            .into()
    }
}

/// "3h ago" for an age in seconds.
fn ago(secs: i64) -> String {
    let (n, unit) = match secs {
        s if s < 60 => return "just now".into(),
        s if s < 3600 => (s / 60, "m"),
        s if s < 86_400 => (s / 3600, "h"),
        s if s < 14 * 86_400 => (s / 86_400, "d"),
        s if s < 60 * 86_400 => (s / (7 * 86_400), "w"),
        s if s < 365 * 86_400 => (s / (30 * 86_400), "mo"),
        s => (s / (365 * 86_400), "y"),
    };
    format!("{n}{unit} ago")
}

/// `s` cut to at most `max` characters, the last one an ellipsis.
pub(super) fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib_proto::GitRepo;

    #[test]
    fn ages_and_ellipsis() {
        assert_eq!(ago(-5), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(3 * 3600 + 5), "3h ago");
        assert_eq!(ago(13 * 86_400), "13d ago");
        assert_eq!(ago(21 * 86_400), "3w ago");
        assert_eq!(ago(90 * 86_400), "3mo ago");
        assert_eq!(ago(800 * 86_400), "2y ago");
        assert_eq!(ellipsize("héllo", 5), "héllo");
        assert_eq!(ellipsize("héllo!", 5), "héll…");
    }

    #[test]
    fn segment_per_state() {
        let mut a = super::super::tests::app();
        assert_eq!(a.git_status(), None);
        a.git = Some(GitInfo::NotRepo);
        assert_eq!(a.git_status(), None);
        a.git = Some(GitInfo::NoGit);
        assert_eq!(a.git_status(), Some(("git not found".into(), true)));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let mut repo = GitRepo {
            name: "nibble".into(),
            branch: Some("main".into()),
            user_name: Some("Ada".into()),
            user_email: Some("ada@example.com".into()),
            last_commit: Some(GitCommit { hash: "76d84d1".into(), subject: "App icon".into(), author: "Ada".into(), time: now - 7300 }),
        };
        a.git = Some(GitInfo::Repo(repo.clone()));
        assert_eq!(a.git_status().unwrap(), ("nibble::main  ·  Ada <ada@example.com>  ·  76d84d1 App icon (2h ago)".into(), false));
        repo.branch = None;
        repo.user_name = None;
        repo.user_email = None;
        repo.last_commit = None;
        a.git = Some(GitInfo::Repo(repo));
        assert_eq!(a.git_status().unwrap().0, "nibble::(detached)  ·  git user not set  ·  no commits yet");
    }

    fn repo() -> GitInfo {
        GitInfo::Repo(GitRepo { name: "nibble".into(), branch: Some("main".into()), user_name: None, user_email: None, last_commit: None })
    }
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
    /// The outstanding GitRun request, if any.
    fn running(a: &App) -> Option<(nib_proto::ReqId, Action)> {
        a.pending.iter().find_map(|(id, p)| match p {
            Pending::GitRun { action } => Some((*id, *action)),
            _ => None,
        })
    }
    fn answer(a: &mut App, ok: bool, output: &str) {
        let (id, _) = running(a).expect("an operation is running");
        let result = Ok(Response::GitRan { ok, output: output.into() });
        let _ = a.update(Message::Server(nib_proto::ServerMsg::Response { id, result }));
    }
    /// To the toolbar (the command Ctrl+B runs on macOS), over to the Git button, Enter.
    fn open_from_toolbar(a: &mut App) {
        let _ = a.update(Message::Control(Command::FocusToolbar));
        assert_eq!(a.focus, Focus::Toolbar);
        a.toolbar_sel = 0; // the toolbar remembers the last button
        let git = a.toolbar_items().iter().position(|(_, tip, _)| tip.starts_with("Git")).unwrap();
        for _ in 0..git {
            key(a, "right");
        }
        key(a, "enter");
    }

    #[test]
    fn toolbar_to_pull_and_out_without_the_mouse() {
        let mut a = super::super::tests::app();
        a.git = Some(repo());
        open_from_toolbar(&mut a);
        assert!(a.git_ui.open && a.git_panel().is_some());
        assert_eq!(a.focus, Focus::Git, "the panel has the keyboard as soon as it opens");
        assert_eq!(ACTIONS[a.git_ui.selected], Action::Pull);
        assert!(a.pending.values().any(|p| matches!(p, Pending::GitDetails)), "history asked for");

        key(&mut a, "left"); // clamps
        key(&mut a, "right");
        assert_eq!(ACTIONS[a.git_ui.selected], Action::Push);
        key(&mut a, "left");
        key(&mut a, "enter");
        assert_eq!(running(&a).map(|r| r.1), Some(Action::Pull));
        key(&mut a, "enter"); // still running: not sent twice
        assert_eq!(a.pending.values().filter(|p| matches!(p, Pending::GitRun { .. })).count(), 1);
        answer(&mut a, true, "Already up to date.");
        assert!(a.git_ui.running.is_none());
        assert!(a.git_ui.last.as_ref().is_some_and(|r| r.ok && r.output == "Already up to date."));
        assert!(a.pending.values().any(|p| matches!(p, Pending::Git { poll: false })), "strip refreshed");

        key(&mut a, "esc");
        assert!(!a.git_ui.open && a.git_panel().is_none());
        assert_eq!(a.focus, Focus::Main);

        // The toolbar command also works from inside the panel, and toggles back out.
        open_from_toolbar(&mut a);
        assert_eq!(a.focus, Focus::Git);
        let _ = a.update(Message::Control(Command::FocusToolbar));
        assert_eq!(a.focus, Focus::Toolbar);
        let _ = a.update(Message::Control(Command::FocusToolbar));
        assert_eq!(a.focus, Focus::Git, "and back to the panel it came from");
        if cfg!(target_os = "macos") {
            key(&mut a, "ctrl+b");
            assert_eq!(a.focus, Focus::Toolbar);
            key(&mut a, "ctrl+b");
            assert_eq!(a.focus, Focus::Git);
        }
    }

    #[test]
    fn commit_asks_for_a_message() {
        let mut a = super::super::tests::app();
        a.git = Some(repo());
        let _ = a.update(Message::Git(Msg::Toggle));
        key(&mut a, "end");
        key(&mut a, "home");
        for _ in 0..3 {
            key(&mut a, "right");
        }
        key(&mut a, "enter");
        assert!(a.git_ui.message.is_none(), "nothing to commit: no box");
        a.git_ui.changes = vec![
            GitChange { path: "a.py".into(), code: " M".into(), staged: false },
            GitChange { path: "b.py".into(), code: "??".into(), staged: false },
        ];
        assert_eq!(a.commit_scope(), "nothing is staged: commits all changes (2 files)");
        key(&mut a, "enter");
        assert_eq!(a.git_ui.message.as_deref(), Some(""));
        key(&mut a, "enter"); // empty message: nothing sent
        assert!(running(&a).is_none());
        type_str(&mut a, "Fix q the parserr");
        key(&mut a, "backspace");
        key(&mut a, "left"); // arrows don't leave the box
        assert_eq!(ACTIONS[a.git_ui.selected], Action::Commit);
        let _ = a.update(Message::Git(Msg::Pasted(Some(" (#12)\nbody".into()))));
        assert!(a.tabs.is_empty() && a.git_panel().is_some());
        key(&mut a, "enter");
        let sent = a.pending.values().any(|p| matches!(p, Pending::GitRun { action: Action::Commit }));
        assert!(sent && a.git_ui.message.is_none());
        answer(&mut a, false, "nothing added to commit");
        assert!(a.git_ui.last.as_ref().is_some_and(|r| !r.ok));
        assert!(a.git_panel().is_some());

        // Esc first leaves the box, then the panel.
        key(&mut a, "enter");
        key(&mut a, "esc");
        assert!(a.git_ui.open && a.git_ui.message.is_none());
        key(&mut a, "esc");
        assert!(!a.git_ui.open);

        a.git_ui.changes[0].staged = true;
        assert_eq!(a.commit_scope(), "commits what is staged (1 file)");
    }

    #[test]
    fn not_a_repository_runs_nothing_and_shares_the_spot_with_settings() {
        let mut a = super::super::tests::app();
        a.git = Some(GitInfo::NotRepo);
        let _ = a.update(Message::Git(Msg::Toggle));
        assert!(a.git_panel().is_some());
        key(&mut a, "enter");
        assert!(running(&a).is_none());
        // Settings takes the spot; Git takes it back. The close-tab key closes the panel too.
        let _ = a.open_settings(super::super::settings::Page::Shortcuts);
        assert!(!a.git_ui.open && a.settings.page.is_some());
        assert_eq!(a.focus, Focus::Main);
        let _ = a.update(Message::Control(Command::ToggleGit));
        assert!(a.git_ui.open && a.settings.page.is_none());
        let _ = a.update(Message::Control(Command::CloseTab));
        assert!(!a.git_ui.open);
        assert_eq!(a.focus, Focus::Main);
    }

    #[test]
    fn pull_reloads_clean_tabs_only() {
        let mut a = super::super::tests::app();
        a.git = Some(repo());
        for name in ["clean.py", "dirty.py"] {
            let mut doc = Document::from_bytes(b"one\ntwo\nthree\n", Some(format!("/tmp/{name}").into()));
            doc.disk_mtime_ns = Some(1);
            a.put_doc(doc, true);
        }
        type_str(&mut a, "x"); // dirty.py now has unsaved edits
        let _ = a.update(Message::Git(Msg::Toggle));
        key(&mut a, "enter");
        answer(&mut a, true, "Updating 1..2");
        let reloads: Vec<(nib_proto::ReqId, u64)> = a
            .pending
            .iter()
            .filter_map(|(id, p)| match p {
                Pending::Reload { tab } => Some((*id, *tab)),
                _ => None,
            })
            .collect();
        assert_eq!(reloads.len(), 1);
        let (id, tab) = reloads[0];
        assert_eq!(tab, a.tabs[0].id);
        let file = Response::File { path: "/tmp/clean.py".into(), data: b"uno\n".to_vec(), mtime_ns: 2 };
        let _ = a.update(Message::Server(nib_proto::ServerMsg::Response { id, result: Ok(file) }));
        assert_eq!(a.tabs[0].doc.text.to_string(), "uno\n");
        assert!(!a.tabs[0].doc.is_modified());
        assert!(a.tabs[1].doc.text.to_string().contains("one") && a.tabs[1].doc.is_modified());
    }
}
