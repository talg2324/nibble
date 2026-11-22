//! The SSH panel (toolbar button, `toggle_ssh`) and the `ssh.*` control requests.
//!
//! One box and one list. Type a host (`user@host`, a `~/.ssh/config` alias, `host:port`) and
//! press Enter: the window connects there, in place, to the home folder. The file tree's
//! "open as root" (`o`) picks the project folder from there. A host that worked is saved (a
//! `[profiles]` entry) with every root opened on it, and the list offers them next time: Enter
//! on a row connects to that host and folder. Typing filters the list. Port, key file and jump
//! host belong in ~/.ssh/config (or config.toml); the panel doesn't ask for them.
//!
//! When a connect fails on an unknown host key, the panel fetches the key's fingerprint and
//! offers "Trust and connect" — the only way to trust a key from nib, and not on the API.

use super::connection::target_text;
use super::{App, Focus, Message, Pending};
use crate::control::Call;
use crate::ssh::{Probe, Stage};
use crate::Launch;
use iced::widget::{button, column, container, mouse_area, rich_text, row, scrollable, space, span, text};
use iced::{Alignment, Border, Color, Element, Length, Padding, Task};
use nib_editor::config::check_profile_name;
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::{Command, Config, Profile, Target};
use nib_proto::{EntryKind, Request, Response, RpcError};
use serde_json::{json, Value};
use std::sync::Arc;

const WIDTH: f32 = 440.0;
/// The command key, as a hint shows it before another key.
const MOD_PLUS: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };
const PLACEHOLDER: &str = "user@host or IP, then ↵ (asks for a password if it needs one)";

/// What the keyboard walks, in the order the panel shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Input,
    /// Stop connecting.
    Cancel,
    /// Trust the host key that stopped the last connect, and connect again.
    Trust,
    /// A saved connection (index into `profiles`) at one of its roots (index into `all_roots`).
    Root(usize, usize),
    /// Where the window started, when that wasn't a saved connection.
    Home,
    /// A ~/.ssh/config host with no saved connection yet.
    Alias(usize),
}

enum AskKind {
    /// A password or code: typed, never shown.
    Secret,
    /// Yes or no.
    Confirm,
    /// Something to read (touch your security key); nothing to answer.
    Notice,
}

/// A question from ssh, through askpass.
struct Ask {
    prompt: String,
    kind: AskKind,
    input: String,
    call: Call,
}

/// The host key of a connect that failed on it.
enum KeyCheck {
    Fetching(u64, Launch),
    Ready(Launch, Arc<Probe>),
}

#[derive(Default)]
pub struct Panel {
    pub open: bool,
    return_to: Option<Focus>,
    /// The box: a host to connect to, and the list's filter.
    input: String,
    sel: usize,
    profiles: Vec<(String, Profile)>,
    aliases: Vec<String>,
    /// config.toml couldn't be read.
    load_error: Option<String>,
    /// What's wrong with what was typed.
    error: Option<String>,
    ask: Option<Ask>,
    /// Pressed once on a row whose switch would close things: press again to go.
    armed: Option<Row>,
    /// Asked "forget …? y / n".
    forgetting: Option<Row>,
    key: Option<KeyCheck>,
    key_seq: u64,
    /// Picking a folder on the connected machine to open as the project.
    browse: Option<Browse>,
    /// Open the folder browser once the connection being made has said hello.
    pub(super) browse_next: bool,
    /// Changing a saved connection.
    edit: Option<Edit>,
}

/// The folder browser: one folder's subfolders, filtered by what is typed.
#[derive(Default)]
struct Browse {
    /// Absolute, on the connected machine.
    dir: String,
    /// Subfolders (and links, which may be folders).
    entries: Vec<(String, bool)>,
    filter: String,
    /// 0 = this folder, then the shown entries.
    sel: usize,
    loading: bool,
    error: Option<String>,
}

impl Browse {
    /// The entries the filter lets through. Hidden folders only when asked for with a dot.
    fn shown(&self) -> Vec<&(String, bool)> {
        let q = self.filter.to_lowercase();
        self.entries.iter().filter(|(n, _)| (!n.starts_with('.') || q.starts_with('.')) && n.to_lowercase().contains(&q)).collect()
    }

    fn path_of(&self, name: &str) -> String {
        format!("{}/{name}", self.dir.trim_end_matches('/'))
    }

    fn parent(&self) -> Option<String> {
        match self.dir.trim_end_matches('/').rsplit_once('/') {
            Some(("", _)) if self.dir.len() > 1 => Some("/".into()),
            Some((p, _)) if !p.is_empty() => Some(p.to_owned()),
            _ => None,
        }
    }
}

/// The fields of a connection, as the edit form shows them.
const EDIT_FIELDS: [(&str, &str); 7] = [
    ("name", "how the list calls it (@name)"),
    ("host", "user@host, an IP, or a ~/.ssh/config alias"),
    ("user", "from ~/.ssh/config"),
    ("port", "22"),
    ("key file", "~/.ssh/id_ed25519"),
    ("jump host", "bastion  (ssh -J)"),
    ("nibd", "a nibd on the host (default: upload one)"),
];

/// The fields the form shows (name, host, user). The rest (port, key file, jump host, nibd)
/// are for ~/.ssh/config or config.toml: they're kept as they are, not shown.
const EDIT_SHOWN: usize = 3;

struct Edit {
    /// The saved connection being changed (None: saving a ~/.ssh/config host as one).
    orig: Option<String>,
    fields: [String; 7],
    sel: usize,
    /// The changes, shown before writing config.toml (y saves).
    confirm: Option<Vec<String>>,
    error: Option<String>,
}

impl Edit {
    fn new(orig: Option<String>, fields: [String; 7]) -> Edit {
        Edit { orig, fields, sel: 1, confirm: None, error: None }
    }

    /// How many fields the form shows.
    fn shown(&self) -> usize {
        EDIT_SHOWN
    }
}

#[derive(Debug, Clone)]
pub enum Msg {
    Toggle,
    Close,
    /// The panel was clicked: give it the keyboard.
    Focus,
    /// A row was clicked (index into the rows): select it, and press it unless it's the box.
    Click(usize),
    KeyFetched(u64, Arc<Probe>),
    Pasted(Option<String>),
    /// The askpass box's buttons: answer (true) or cancel.
    Answer(bool),
    /// A folder in the browser was clicked (0 = this folder): go in.
    BrowseInto(usize),
    /// Open the selected folder as the project.
    BrowseOpen,
    BrowseUp,
    EditField(usize),
    EditSave,
    EditCancel,
}

/// Run `f` on a thread; the future resolves with its result.
fn background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> impl std::future::Future<Output = Option<T>> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    async move { rx.await.ok() }
}

/// Where a saved connection takes the window, at `root`.
fn launch_for(name: &str, p: &Profile, root: &str) -> anyhow::Result<Launch> {
    Ok(Launch { target: p.target()?, root: root.to_owned(), open: None, label: format!("@{name}"), profile: Some(name.to_owned()) })
}

/// The ssh side of a target, for a test.
fn test_args(l: &Launch) -> Option<(nib_editor::SshHost, String, Option<String>)> {
    match &l.target {
        Target::Ssh { ssh, nibd } => Some((ssh.clone(), l.root.clone(), nibd.clone())),
        _ => None,
    }
}

fn load_profiles() -> Result<Vec<(String, Profile)>, String> {
    Config::load().map(|c| c.profiles.into_iter().collect()).map_err(|e| format!("{e:#}"))
}

/// A profile name from a host: `tal@gpu.lab` -> `gpu.lab`.
fn name_for(host: &str) -> String {
    let h = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let n: String = h.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let n = n.trim_start_matches(['-', '.']).to_owned();
    if n.is_empty() { "host".into() } else { n.chars().take(64).collect() }
}

/// A path cut from the left to `max` characters: the end is what tells folders apart.
fn short_path(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        return p.to_owned();
    }
    let tail: String = p.chars().skip(n - (max - 1)).collect();
    // Start at a folder boundary when there is one.
    match tail.find('/') {
        Some(i) if i < tail.len() / 2 => format!("…{}", &tail[i..]),
        _ => format!("…{tail}"),
    }
}

/// `user@host`, `host`, `host:2222` as a profile (root: home).
fn parse_host(s: &str) -> Result<Profile, String> {
    let s = s.trim();
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h, Some(p.parse::<u16>().map_err(|_| format!("port {p} is not a port"))?))
        }
        _ => (s, None),
    };
    let p = Profile { ssh: Some(host.to_owned()), port, root: "~".into(), ..Default::default() };
    p.check().map_err(|e| format!("{e:#}"))?;
    Ok(p)
}

impl App {
    pub(super) fn on_ssh_msg(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::Toggle if self.ssh.open => self.ssh_close(),
            Msg::Toggle => self.ssh_open(),
            Msg::Close => self.ssh_close(),
            Msg::Focus => self.ssh_focus(),
            Msg::Click(i) => {
                self.ssh_focus();
                self.ssh.sel = i;
                if self.ssh_rows().get(i) != Some(&Row::Input) {
                    return self.ssh_activate();
                }
            }
            Msg::KeyFetched(seq, p) => {
                if let Some(KeyCheck::Fetching(s, l)) = &self.ssh.key {
                    if *s == seq {
                        let l = l.clone();
                        self.ssh.key = Some(KeyCheck::Ready(l, p));
                        // Put the keyboard on "Trust and connect".
                        if let Some(i) = self.ssh_rows().iter().position(|r| *r == Row::Trust) {
                            self.ssh.sel = i;
                        }
                    }
                }
            }
            Msg::Pasted(s) => {
                let Some(s) = s else { return Task::none() };
                let line = s.trim().split(['\n', '\r']).next().unwrap_or("").to_owned();
                if let Some(a) = &mut self.ssh.ask {
                    a.input.push_str(&line);
                } else if let Some(e) = &mut self.ssh.edit {
                    e.fields[e.sel].push_str(&line);
                } else if let Some(b) = &mut self.ssh.browse {
                    b.filter.push_str(&line);
                } else {
                    self.ssh.input.push_str(&line);
                    self.ssh.sel = 0;
                }
            }
            Msg::Answer(yes) => return self.ssh_answer(yes),
            Msg::BrowseInto(i) => {
                self.focus = Focus::Tree;
                if let Some(b) = &mut self.ssh.browse {
                    b.sel = i;
                }
                return self.ssh_browse_into();
            }
            Msg::BrowseOpen => return self.ssh_browse_open(),
            Msg::BrowseUp => return self.ssh_browse_up(),
            Msg::EditField(i) => {
                self.ssh_focus();
                if let Some(e) = &mut self.ssh.edit {
                    e.sel = i.min(e.shown() - 1);
                    e.confirm = None;
                }
            }
            Msg::EditSave => return self.ssh_edit_confirm(),
            Msg::EditCancel => self.ssh.edit = None,
        }
        Task::none()
    }

    /// Show the panel (in the Git / Settings spot) with the keyboard in the box.
    fn ssh_open(&mut self) {
        self.git_close();
        self.apps_close();
        self.settings.menu = false;
        self.settings.page = None;
        self.settings.recording = None;
        self.ssh.open = true;
        self.ssh_reload();
        self.ssh.aliases = crate::ssh::config_hosts();
        self.ssh.sel = 0;
        self.ssh_focus();
    }

    fn ssh_focus(&mut self) {
        if self.focus != Focus::Ssh {
            self.ssh.return_to = Some(self.focus).filter(|f| !matches!(f, Focus::Ssh | Focus::Toolbar));
            self.focus = Focus::Ssh;
        }
    }

    /// Hide the panel; the keyboard goes back to where it was.
    pub(super) fn ssh_close(&mut self) {
        self.ssh.open = false;
        self.ssh.armed = None;
        self.ssh.forgetting = None;
        self.ssh.edit = None;
        if self.focus != Focus::Ssh {
            return;
        }
        self.focus = match self.ssh.return_to.take() {
            Some(Focus::Terminal(k)) if self.terms_visible && self.terms.iter().any(|t| t.key == k) => Focus::Terminal(k),
            Some(Focus::Tree) if self.tree.open => Focus::Tree,
            Some(Focus::Ai) if self.ai.open => Focus::Ai,
            _ => Focus::Main,
        };
    }

    pub(super) fn ssh_reload(&mut self) {
        match load_profiles() {
            Ok(p) => {
                self.ssh.profiles = p;
                self.ssh.load_error = None;
            }
            Err(e) => self.ssh.load_error = Some(e),
        }
        self.ssh.sel = self.ssh.sel.min(self.ssh_rows().len().saturating_sub(1));
    }

    /// The box's text matches one of these (case-insensitive)? An empty box matches everything.
    fn ssh_matches(&self, words: &[&str]) -> bool {
        let q = self.ssh.input.trim().to_lowercase();
        q.is_empty() || words.iter().any(|w| w.to_lowercase().contains(&q))
    }

    fn ssh_rows(&self) -> Vec<Row> {
        let ui = &self.ssh;
        let mut r = vec![Row::Input];
        if self.connecting.is_some() {
            r.push(Row::Cancel);
        }
        if matches!(&ui.key, Some(KeyCheck::Ready(_, p)) if p.stage == Stage::HostKeyUnknown) {
            r.push(Row::Trust);
        }
        for (i, (name, p)) in ui.profiles.iter().enumerate() {
            let roots = p.all_roots();
            let mut words: Vec<&str> = vec![name, p.ssh.as_deref().or(p.wsl.as_deref()).unwrap_or("")];
            words.extend(roots.iter().map(String::as_str));
            if self.ssh_matches(&words) {
                r.extend((0..roots.len()).map(|j| Row::Root(i, j)));
            }
        }
        if self.home.profile.is_none() && self.ssh_matches(&[&self.home.label, &self.home.root]) {
            r.push(Row::Home);
        }
        r.extend(self.ssh_new_aliases().into_iter().filter(|(_, a)| self.ssh_matches(&[a])).map(|(i, _)| Row::Alias(i)));
        r
    }

    /// ~/.ssh/config hosts no saved connection uses yet, with their index in `aliases`.
    fn ssh_new_aliases(&self) -> Vec<(usize, &str)> {
        let ui = &self.ssh;
        ui.aliases
            .iter()
            .enumerate()
            .filter(|(_, a)| !ui.profiles.iter().any(|(n, p)| p.ssh.as_deref() == Some(a.as_str()) || n == *a))
            .map(|(i, a)| (i, a.as_str()))
            .collect()
    }

    // -----------------------------------------------------------------------------------------
    // keys
    // -----------------------------------------------------------------------------------------

    pub(super) fn ssh_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let plain = !(held || chord.alt);
        let paste = self.keymap.lookup(&chord) == Some(Command::Paste);
        if let Some(a) = &mut self.ssh.ask {
            match chord.key {
                Key::Enter => return self.ssh_answer(true),
                Key::Esc => return self.ssh_answer(false),
                Key::Char('y') if matches!(a.kind, AskKind::Confirm) && plain => return self.ssh_answer(true),
                Key::Char('n') if matches!(a.kind, AskKind::Confirm) && plain => return self.ssh_answer(false),
                Key::Backspace if held || chord.alt => a.input.clear(),
                Key::Backspace => {
                    a.input.pop();
                }
                _ if paste => return iced::clipboard::read().map(|s| Message::Ssh(Msg::Pasted(s))),
                _ if !held && matches!(a.kind, AskKind::Secret) => a.input.extend(text.unwrap_or_default().chars().filter(|c| !c.is_control())),
                _ => {}
            }
            return Task::none();
        }
        if self.ssh.edit.is_some() {
            return self.ssh_edit_key(chord, text);
        }
        if let Some(r) = self.ssh.forgetting.take() {
            if chord.key == Key::Char('y') && plain {
                return self.ssh_forget(r);
            }
            return self.flash_info("kept it");
        }
        let rows = self.ssh_rows();
        let n = rows.len();
        let cur = rows.get(self.ssh.sel.min(n - 1)).copied().unwrap_or(Row::Input);
        if chord.key != Key::Enter {
            self.ssh.armed = None;
        }
        let down = |s: usize| (s + 1).min(n - 1);
        match chord.key {
            Key::Up if plain => self.ssh.sel = self.ssh.sel.saturating_sub(1),
            Key::Down if plain => self.ssh.sel = down(self.ssh.sel),
            Key::Tab if chord.shift && !held => self.ssh.sel = self.ssh.sel.saturating_sub(1),
            Key::Tab if !held => self.ssh.sel = down(self.ssh.sel),
            Key::Enter if !held => return self.ssh_activate(),
            Key::Esc if !self.ssh.input.is_empty() => {
                self.ssh.input.clear();
                self.ssh.error = None;
                self.ssh.sel = 0;
            }
            Key::Esc => self.ssh_close(),
            Key::Delete if plain && matches!(cur, Row::Root(..)) => return self.ssh_ask_forget(cur),
            Key::Char('d') if plain && matches!(cur, Row::Root(..)) => return self.ssh_ask_forget(cur),
            Key::Char('e') if plain && matches!(cur, Row::Root(..) | Row::Alias(_)) => return self.ssh_edit_start(cur),
            Key::Backspace if held || chord.alt => {
                self.ssh.input.clear();
                self.ssh.sel = 0;
            }
            Key::Backspace => {
                self.ssh.input.pop();
                self.ssh.error = None;
                self.ssh.sel = 0;
            }
            _ if paste => return iced::clipboard::read().map(|s| Message::Ssh(Msg::Pasted(s))),
            // Typing anywhere goes into the box (and filters the list).
            _ if !held => {
                let t: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    self.ssh.input.push_str(&t);
                    self.ssh.error = None;
                    self.ssh.sel = 0;
                }
            }
            _ => {
                if let Some(c) = self.keymap.lookup(&chord).filter(|c| super::app_wide(*c)) {
                    return self.exec(c);
                }
            }
        }
        Task::none()
    }

    /// Enter on the selected row.
    fn ssh_activate(&mut self) -> Task<Message> {
        let rows = self.ssh_rows();
        let r = rows.get(self.ssh.sel.min(rows.len() - 1)).copied().unwrap_or(Row::Input);
        if self.ssh.armed.is_some_and(|a| a != r) {
            self.ssh.armed = None;
        }
        match r {
            Row::Input => self.ssh_connect_typed(),
            Row::Cancel => {
                self.cancel_connect("cancelled");
                self.flash_info("connecting cancelled")
            }
            Row::Trust => self.ssh_trust(),
            Row::Root(i, j) => {
                let (name, p) = self.ssh.profiles[i].clone();
                let root = p.all_roots().get(j).cloned().unwrap_or_else(|| p.root.clone());
                match launch_for(&name, &p, &root) {
                    Ok(l) => self.ssh_switch(r, l),
                    Err(e) => self.flash_warn(format!("@{name}: {e:#}")),
                }
            }
            Row::Home => {
                let l = self.home.clone();
                self.ssh_switch(r, l)
            }
            Row::Alias(i) => {
                let alias = self.ssh.aliases[i].clone();
                self.ssh_connect_host(r, &alias)
            }
        }
    }

    /// Enter in the box: a saved connection or ~/.ssh/config host by its exact name, else a new host.
    fn ssh_connect_typed(&mut self) -> Task<Message> {
        let typed = self.ssh.input.trim().to_owned();
        if typed.is_empty() {
            // Nothing typed: Enter takes the first connection in the list.
            let rows = self.ssh_rows();
            if let Some(i) = rows.iter().position(|r| matches!(r, Row::Root(..) | Row::Home | Row::Alias(_))) {
                self.ssh.sel = i;
                return self.ssh_activate();
            }
            return Task::none();
        }
        let typed_name = typed.trim_start_matches('@');
        if let Some(i) = self.ssh.profiles.iter().position(|(n, p)| n == typed_name || p.ssh.as_deref() == Some(typed.as_str())) {
            let (name, p) = self.ssh.profiles[i].clone();
            return match launch_for(&name, &p, &p.root) {
                Ok(l) => self.ssh_switch(Row::Root(i, 0), l),
                Err(e) => self.flash_warn(format!("@{name}: {e:#}")),
            };
        }
        self.ssh_connect_host(Row::Input, &typed)
    }

    /// Connect to a host nib has no profile for, in its home folder. Saved once it works.
    fn ssh_connect_host(&mut self, r: Row, host: &str) -> Task<Message> {
        let p = match parse_host(host) {
            Ok(p) => p,
            Err(e) => {
                self.ssh.error = Some(e);
                return Task::none();
            }
        };
        // A free name: the host's, or with -2, -3… when another host has it.
        let base = name_for(p.ssh.as_deref().unwrap_or(host));
        let taken = |n: &str| self.ssh.profiles.iter().any(|(m, q)| m == n && q.ssh != p.ssh);
        let name = (1..).map(|k| if k == 1 { base.clone() } else { format!("{base}-{k}") }).find(|n| !taken(n)).unwrap_or(base);
        if let Err(e) = check_profile_name(&name) {
            self.ssh.error = Some(format!("{e:#}"));
            return Task::none();
        }
        match launch_for(&name, &p, "~") {
            Ok(l) => self.ssh_switch(r, l),
            Err(e) => {
                self.ssh.error = Some(format!("{e:#}"));
                Task::none()
            }
        }
    }

    /// Switch the window to `l`, asking first (press again) if that closes anything.
    fn ssh_switch(&mut self, r: Row, l: Launch) -> Task<Message> {
        if self.connecting.as_ref().is_some_and(|c| c.launch.label == l.label && c.launch.root == l.root) {
            return self.flash_info(format!("already connecting to {}", l.label));
        }
        let here = self.connected && !self.disconnected && self.launch.label == l.label && self.launch.target == l.target;
        if here && l.root != self.launch.root {
            // Same machine, other folder: no new connection needed.
            let root = self.untilde(&l.root);
            self.ssh_close();
            return self.change_root(root);
        }
        if here {
            return self.flash_info(format!("already on {} · {}", l.label, l.root));
        }
        if let Some(cost) = self.switch_cost() {
            if self.ssh.armed != Some(r) {
                self.ssh.armed = Some(r);
                return self.flash_warn(format!("{cost} — press again to switch to {}", l.label));
            }
        }
        self.ssh.armed = None;
        self.ssh.error = None;
        self.ssh.key = None;
        self.ssh.input.clear();
        self.ssh.sel = 0;
        self.connect(l, false, true, None, true)
    }

    /// `~/x` -> `/home/tal/x` on the connected machine (the daemon expands `~` only at Hello).
    fn untilde(&self, p: &str) -> String {
        match (&self.remote_home, p.strip_prefix('~')) {
            (Some(h), Some(rest)) => format!("{h}{rest}"),
            _ => p.to_owned(),
        }
    }

    fn ssh_ask_forget(&mut self, r: Row) -> Task<Message> {
        let Row::Root(i, j) = r else { return Task::none() };
        let (name, p) = &self.ssh.profiles[i];
        let roots = p.all_roots();
        let what = if roots.len() > 1 { format!("{} on @{name}", roots[j]) } else { format!("@{name}") };
        self.ssh.forgetting = Some(r);
        self.flash_warn(format!("forget {what}? y / n"))
    }

    /// Forget one root of a connection, or the connection when it was its only root.
    fn ssh_forget(&mut self, r: Row) -> Task<Message> {
        let Row::Root(i, j) = r else { return Task::none() };
        let (name, mut p) = self.ssh.profiles[i].clone();
        let mut roots = p.all_roots();
        let result = if roots.len() > 1 {
            let gone = roots.remove(j);
            p.root = roots[0].clone();
            p.roots = roots;
            Config::save_profile(&name, &p, None).map(|_| format!("forgot {gone} on @{name}"))
        } else {
            Config::remove_profile(&name).map(|_| format!("forgot @{name}"))
        };
        self.ssh_reload();
        match result {
            Ok(msg) => self.flash_info(msg),
            Err(e) => self.flash_warn(format!("{e:#}")),
        }
    }

    /// A person read the fingerprint and trusts the key: add it to known_hosts, connect again.
    fn ssh_trust(&mut self) -> Task<Message> {
        let Some(KeyCheck::Ready(l, p)) = &self.ssh.key else { return Task::none() };
        let (l, p) = (l.clone(), p.clone());
        match crate::ssh::trust_host_key(&p) {
            Ok(file) => {
                crate::control::log("trust", &format!("{}: {} added to {}", l.label, p.fingerprints.join(", "), file.display()));
                self.ssh.key = None;
                let done = self.flash_info(format!("host key trusted (added to {})", file.display()));
                Task::batch([done, self.connect(l, false, true, None, true)])
            }
            Err(e) => self.flash_warn(format!("couldn't trust the key: {e:#}")),
        }
    }

    /// Connecting failed: show the panel, and when the host key is the problem, fetch its
    /// fingerprint so the person can check and trust it right there.
    pub(super) fn ssh_connect_failed(&mut self, l: &Launch) -> Task<Message> {
        if self.headless {
            return Task::none();
        }
        if !self.ssh.open {
            self.ssh_open();
        }
        let key = self.conn_error.as_ref().is_some_and(|(_, e)| e.contains("host key") || e.contains("Host key verification failed"));
        let Some((host, root, nibd)) = test_args(l).filter(|_| key) else { return Task::none() };
        self.ssh.key_seq += 1;
        let seq = self.ssh.key_seq;
        self.ssh.key = Some(KeyCheck::Fetching(seq, l.clone()));
        Task::perform(background(move || crate::ssh::probe(&host, &root, nibd.as_deref())), move |p| match p {
            Some(p) => Message::Ssh(Msg::KeyFetched(seq, Arc::new(p))),
            None => Message::Tick,
        })
    }

    // -----------------------------------------------------------------------------------------
    // folder browser
    // -----------------------------------------------------------------------------------------

    /// The folder browser is up, where the file tree is.
    pub(super) fn ssh_browsing(&self) -> bool {
        self.ssh.browse.is_some()
    }

    /// Show the subfolders of `dir` (absolute, on the connected machine) where the file tree
    /// is, with the keyboard: picking one makes it the root, and the tree comes back.
    pub(super) fn ssh_browse(&mut self, dir: String) -> Task<Message> {
        if !self.connected || self.disconnected {
            return self.flash_warn("not connected");
        }
        // The SSH panel brought us here (a new host): it's done.
        if self.ssh.open {
            self.ssh_close();
        }
        self.tree.open = true;
        self.focus = Focus::Tree;
        let b = self.ssh.browse.get_or_insert_with(Default::default);
        b.dir = dir.clone();
        b.filter.clear();
        b.sel = 0;
        b.loading = true;
        b.error = None;
        self.request(Request::ListDir { path: dir }, Pending::Browse)
    }

    pub(super) fn on_browse(&mut self, result: Result<Response, RpcError>) -> Task<Message> {
        let Some(b) = &mut self.ssh.browse else { return Task::none() };
        b.loading = false;
        match result {
            Ok(Response::Dir { path, entries }) => {
                b.dir = path;
                b.entries = entries
                    .into_iter()
                    .filter(|e| matches!(e.kind, EntryKind::Dir | EntryKind::Symlink))
                    .map(|e| (e.name, e.kind == EntryKind::Symlink))
                    .collect();
                b.error = None;
            }
            Ok(_) => b.error = Some("unexpected answer from nibd".into()),
            Err(e) => b.error = Some(e.msg),
        }
        Task::none()
    }

    /// The folder the browser's selection stands for (row 0: the one being shown).
    fn ssh_browse_target(&self) -> Option<String> {
        let b = self.ssh.browse.as_ref()?;
        match b.sel {
            0 => Some(b.dir.clone()),
            i => b.shown().get(i - 1).map(|(n, _)| b.path_of(n)),
        }
    }

    fn ssh_browse_into(&mut self) -> Task<Message> {
        let Some(b) = &self.ssh.browse else { return Task::none() };
        if b.sel == 0 {
            return self.ssh_browse_open();
        }
        match self.ssh_browse_target() {
            Some(d) => self.ssh_browse(d),
            None => Task::none(),
        }
    }

    fn ssh_browse_up(&mut self) -> Task<Message> {
        match self.ssh.browse.as_ref().and_then(Browse::parent) {
            Some(p) => self.ssh_browse(p),
            None => Task::none(),
        }
    }

    /// Open the selected folder as the project, on the same connection.
    fn ssh_browse_open(&mut self) -> Task<Message> {
        let Some(dir) = self.ssh_browse_target() else { return Task::none() };
        self.ssh.browse = None;
        if dir == self.root {
            self.tree.open = true;
            self.focus = Focus::Tree;
            return self.tree_refresh();
        }
        self.change_root(dir)
    }

    pub(super) fn ssh_browse_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let paste = self.keymap.lookup(&chord) == Some(Command::Paste);
        let Some(b) = &mut self.ssh.browse else { return Task::none() };
        let n = b.shown().len() + 1;
        match chord.key {
            Key::Enter if held => return self.ssh_browse_open(),
            Key::Enter | Key::Right if !chord.alt => return self.ssh_browse_into(),
            Key::Left if !held => return self.ssh_browse_up(),
            Key::Up => b.sel = b.sel.saturating_sub(1),
            Key::Down => b.sel = (b.sel + 1).min(n - 1),
            Key::Tab if chord.shift => b.sel = b.sel.saturating_sub(1),
            Key::Tab => b.sel = (b.sel + 1).min(n - 1),
            Key::Esc if !b.filter.is_empty() => {
                b.filter.clear();
                b.sel = 0;
            }
            // Back to the tree, the root as it was.
            Key::Esc => self.ssh.browse = None,
            Key::Backspace if b.filter.is_empty() => return self.ssh_browse_up(),
            Key::Backspace if held || chord.alt => b.filter.clear(),
            Key::Backspace => {
                b.filter.pop();
            }
            _ if paste => return iced::clipboard::read().map(|s| Message::Ssh(Msg::Pasted(s))),
            _ if !held => {
                let t: String = text.unwrap_or_default().chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    b.filter.push_str(&t);
                    // The best match is what Enter takes.
                    b.sel = usize::from(!b.shown().is_empty());
                }
            }
            _ => {
                if let Some(c) = self.keymap.lookup(&chord).filter(|c| super::app_wide(*c)) {
                    return self.exec(c);
                }
            }
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // editing a saved connection
    // -----------------------------------------------------------------------------------------

    fn ssh_edit_start(&mut self, r: Row) -> Task<Message> {
        let (orig, p) = match r {
            Row::Root(i, _) => {
                let (name, p) = self.ssh.profiles[i].clone();
                if p.ssh.is_none() {
                    return self.flash_info(format!("@{name} is a {} connection: change it in config.toml", p.kind()));
                }
                (Some(name), p)
            }
            Row::Alias(i) => {
                let alias = self.ssh.aliases[i].clone();
                (None, Profile { ssh: Some(alias), root: "~".into(), ..Default::default() })
            }
            _ => return Task::none(),
        };
        let name = orig.clone().unwrap_or_else(|| name_for(p.ssh.as_deref().unwrap_or("")));
        let fields = [
            name,
            p.ssh.clone().unwrap_or_default(),
            p.user.clone().unwrap_or_default(),
            p.port.map(|p| p.to_string()).unwrap_or_default(),
            p.identity.clone().unwrap_or_default(),
            p.jump.clone().unwrap_or_default(),
            p.nibd.clone().unwrap_or_default(),
        ];
        self.ssh.edit = Some(Edit::new(orig, fields));
        Task::none()
    }

    /// The form as a connection (keeping the original's folders).
    fn ssh_edit_profile(&self) -> Result<(String, Profile, Option<Profile>), String> {
        let e = self.ssh.edit.as_ref().ok_or("not editing")?;
        let v = |i: usize| e.fields[i].trim().to_owned();
        let opt = |i: usize| Some(v(i)).filter(|s| !s.is_empty());
        let name = v(0).trim_start_matches('@').to_owned();
        check_profile_name(&name).map_err(|e| format!("{e:#}"))?;
        let old = e.orig.as_ref().and_then(|o| self.ssh.profiles.iter().find(|(n, _)| n == o)).map(|(_, p)| p.clone());
        let base = old.clone().unwrap_or(Profile { root: "~".into(), ..Default::default() });
        let port = match opt(3) {
            Some(p) => Some(p.parse::<u16>().map_err(|_| format!("port {p:?} is not a number from 1 to 65535"))?),
            None => None,
        };
        let p = Profile { ssh: Some(opt(1).ok_or("host is empty")?), user: opt(2), port, identity: opt(4), jump: opt(5), nibd: opt(6), ..base };
        p.check().map_err(|e| format!("{e:#}"))?;
        if e.orig.as_deref() != Some(name.as_str()) && self.ssh.profiles.iter().any(|(n, _)| *n == name) {
            return Err(format!("@{name} already exists: pick another name"));
        }
        Ok((name, p, old))
    }

    /// Show what saving would change, and ask.
    fn ssh_edit_confirm(&mut self) -> Task<Message> {
        let (name, p, old) = match self.ssh_edit_profile() {
            Ok(v) => v,
            Err(err) => {
                if let Some(e) = &mut self.ssh.edit {
                    e.error = Some(err);
                }
                return Task::none();
            }
        };
        let Some(e) = &self.ssh.edit else { return Task::none() };
        let show = |v: Option<String>| v.unwrap_or_else(|| "(not set)".into());
        let mut changes = Vec::new();
        match &e.orig {
            Some(o) if *o != name => changes.push(format!("name: @{o} → @{name}")),
            None => changes.push(format!("new connection @{name}")),
            _ => {}
        }
        let old = old.unwrap_or_default();
        for (label, a, b) in [
            ("host", old.ssh.clone(), p.ssh.clone()),
            ("user", old.user.clone(), p.user.clone()),
            ("port", old.port.map(|p| p.to_string()), p.port.map(|p| p.to_string())),
            ("key file", old.identity.clone(), p.identity.clone()),
            ("jump host", old.jump.clone(), p.jump.clone()),
            ("nibd", old.nibd.clone(), p.nibd.clone()),
        ] {
            if a != b {
                changes.push(format!("{label}: {} → {}", show(a), show(b)));
            }
        }
        if changes.is_empty() {
            self.ssh.edit = None;
            return self.flash_info("nothing changed");
        }
        if let Some(e) = &mut self.ssh.edit {
            e.confirm = Some(changes);
            e.error = None;
        }
        Task::none()
    }

    fn ssh_edit_save(&mut self) -> Task<Message> {
        let (name, p, _) = match self.ssh_edit_profile() {
            Ok(v) => v,
            Err(err) => {
                if let Some(e) = &mut self.ssh.edit {
                    e.error = Some(err);
                    e.confirm = None;
                }
                return Task::none();
            }
        };
        let orig = self.ssh.edit.as_ref().and_then(|e| e.orig.clone());
        if let Err(err) = Config::save_profile(&name, &p, orig.as_deref()) {
            if let Some(e) = &mut self.ssh.edit {
                e.error = Some(format!("{err:#}"));
                e.confirm = None;
            }
            return Task::none();
        }
        self.ssh.edit = None;
        self.ssh_reload();
        let here = self.launch.profile.is_some() && self.launch.profile == orig;
        if here {
            // This window stays as it is; the changes apply from the next connect.
            self.launch.profile = Some(name.clone());
            return self.flash_info(format!("saved @{name} (applies the next time you connect)"));
        }
        self.flash_info(format!("saved @{name}"))
    }

    fn ssh_edit_key(&mut self, chord: KeyChord, text: Option<String>) -> Task<Message> {
        let held = chord.ctrl || chord.logo;
        let cmd = self.keymap.lookup(&chord);
        let Some(e) = &mut self.ssh.edit else { return Task::none() };
        if e.confirm.is_some() {
            if matches!(chord.key, Key::Char('y') | Key::Enter) && !held {
                return self.ssh_edit_save();
            }
            e.confirm = None;
            return Task::none();
        }
        let last = e.shown() - 1;
        match chord.key {
            Key::Esc => self.ssh.edit = None,
            _ if cmd == Some(Command::Save) => return self.ssh_edit_confirm(),
            Key::Enter if e.sel == last => return self.ssh_edit_confirm(),
            Key::Enter | Key::Down => e.sel = (e.sel + 1).min(last),
            Key::Tab if !chord.shift => e.sel = (e.sel + 1).min(last),
            Key::Up | Key::Tab => e.sel = e.sel.saturating_sub(1),
            Key::Backspace if held || chord.alt => e.fields[e.sel].clear(),
            Key::Backspace => {
                e.fields[e.sel].pop();
            }
            _ if cmd == Some(Command::Paste) => return iced::clipboard::read().map(|s| Message::Ssh(Msg::Pasted(s))),
            _ if !held => {
                e.fields[e.sel].extend(text.unwrap_or_default().chars().filter(|c| !c.is_control()));
                e.error = None;
            }
            _ => {}
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // askpass
    // -----------------------------------------------------------------------------------------

    /// ssh (through `nib` as askpass) asks something while connecting. Only the ssh this window
    /// started knows the token, so agents on the socket can't raise prompts.
    pub(super) fn on_askpass(&mut self, call: Call) -> Task<Message> {
        let mine = self.connecting.as_ref().is_some_and(|c| Some(c.token.as_str()) == call.str_arg("token"));
        if !mine {
            call.err("not asked for");
            return Task::none();
        }
        if self.headless {
            call.err("headless: there is nobody to ask");
            return Task::none();
        }
        let kind = match call.str_arg("kind").unwrap_or("") {
            "confirm" => AskKind::Confirm,
            "none" => AskKind::Notice,
            _ => AskKind::Secret,
        };
        let prompt = call.str_arg("prompt").unwrap_or("ssh asks:").trim().to_owned();
        self.ssh_ask_cancel("asked something else");
        self.ssh.ask = Some(Ask { prompt, kind, input: String::new(), call });
        if !self.ssh.open {
            self.ssh_open();
        }
        self.ssh_focus();
        iced::window::latest().and_then(iced::window::gain_focus)
    }

    fn ssh_answer(&mut self, yes: bool) -> Task<Message> {
        let Some(a) = self.ssh.ask.take() else { return Task::none() };
        match (a.kind, yes) {
            (AskKind::Secret, true) => a.call.ok(json!({ "answer": a.input })),
            (AskKind::Confirm, true) => a.call.ok(json!({ "answer": "yes" })),
            (AskKind::Notice, _) => a.call.ok(json!({})),
            (_, false) => a.call.err("cancelled"),
        }
        Task::none()
    }

    /// Drop ssh's question (it gave up, or the attempt ended).
    pub(super) fn ssh_ask_cancel(&mut self, why: &str) {
        if let Some(a) = self.ssh.ask.take() {
            a.call.err(why);
        }
    }

    // -----------------------------------------------------------------------------------------
    // control socket: ssh.*
    // -----------------------------------------------------------------------------------------

    pub(super) fn on_ssh_ctl(&mut self, call: Call) -> Task<Message> {
        let r = match call.cmd() {
            "ssh.list" => Ok(self.ssh_list_json()),
            "ssh.add" if call.req.get("test").and_then(Value::as_bool) == Some(true) => match self.ctl_ssh_add(&call) {
                Ok(saved) => return self.ctl_ssh_test_then(call, saved),
                Err(e) => Err(e),
            },
            "ssh.add" => self.ctl_ssh_add(&call),
            "ssh.update" => self.ctl_ssh_update(&call),
            "ssh.remove" => self.ctl_ssh_remove(&call),
            "ssh.test" => return self.ctl_ssh_test_then(call, json!({})),
            "ssh.connect" => return self.ctl_ssh_connect(call),
            "ssh.root" => return self.ctl_ssh_root(call),
            "ssh.cancel" => {
                let was = self.connecting.as_ref().map(|c| c.launch.label.clone());
                self.cancel_connect("cancelled");
                Ok(json!({ "cancelled": was }))
            }
            other => Err(format!("unknown cmd {other:?} (ssh.list, ssh.add, ssh.update, ssh.remove, ssh.test, ssh.connect, ssh.root, ssh.cancel)")),
        };
        match r {
            Ok(v) => call.ok(v),
            Err(e) => call.err(e),
        }
        Task::none()
    }

    fn ssh_list_json(&self) -> Value {
        let profiles = load_profiles().unwrap_or_default();
        let current = self.launch.profile.as_deref();
        let list: Vec<Value> = profiles
            .iter()
            .map(|(name, p)| {
                let target = p.target().map(|t| target_text(&t)).unwrap_or_else(|e| format!("invalid: {e:#}"));
                json!({
                    "name": name,
                    "kind": p.kind(),
                    "host": p.ssh.as_deref().or(p.wsl.as_deref()),
                    "user": p.user, "port": p.port, "identity": p.identity, "jump": p.jump,
                    "root": p.root, "roots": p.all_roots(), "nibd": p.nibd,
                    "target": target,
                    "current": Some(name.as_str()) == current,
                })
            })
            .collect();
        let used: Vec<&str> = profiles.iter().filter_map(|(_, p)| p.ssh.as_deref()).collect();
        let aliases: Vec<String> = crate::ssh::config_hosts().into_iter().filter(|a| !used.contains(&a.as_str())).collect();
        json!({
            "profiles": list,
            "current": self.connection_json(),
            "home": self.home.profile.is_none().then(|| json!({ "label": self.home.label, "target": target_text(&self.home.target), "root": self.home.root })),
            "ssh_config_hosts": aliases,
            "config": Config::path(),
        })
    }

    fn ctl_ssh_add(&mut self, call: &Call) -> Result<Value, String> {
        let name = ctl_name(call)?;
        let profiles = load_profiles()?;
        if profiles.iter().any(|(n, _)| *n == name) {
            return Err(format!("@{name} already exists (ssh.update changes it)"));
        }
        let mut p = Profile { root: "~".into(), ..Default::default() };
        apply_fields(&mut p, &call.req)?;
        if p.ssh.is_none() && p.wsl.is_none() {
            return Err("ssh.add needs \"host\"".into());
        }
        Config::save_profile(&name, &p, None).map_err(|e| format!("{e:#}"))?;
        self.ssh_reload();
        Ok(json!({ "name": name, "saved": Config::path() }))
    }

    fn ctl_ssh_update(&mut self, call: &Call) -> Result<Value, String> {
        let name = ctl_name(call)?;
        let profiles = load_profiles()?;
        let mut p = profiles.into_iter().find(|(n, _)| *n == name).map(|(_, p)| p).ok_or(format!("no profile @{name}"))?;
        apply_fields(&mut p, &call.req)?;
        let new_name = match call.req.get("rename") {
            Some(Value::String(r)) => r.clone(),
            None | Some(Value::Null) => name.clone(),
            Some(_) => return Err("\"rename\" is a string".into()),
        };
        Config::save_profile(&new_name, &p, Some(&name)).map_err(|e| format!("{e:#}"))?;
        if self.launch.profile.as_deref() == Some(name.as_str()) && new_name != name {
            // The window stays connected; it just goes by the new name from the next connect.
            self.launch.profile = Some(new_name.clone());
        }
        self.ssh_reload();
        Ok(json!({ "name": new_name }))
    }

    fn ctl_ssh_remove(&mut self, call: &Call) -> Result<Value, String> {
        let name = ctl_name(call)?;
        Config::remove_profile(&name).map_err(|e| format!("{e:#}"))?;
        self.ssh_reload();
        Ok(json!({ "removed": name }))
    }

    /// Test, and answer with `extra` merged in. `{"name": …}` tests a saved connection; host
    /// fields (as in ssh.add) test one without saving.
    fn ctl_ssh_test_then(&mut self, call: Call, extra: Value) -> Task<Message> {
        let saved = extra.get("saved").is_some();
        let launch = if call.req.get("host").is_some() && !saved {
            let mut p = Profile { root: "~".into(), ..Default::default() };
            apply_fields(&mut p, &call.req).and_then(|_| launch_for("test", &p, &p.root).map_err(|e| format!("{e:#}")))
        } else {
            self.ctl_profile_launch(&call)
        };
        let args = launch.and_then(|l| test_args(&l).ok_or_else(|| format!("{} isn't an ssh connection", l.label)));
        match args {
            Ok((host, root, nibd)) => {
                std::thread::spawn(move || {
                    let p = crate::ssh::probe(&host, &root, nibd.as_deref());
                    let mut out = extra;
                    out["test"] = p.to_json();
                    call.ok(out);
                });
            }
            Err(e) => call.err(e),
        }
        Task::none()
    }

    /// A saved connection named in a request, at its `root` (default: the last one used).
    fn ctl_profile_launch(&self, call: &Call) -> Result<Launch, String> {
        let name = ctl_name(call)?;
        let p = load_profiles()?.into_iter().find(|(n, _)| *n == name).map(|(_, p)| p).ok_or(format!("no profile @{name}"))?;
        let root = call.str_arg("root").map(str::to_owned).unwrap_or_else(|| p.root.clone());
        launch_for(&name, &p, &root).map_err(|e| format!("{e:#}"))
    }

    /// Switch this window to a saved connection. Answers once the new daemon is up. ssh may not
    /// ask anything (agents have no password to give); unknown host keys are refused.
    fn ctl_ssh_connect(&mut self, call: Call) -> Task<Message> {
        let force = call.req.get("force").and_then(Value::as_bool).unwrap_or(false);
        let launch = if call.req.get("home").and_then(Value::as_bool) == Some(true) { Ok(self.home.clone()) } else { self.ctl_profile_launch(&call) };
        let l = match launch {
            Ok(l) => l,
            Err(e) => {
                call.err(e);
                return Task::none();
            }
        };
        if let (Some(cost), false) = (self.switch_cost(), force) {
            call.err(format!("{cost}; pass \"force\": true (nib ctl ssh connect NAME --force) to switch anyway"));
            return Task::none();
        }
        self.connect(l, true, force, Some(call), false)
    }

    /// Make another folder on the connected machine the root (`path`: absolute, `~/…`, or
    /// relative to the current root). Tabs and terminals stay.
    fn ctl_ssh_root(&mut self, call: Call) -> Task<Message> {
        let Some(p) = call.str_arg("path").map(str::to_owned) else {
            call.err("ssh.root needs \"path\"");
            return Task::none();
        };
        let abs = if nib_proto::path::is_abs(&p) {
            p
        } else if p.starts_with('~') {
            if self.remote_home.is_none() {
                call.err("the home folder on this machine isn't known yet: give an absolute path");
                return Task::none();
            }
            self.untilde(&p)
        } else {
            format!("{}/{p}", self.root.trim_end_matches('/'))
        };
        if abs == self.root {
            call.ok(json!({ "root": self.root }));
            return Task::none();
        }
        self.connect_call = Some(call);
        self.change_root(abs)
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    pub(super) fn ssh_panel(&self) -> Option<Element<'_, Message>> {
        let ui = &self.ssh;
        if !ui.open {
            return None;
        }
        let th = &self.theme;
        let (accent, dim, fg) = (th.accent, th.dim, th.foreground);
        let focused = self.focus == Focus::Ssh;
        let rows = self.ssh_rows();
        let sel = ui.sel.min(rows.len() - 1);
        let at = |r: Row| rows.iter().position(|x| *x == r);
        let is_sel = |r: Row| focused && ui.ask.is_none() && at(r) == Some(sel);
        let click = |r: Row| Message::Ssh(Msg::Click(at(r).unwrap_or(0)));

        let here = if self.connected && !self.disconnected { format!("{} · {}", self.label, short_path(&self.launch.root, 40)) } else { String::new() };
        let close = button(text("✕").size(12).color(dim)).on_press(Message::Ssh(Msg::Close)).padding([1, 6]).style(|_, _| button::Style::default());
        let header = row![
            text("SSH").size(12).color(fg),
            container(text(here).size(12).color(dim).wrapping(text::Wrapping::None)).width(Length::Fill).clip(true),
            close
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding(Padding { left: 14.0, right: 8.0, top: 8.0, bottom: 6.0 });

        let mut body = column![].spacing(6);

        // ssh's question comes first: nothing else matters until it's answered.
        if let Some(a) = &ui.ask {
            body = body.push(self.ssh_ask_box(a, focused));
        }

        if let Some(e) = &ui.edit {
            body = body.push(self.ssh_edit_view(e, focused));
            return Some(self.ssh_frame(header, body, focused));
        }

        // The box.
        body = body.push(mouse_area(self.ssh_box(ui.input.clone(), PLACEHOLDER, is_sel(Row::Input), focused)).on_press(click(Row::Input)));
        if let Some(e) = &ui.error {
            body = body.push(text(e.clone()).size(11).color(th.warn));
        }

        // Connecting, or why it didn't work.
        if let Some(c) = &self.connecting {
            let secs = c.started.elapsed().as_secs();
            let line = if secs >= 2 { format!("{}  ({secs}s)", c.status) } else { c.status.clone() };
            body = body.push(
                row![text(line).size(12).color(accent), space::horizontal(), self.ssh_button("Cancel", Some(click(Row::Cancel)), is_sel(Row::Cancel))]
                    .align_y(Alignment::Center),
            );
            if let Some(f) = c.fraction {
                body = body.push(self.progress(f));
            }
        } else if let Some((label, e)) = &self.conn_error {
            if !matches!(&ui.key, Some(KeyCheck::Ready(..))) {
                body = body.push(container(text(format!("couldn't connect to {label}: {e}")).size(11).color(th.warn)).width(Length::Fill));
            }
        }
        match &ui.key {
            Some(KeyCheck::Fetching(_, l)) => body = body.push(text(format!("checking {}'s host key…", l.label)).size(12).color(dim)),
            Some(KeyCheck::Ready(l, p)) => {
                let color = if p.stage == Stage::HostKeyUnknown { fg } else { th.warn };
                let mut k = column![text(p.message.clone()).size(12).color(color)].spacing(6);
                for fp in &p.fingerprints {
                    k = k.push(text(fp.clone()).size(11).font(self.face.font).color(fg));
                }
                if at(Row::Trust).is_some() {
                    k = k.push(text("Compare it with the one the machine's owner gives you before trusting it.").size(11).color(dim));
                    k = k.push(self.ssh_button(format!("Trust and connect to {}", l.label), Some(click(Row::Trust)), is_sel(Row::Trust)));
                }
                body = body.push(container(k).padding(10).width(Length::Fill).style(move |_| container::Style {
                    border: Border { color: dim, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                }));
            }
            None => {}
        }

        // Connections.
        body = body.push(space().height(6));
        if let Some(e) = &ui.load_error {
            body = body.push(text(e.clone()).size(11).color(th.warn));
        }
        let mut listed = 0;
        for r in &rows {
            let line = match *r {
                Row::Root(i, j) => {
                    let (name, p) = &ui.profiles[i];
                    let root = p.all_roots().get(j).cloned().unwrap_or_default();
                    // On this host at a folder it doesn't list (home): the dot goes on its first row.
                    let on_host = self.connected && self.launch.profile.as_deref() == Some(name.as_str());
                    let listed = p.all_roots().contains(&self.launch.root);
                    let current = on_host && (self.launch.root == root || (!listed && j == 0));
                    let (name, host) = if j == 0 {
                        let host = p.target().map(|t| target_text(&t)).unwrap_or_default();
                        (name.clone(), if host == *name { String::new() } else { host })
                    } else {
                        (String::new(), String::new())
                    };
                    self.ssh_list_row(click(*r), is_sel(*r), current, name, host, root)
                }
                Row::Home => {
                    let current = self.connected && self.launch.profile.is_none();
                    self.ssh_list_row(click(*r), is_sel(*r), current, self.home.label.clone(), target_text(&self.home.target), self.home.root.clone())
                }
                Row::Alias(i) => self.ssh_list_row(click(*r), is_sel(*r), false, ui.aliases[i].clone(), "~/.ssh/config".into(), String::new()),
                _ => continue,
            };
            body = body.push(line);
            listed += 1;
        }
        if listed == 0 && ui.load_error.is_none() {
            let msg = if ui.input.trim().is_empty() { "No connections yet: type a host above." } else { "↵ connects to this host" };
            body = body.push(text(msg).size(11).color(dim));
        }
        if focused && ui.ask.is_none() {
            let mut hints = row![self.hint("↵", "connect"), self.hint("↑↓", "choose")].spacing(14);
            if matches!(rows.get(sel), Some(Row::Root(..))) {
                hints = hints.push(self.hint("e", "edit")).push(self.hint("d", "forget"));
            } else if matches!(rows.get(sel), Some(Row::Alias(_))) {
                hints = hints.push(self.hint("e", "edit & save"));
            }
            hints = hints.push(self.hint("esc", if ui.input.is_empty() { "close" } else { "clear" }));
            body = body.push(space().height(4)).push(hints);
        }

        Some(self.ssh_frame(header, body, focused))
    }

    /// The panel around `body`: scrolls, keyboard halo, click to focus.
    fn ssh_frame<'a>(&'a self, header: iced::widget::Row<'a, Message>, body: iced::widget::Column<'a, Message>, focused: bool) -> Element<'a, Message> {
        let (accent, panel) = (self.theme.accent, self.theme.panel);
        let body = scrollable(body.padding(Padding { left: 14.0, right: 14.0, top: 10.0, bottom: 12.0 })).style(crate::theme::scrollbar).height(Length::Fill);
        let halo = if focused { accent } else { Color::TRANSPARENT };
        let panel = container(column![header, self.hrule(), body]).padding(2).width(WIDTH).height(Length::Fill).style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: halo, width: 2.0, radius: 3.0.into() },
            ..Default::default()
        });
        mouse_area(panel).on_press(Message::Ssh(Msg::Focus)).into()
    }

    /// The folder browser, in the file tree's place.
    pub(super) fn ssh_browse_view(&self) -> Option<Element<'_, Message>> {
        let b = self.ssh.browse.as_ref()?;
        let focused = self.focus == Focus::Tree;
        let th = &self.theme;
        let (accent, dim, fg, sel_bg) = (th.accent, th.dim, th.foreground, th.selection);
        let mut c = column![text(format!("Open a folder on {}", self.label)).size(12).color(fg)].spacing(6);
        let up = button(text("↑ up").size(11).color(dim)).on_press(Message::Ssh(Msg::BrowseUp)).padding([1, 6]).style(|_, _| button::Style::default());
        c = c.push(
            row![container(text(short_path(&self.tilde(&b.dir), 32)).size(12).color(accent).wrapping(text::Wrapping::None)).width(Length::Fill).clip(true), up]
                .align_y(Alignment::Center),
        );
        c = c.push(self.ssh_box(b.filter.clone(), "type to filter", true, focused));
        let item = |i: usize, label: String, color: Color| -> Element<'_, Message> {
            let selected = b.sel == i;
            let r = container(text(label).size(12).color(color).wrapping(text::Wrapping::None))
                .padding([3, 6])
                .width(Length::Fill)
                .clip(true)
                .style(move |_| container::Style {
                    background: selected.then(|| sel_bg.into()),
                    border: Border { radius: 3.0.into(), ..Default::default() },
                    ..Default::default()
                });
            mouse_area(r).on_press(Message::Ssh(Msg::BrowseInto(i))).into()
        };
        let mut list = column![item(0, "·  this folder".into(), fg)].spacing(2);
        if b.loading {
            list = list.push(text("reading…").size(11).color(dim));
        } else if let Some(e) = &b.error {
            list = list.push(text(e.clone()).size(11).color(th.warn));
        } else {
            let shown = b.shown();
            if shown.is_empty() {
                list = list.push(text(if b.filter.is_empty() { "no folders here" } else { "no folder matches" }).size(11).color(dim));
            }
            for (i, (name, link)) in shown.into_iter().enumerate() {
                list = list.push(item(i + 1, format!("{name}/{}", if *link { "  →" } else { "" }), fg));
            }
        }
        c = c.push(scrollable(list).style(crate::theme::scrollbar).height(Length::Fill));
        let open = self.ssh_button("Open as project", Some(Message::Ssh(Msg::BrowseOpen)), false);
        c = c.push(open);
        if focused {
            c = c.push(row![self.hint("↵", "go in"), self.hint("←", "up")].spacing(10));
            c = c.push(row![self.hint(&format!("{MOD_PLUS}↵"), "open"), self.hint("esc", "back")].spacing(10));
        }
        Some(container(c).padding(Padding { left: 10.0, right: 10.0, top: 10.0, bottom: 10.0 }).height(Length::Fill).into())
    }

    fn ssh_edit_view<'a>(&'a self, e: &'a Edit, focused: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let (dim, fg) = (th.dim, th.foreground);
        let title = match &e.orig {
            Some(n) => format!("Edit @{n}"),
            None => format!("Save {} as a connection", e.fields[1]),
        };
        let mut c = column![text(title).size(12).color(fg)].spacing(6);
        for (i, (label, hint)) in EDIT_FIELDS.iter().enumerate().take(e.shown()) {
            let selected = e.sel == i && e.confirm.is_none();
            let r = row![text(*label).size(11).color(if selected { fg } else { dim }).width(70), self.ssh_box(e.fields[i].clone(), hint, selected, focused)]
                .spacing(8)
                .align_y(Alignment::Center);
            c = c.push(mouse_area(r).on_press(Message::Ssh(Msg::EditField(i))));
        }
        if let Some(err) = &e.error {
            c = c.push(text(err.clone()).size(11).color(th.warn));
        }
        match &e.confirm {
            Some(changes) => {
                let mut w = column![text("Save these changes to config.toml?").size(12).color(th.warn)].spacing(4);
                for ch in changes {
                    w = w.push(text(format!("  {ch}")).size(11).color(fg));
                }
                w = w.push(
                    row![
                        self.ssh_button("Save", Some(Message::Ssh(Msg::EditSave)), true),
                        self.ssh_button("Keep editing", Some(Message::Ssh(Msg::EditField(e.sel))), false),
                        self.hint("y", "save"),
                        self.hint("n", "back")
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
                c = c.push(container(w).padding(10).width(Length::Fill).style(move |_| container::Style {
                    border: Border { color: th.warn, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                }));
            }
            None => {
                c = c.push(
                    row![self.ssh_button("Save…", Some(Message::Ssh(Msg::EditSave)), false), self.ssh_button("Cancel", Some(Message::Ssh(Msg::EditCancel)), false)]
                        .spacing(8),
                );
                if focused {
                    c = c.push(row![self.hint("↑↓", "field"), self.hint(&format!("{MOD_PLUS}S"), "save"), self.hint("esc", "cancel")].spacing(12));
                }
            }
        }
        c.into()
    }

    fn ssh_ask_box<'a>(&'a self, a: &'a Ask, focused: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let (accent, dim, fg, divider) = (th.accent, th.dim, th.foreground, th.divider);
        let answer = |label: &'static str, yes: bool| {
            button(text(label).size(12).color(fg))
                .on_press(Message::Ssh(Msg::Answer(yes)))
                .padding([3, 10])
                .style(move |_, _| button::Style { border: Border { color: divider, width: 1.0, radius: 4.0.into() }, ..Default::default() })
        };
        let mut q = column![text(a.prompt.clone()).size(12).color(fg)].spacing(8);
        match a.kind {
            AskKind::Secret => {
                let dots: String = "●".repeat(a.input.chars().count());
                q = q.push(self.ssh_box(dots, "", true, focused));
                q = q.push(row![answer("Send", true), answer("Cancel", false)].spacing(8));
            }
            AskKind::Confirm => q = q.push(row![answer("Yes", true), answer("No", false)].spacing(8)),
            AskKind::Notice => q = q.push(answer("OK", true)),
        }
        let border = if focused { accent } else { dim };
        container(q)
            .padding(10)
            .width(Length::Fill)
            .style(move |_| container::Style { border: Border { color: border, width: 1.0, radius: 4.0.into() }, ..Default::default() })
            .into()
    }

    /// A one-line text box: `value`, or the dim `placeholder`; a caret when it has the keyboard.
    fn ssh_box(&self, value: String, placeholder: &str, selected: bool, focused: bool) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, dim, fg, divider) = (th.accent, th.dim, th.foreground, th.divider);
        let active = selected && focused;
        let caret = container(space()).width(2).height(15).style(move |_| container::Style::default().background(if active { accent } else { Color::TRANSPARENT }));
        let shown: Element<'_, Message> = if value.is_empty() {
            row![caret, text(placeholder.to_owned()).size(12).color(dim)].align_y(Alignment::Center).into()
        } else {
            // The caret is a glyph inside the text so it stays on the last character, never beside it.
            let typed: [iced::widget::text::Span<'_, (), iced::Font>; 2] = [span(value).color(fg), span("▏").color(if active { accent } else { Color::TRANSPARENT })];
            super::tail_line(rich_text(typed).size(12).wrapping(text::Wrapping::None))
        };
        container(shown)
            .padding([6, 8])
            .width(Length::Fill)
            .clip(true)
            .style(move |_| container::Style {
                border: Border { color: if active { accent } else { divider }, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            })
            .into()
    }

    fn ssh_button(&self, label: impl Into<String>, msg: Option<Message>, selected: bool) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, fg, sel_bg, divider) = (th.accent, th.foreground, th.selection, th.divider);
        let mut b = button(text(label.into()).size(12).color(fg)).padding([4, 10]).style(move |_, st| button::Style {
            background: (selected || matches!(st, button::Status::Hovered)).then(|| sel_bg.into()),
            border: Border { color: if selected { accent } else { divider }, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        });
        if let Some(m) = msg {
            b = b.on_press(m);
        }
        b.into()
    }

    /// `● name  host  root`; continuation rows (more roots) leave name and host empty.
    fn ssh_list_row(&self, msg: Message, selected: bool, current: bool, name: String, host: String, root: String) -> Element<'_, Message> {
        let th = &self.theme;
        let (accent, dim, fg, sel_bg) = (th.accent, th.dim, th.foreground, th.selection);
        let cell = |s: String, w: f32, c: Color| container(text(s).size(12).color(c).wrapping(text::Wrapping::None)).width(w).clip(true);
        let line = row![
            text(if current { "●" } else { " " }).size(11).color(accent).width(12),
            cell(name, 96.0, fg),
            cell(host, 116.0, dim),
            container(text(short_path(&root, 20)).size(12).color(if current { fg } else { dim }).wrapping(text::Wrapping::None)).width(Length::Fill).clip(true),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let r = container(line).padding([3, 6]).width(Length::Fill).style(move |_| container::Style {
            background: selected.then(|| sel_bg.into()),
            border: Border { radius: 3.0.into(), ..Default::default() },
            ..Default::default()
        });
        mouse_area(r).on_press(msg).into()
    }
}

fn ctl_name(call: &Call) -> Result<String, String> {
    let name = call.str_arg("name").ok_or("needs \"name\"")?.trim_start_matches('@').to_owned();
    check_profile_name(&name).map_err(|e| format!("{e:#}"))?;
    Ok(name)
}

/// Fields of a control request onto a profile: a string sets, `null` removes, absent keeps.
/// Unknown keys are refused, so a typo can't silently do nothing.
fn apply_fields(p: &mut Profile, req: &Value) -> Result<(), String> {
    let Some(obj) = req.as_object() else { return Err("a request is an object".into()) };
    for (k, v) in obj {
        let s = || -> Result<Option<String>, String> {
            match v {
                Value::Null => Ok(None),
                Value::String(s) if s.trim().is_empty() => Ok(None),
                Value::String(s) => Ok(Some(s.trim().to_owned())),
                _ => Err(format!("\"{k}\" is a string")),
            }
        };
        match k.as_str() {
            "cmd" | "name" | "rename" | "force" | "home" | "test" => {}
            "host" | "ssh" => p.ssh = s()?,
            "wsl" => p.wsl = s()?,
            "root" => {
                let r = s()?.ok_or("\"root\" can't be removed")?;
                p.use_root(&r);
            }
            "user" => p.user = s()?,
            "identity" => p.identity = s()?,
            "jump" => p.jump = s()?,
            "nibd" => p.nibd = s()?,
            "port" => {
                p.port = match v {
                    Value::Null => None,
                    Value::Number(n) => Some(n.as_u64().and_then(|n| u16::try_from(n).ok()).ok_or("\"port\" is 1-65535")?),
                    Value::String(s) => Some(s.trim().parse().map_err(|_| "\"port\" is 1-65535")?),
                    _ => return Err("\"port\" is a number".into()),
                }
            }
            other => return Err(format!("unknown field {other:?} (host, root, user, port, identity, jump, nibd, wsl)")),
        }
    }
    p.check().map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::super::tests::app;
    use super::*;

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
    fn saved(name: &str, host: &str, roots: &[&str]) -> (String, Profile) {
        let mut p = Profile { ssh: Some(host.into()), root: roots[roots.len() - 1].into(), ..Default::default() };
        for r in roots.iter().rev() {
            p.use_root(r);
        }
        (name.into(), p)
    }

    #[test]
    fn fields_from_requests() {
        let mut p = Profile { root: "~".into(), ..Default::default() };
        apply_fields(&mut p, &json!({ "cmd": "ssh.add", "name": "x", "host": "gpu", "port": 2222, "user": "tal" })).unwrap();
        assert_eq!((p.ssh.as_deref(), p.port, p.user.as_deref()), (Some("gpu"), Some(2222), Some("tal")));
        apply_fields(&mut p, &json!({ "user": null, "port": "22" })).unwrap();
        assert_eq!((p.user.as_deref(), p.port), (None, Some(22)));
        assert!(apply_fields(&mut p, &json!({ "hots": "typo" })).is_err());
        assert!(apply_fields(&mut p, &json!({ "host": "-oProxyCommand=x" })).is_err());
        assert!(apply_fields(&mut p, &json!({ "port": 70000 })).is_err());
    }

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(short_path("~/code", 26), "~/code");
        assert_eq!(short_path("/private/tmp/claude-501/scratchpad/e2e/a/proj/src", 26), "…/e2e/a/proj/src");
    }

    #[test]
    fn hosts_typed_in_the_box() {
        let p = parse_host("tal@gpu.lab:2222").unwrap();
        assert_eq!((p.ssh.as_deref(), p.port, p.root.as_str()), (Some("tal@gpu.lab"), Some(2222), "~"));
        assert!(parse_host("-oProxyCommand=x").is_err());
        assert!(parse_host("two words").is_err());
        assert_eq!(name_for("tal@gpu.lab"), "gpu.lab");
        assert_eq!(name_for("10.0.0.7"), "10.0.0.7");
    }

    #[test]
    fn the_list_is_every_root_of_every_host_and_typing_filters_it() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleSsh));
        assert_eq!(a.focus, Focus::Ssh);
        a.ssh.profiles = vec![saved("gpu", "gpu-box", &["~/seg", "~/code"]), saved("lab", "10.0.0.7", &["~"])];
        a.ssh.aliases = vec!["gpu-box".into(), "pi".into()];
        let rows = a.ssh_rows();
        // gpu-box is saved already, so only pi is offered from ~/.ssh/config.
        assert_eq!(rows, [Row::Input, Row::Root(0, 0), Row::Root(0, 1), Row::Root(1, 0), Row::Home, Row::Alias(1)]);
        type_str(&mut a, "code");
        assert_eq!(a.ssh_rows(), [Row::Input, Row::Root(0, 0), Row::Root(0, 1)], "a host shows all its roots when one matches");
        key(&mut a, "down");
        type_str(&mut a, "x"); // typing on a row goes back to the box
        assert_eq!((a.ssh.input.as_str(), a.ssh.sel), ("codex", 0));
        key(&mut a, "esc");
        assert!(a.ssh.input.is_empty() && a.ssh.open, "esc clears first");
        key(&mut a, "esc");
        assert!(!a.ssh.open && a.focus == Focus::Main);
    }

    #[test]
    fn a_bad_host_is_explained_not_connected() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleSsh));
        a.ssh.profiles.clear();
        type_str(&mut a, "two words");
        key(&mut a, "enter");
        assert!(a.ssh.error.is_some() && a.connecting.is_none());
    }

    #[test]
    fn the_browser_hides_dot_folders_until_asked_and_knows_the_parent() {
        let mut b = Browse { dir: "/home/tal".into(), ..Default::default() };
        b.entries = vec![(".cache".into(), false), ("code".into(), false), ("Codex".into(), true)];
        assert_eq!(b.shown().len(), 2);
        b.filter = "cod".into();
        assert_eq!(b.shown().iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), ["code", "Codex"]);
        b.filter = ".c".into();
        assert_eq!(b.shown().len(), 1);
        assert_eq!((b.path_of("code").as_str(), b.parent().as_deref()), ("/home/tal/code", Some("/home")));
        b.dir = "/home".into();
        assert_eq!(b.parent().as_deref(), Some("/"));
        b.dir = "/".into();
        assert_eq!(b.parent(), None);
    }

    #[test]
    fn editing_shows_the_changes_and_saves_only_after_yes() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleSsh));
        a.ssh.profiles = vec![saved("gpu", "gpu-box", &["~/seg"])];
        a.ssh.aliases.clear();
        a.ssh.sel = 1; // gpu · ~/seg
        type_str(&mut a, "e");
        assert!(a.ssh.edit.is_some(), "e on a connection edits it");
        assert_eq!(a.ssh.input, "", "and doesn't type");
        type_str(&mut a, "2"); // host: gpu-box2
        key(&mut a, "down");
        type_str(&mut a, "tal"); // user
        let _ = a.ssh_edit_confirm();
        let changes = a.ssh.edit.as_ref().unwrap().confirm.clone().unwrap();
        assert_eq!(changes, ["host: gpu-box → gpu-box2", "user: (not set) → tal"]);
        key(&mut a, "n");
        key(&mut a, "down");
        assert_eq!(a.ssh.edit.as_ref().unwrap().sel, 2, "user is the last field shown");
        let _ = a.ssh_edit_confirm();
        key(&mut a, "n"); // back to the form, nothing written
        assert!(a.ssh.edit.as_ref().unwrap().confirm.is_none());
        key(&mut a, "esc");
        assert!(a.ssh.edit.is_none());
    }

    #[test]
    fn askpass_needs_the_attempts_token() {
        let mut a = app();
        let (call, rx) = Call::test(json!({ "cmd": "askpass", "token": "guess", "prompt": "Password:" }));
        let _ = a.on_askpass(call);
        assert_eq!(rx.recv().unwrap()["ok"], false, "no attempt in progress, no prompt");
        assert!(a.ssh.ask.is_none());
    }
}
