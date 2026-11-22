mod ai;
mod code;
mod comment;
mod connection;
mod control;
mod diff;
mod drop;
mod forward;
mod git;
mod pdf_tab;
mod peek;
mod plan;
mod search;
mod apps;
mod serve;
mod settings;
mod ssh;
mod titlebar;
mod tree;
mod viewers;
mod web;
mod watch;

pub use connection::ConnEvent;
pub use web::Msg as WebMsg;

use crate::editor_view::{Typeface, ViewEvent};
use crate::icons::{hex, Icons};
use crate::media::{Decoded, Kind, Media};
use crate::table::Table;
use crate::table_view::TableEvent;
use crate::terminal_view::{key_bytes, TermEvent, TerminalView};
use crate::theme::Theme;
use crate::transport::Conn;
use crate::zoom;
use iced::keyboard::{self, key::Named, Key as IKey};
use iced::widget::{button, column, container, mouse_area, progress_bar, row, space, svg, text, tooltip};
use iced::{Border, Color, Element, Length, Subscription, Task};
use nib_editor::keymap::{Key, KeyChord};
use nib_editor::config::ThemeConfig;
use nib_editor::{Command, Document, Keymap};
use nib_proto::{
    ClientNotify, ErrorKind, LspOp, Pos, PtyId, Request, ReqId, Response, ServerMsg,
    ServerNotify,
};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Message {
    /// Files dragged onto the window (`drop.rs`).
    Drop(drop::Msg),
    /// From the current connection, as tests send it (real traffic comes as `Conn`).
    #[cfg(test)]
    Server(ServerMsg),
    /// From connection attempt `gen` (`connection.rs`); stale ones are dropped.
    Conn(u64, ConnEvent),
    SyntaxReady,
    Key { chord: KeyChord, text: Option<String>, repeat: bool },
    KeyUp(Key),
    /// Window lost focus: a held key's release won't arrive.
    Blur,
    View(ViewEvent),
    Term(u64, TermEvent),
    Pasted(Option<String>),
    PastedTerm(Option<String>),
    Tree(tree::Msg),
    /// Shift / Cmd / Ctrl went down or up (clicks in the file tree read them).
    Mods(keyboard::Modifiers),
    SelectTab(usize),
    CloseTabAt(usize),
    Control(Command),
    /// Cmd / Ctrl / Shift + wheel over a zone (`None`: anywhere else), in steps (+: bigger).
    WheelZoom(Option<zoom::Zone>, i32),
    FlashExpired(u64),
    Frame,
    Settings(settings::Msg),
    Table(TableEvent),
    MediaDecoded(u64, Result<Arc<Decoded>, String>),
    /// The renderer holds frame `.1` of the animation in tab `.0` (see `Media::tick`).
    MediaPinned(u64, usize, Result<iced_runtime::image::Allocation, iced_runtime::image::Error>),
    /// The image was clicked: play / pause an animation.
    MediaClick,
    Pdf(crate::pdf::Msg),
    PreviewLink(String),
    /// A block of the Markdown / HTML preview was clicked: the char offset it comes from.
    PreviewClick(usize),
    PreviewScrolled(iced::widget::scrollable::Viewport),
    /// Redraw: an animation is playing.
    Tick,
    /// Time to ask the daemon about git again (for `App::epoch`).
    GitPoll(u64),
    Git(git::Msg),
    /// The diff tab (`diff.rs`).
    Diff(diff::Msg),
    /// The web tabs (`web.rs`).
    Web(web::Msg),
    Ai(ai::Msg),
    Ssh(ssh::Msg),
    /// The Apps panel (`apps.rs`).
    Apps(apps::Msg),
    /// A result in the definitions / references list was clicked.
    PickerOpen(usize),
    /// The mouse rested long enough: show, replace or close the hover popup (if `seq` is current).
    HoverDue(u64),
    /// The mouse went onto (`true`) or off the hover popup.
    HoverPopup(bool),
    /// Find (`search.rs`).
    Search(search::Msg),
    /// A request on the control socket (`crate::control`).
    Ctl(crate::control::Call),
    /// The window screenshot a control `snapshot` asked for (and where to save it).
    ControlShot(crate::control::Call, Option<PathBuf>, iced::window::Screenshot),
    /// nib's own title bar and window edges (`titlebar.rs`).
    TitleBar(titlebar::Msg),
}

enum Pending {
    Hello { open: Option<String> },
    /// `goto`: put the cursor there once it's open (a jump to a definition / reference).
    Open { path: String, new_tab: bool, goto: Option<Pos> },
    /// The file tree's snapshot.
    Tree,
    /// The file tree staged or unstaged something.
    TreeGit,
    /// The file tree created, renamed or deleted something. `reveal`: put the cursor there;
    /// `open`: and open it in a tab.
    TreeFs { reveal: Option<String>, renamed: Option<(String, String)>, open: bool },
    Save { tab: u64, revision: usize },
    PtyOpen { term: u64 },
    /// Something asked of the project's apps; `call`: by a control client, who gets the answer.
    /// `quiet`: asked by the window itself at connect; a refusal is not worth a message.
    Serve { call: Option<crate::control::Call>, quiet: bool },
    /// `poll`: the answer schedules the next ask.
    Git { poll: bool },
    GitDetails,
    GitRun { action: git::Action },
    /// The diff tab's diff of `scope`, asked while `App::epoch` was `epoch`.
    Diff { scope: nib_proto::DiffScope, epoch: u64 },
    /// The diff tab staged or unstaged a file.
    DiffGit,
    /// The review of agent `id`'s work, read or just changed.
    DiffReview { id: nib_proto::AiId },
    /// Landing agent `id`'s branch on its base.
    WorkLand { id: nib_proto::AiId },
    /// The plan of agent `id` (as of `AiWork::plan` = `stamp`), for the diff tab.
    DiffPlan { id: nib_proto::AiId, stamp: u64 },
    /// Re-read of an open file that changed on disk (git, an agent, anything outside nib).
    Reload { tab: u64 },
    /// Re-read of an image tab's file that changed on disk.
    ReloadMedia { tab: u64 },
    /// An image referenced by a Markdown / HTML preview.
    PreviewImage { tab: u64, url: String },
    /// An image an agent's reply shows, for the chat with this key.
    AiImage { chat: u64, url: String },
    /// A language server query about tab `tab` at its `Document::version` `version`.
    Lsp { tab: u64, version: u64, op: LspOp },
    ApplyEdits,
    /// For the chat with this key.
    AiOpen { chat: u64, generation: u64 },
    /// `then`: the assistant just set up in a terminal, to chat with if it's ready now.
    AiAgents { then: Option<nib_proto::AiAgent> },
    /// The list of past conversations.
    AiHistory,
    /// Every mailbox message the project kept.
    AiMailHistory,
    /// One of them, to reopen (`replay`: and walk through it).
    AiHistoryLoad { replay: bool },
    AiHistoryDelete,
    /// The skills the `/` menu offers a chat with `agent`.
    AiSkills { agent: nib_proto::AiAgent },
    /// Loading / unloading a local model.
    AiWarm { model: String, warm: bool },
    /// Downloading an Ollama model.
    AiPull { model: String },
    /// Deleting an Ollama model.
    AiDelete { model: String },
    /// A folder listing for the SSH panel's folder browser.
    Browse,
    /// Find across the project, for query number `seq`.
    Search { seq: u64 },
}

pub struct Tab {
    id: u64,
    doc: Document,
    top: usize,
    left: usize,
    /// A file on this machine (config.toml), read and written without nibd.
    local: bool,
    kind: Kind,
    /// Showing the rendered view: the table for CSV, the side-by-side preview for Markdown / HTML
    /// / SVG.
    rendered: bool,
    /// Set for files that are shown, not edited (images).
    media: Option<Media>,
    table: Option<Table>,
    /// `Document::version` that `table` was parsed from.
    table_version: u64,
    preview: Option<viewers::Preview>,
    svg: Option<viewers::SvgPreview>,
    /// `Document::version` last sent to the language server (`DocSync`).
    lsp_synced: Option<u64>,
    /// The diff tab (`diff.rs`): its document is an empty placeholder.
    diff: bool,
    /// A web tab (`web.rs`), likewise: the page it shows.
    web: Option<u32>,
}

struct Term {
    /// Local, stable id (the PTY id only exists once the daemon answers).
    key: u64,
    id: Option<PtyId>,
    parser: vt100::Parser,
    size: (u16, u16),
    /// Input typed before the PTY exists.
    queued: Vec<u8>,
    /// The project's server that runs here (`serve.rs`): headed with its name and state, kept
    /// when the server ends by itself, and closing it stops the server.
    server: Option<String>,
}

/// Where keyboard input goes. Terminals are referenced by `Term::key`.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Focus {
    /// The file tree, left of everything: first in the ring.
    Tree,
    /// The tab bar above the main view: ←/→ show the next tab, Enter / ↓ / Esc go into it.
    /// In the ring only when there are tabs to choose between.
    Tabs,
    Main,
    Terminal(u64),
    /// The bottom control strip: ←/→ pick a button, Enter presses it, Esc goes back.
    Toolbar,
    /// The Git panel: ←/→ pick an operation, Enter runs it, Esc closes the panel.
    Git,
    /// The AI panel's message box.
    Ai,
    /// The SSH panel: connections, the new-connection form, ssh's questions.
    Ssh,
    /// The Apps panel: the project's apps, the add / edit form, an approval.
    Apps,
}

/// The terminal key (Ctrl+`, with or without Shift) is held: H and B can join it (see below).
/// Released without any gesture, it runs its own command (`tap`, a new terminal). Moving between
/// panes is not a gesture: see `Command::NextPane` and `App::switch_tab_or_pane`. Closing a terminal is the Close command (`App::close_focused`).
#[derive(Debug)]
struct Leader {
    key: Key,
    tap: Command,
    used: bool,
}

/// Ctrl + ` + H: hide / show the terminals.
const HIDE_TERMS_KEY: Key = Key::Char('h');
/// Ctrl + ` + B: jump to the bottom toolbar (and back). Works from a terminal too, where a plain
/// Ctrl+B (`Command::FocusToolbar` on macOS) belongs to the shell.
const TOOLBAR_KEY: Key = Key::Char('b');

/// What a terminal asks of its emulator in `data` and vt100 doesn't answer: the cursor position
/// (`ESC[6n` → `ESC[row;colR`, 1-based) and device status (`ESC[5n` → `ESC[0n`). Windows' ConPTY
/// asks for the cursor when a shell starts and shows nothing until it hears back.
fn status_replies(data: &[u8], screen: &vt100::Screen) -> Vec<u8> {
    let mut out = Vec::new();
    for w in data.windows(4) {
        match w {
            b"\x1b[6n" => {
                let (row, col) = screen.cursor_position();
                out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            b"\x1b[5n" => out.extend_from_slice(b"\x1b[0n"),
            _ => {}
        }
    }
    out
}

/// Commands that work wherever keyboard focus is (terminal, explorer, toolbar).
/// One line of a text box that doesn't wrap: when it's wider than the box, the box shows its end
/// (where the caret is) instead of clipping it off.
fn tail_line<'a>(line: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    use iced::widget::scrollable;
    scrollable(line).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::hidden())).anchor_right().width(Length::Fill).into()
}

fn app_wide(c: Command) -> bool {
    use Command::*;
    tab_number(c).is_some()
        || matches!(
            c,
            Quit | ToggleExplorer
                | Find
                | ToggleTerminal
                | ToggleGit
                | ToggleDiff
                | ToggleSsh
                | ToggleApps
                | OpenFolder
                | ToggleAi
                | NextAgent
                | PrevAgent
                | NewAgent
                | KillAgent
                | FoldAgents
                | AgentHistory
                | Panic
                | FocusToolbar
                | NextPane
                | PrevPane
                | NewTerminal
                | NextTab
                | PrevTab
                | CloseTab
                | ReopenClosedTab
                | FontBigger
                | FontSmaller
                | OpenSettings
                | KeyboardShortcuts
                | ToggleBeginnerMode
                | ToggleFullscreen
                | TogglePreview
        )
}

/// GoToTabN -> N-1.
fn tab_number(c: Command) -> Option<usize> {
    use Command::*;
    [GoToTab1, GoToTab2, GoToTab3, GoToTab4, GoToTab5, GoToTab6, GoToTab7, GoToTab8, GoToTab9]
        .iter()
        .position(|t| *t == c)
}

pub struct Bench {
    frames: u32,
    started: Option<Instant>,
}

pub struct App {
    /// None until the first connection is made.
    conn: Option<Conn>,
    /// Number of the connection in `conn`; messages from other connections are dropped.
    gen: u64,
    /// Bumped on every switch and root change: a git poll from before is dropped.
    epoch: u64,
    /// The home folder on the connected machine, when known (to show and save roots as `~/…`).
    remote_home: Option<String>,
    /// Last number handed out.
    next_gen: u64,
    /// Where the window is connected (or, before that, being connected).
    launch: crate::Launch,
    /// Where the window started: offered in the SSH panel to go back to.
    home: crate::Launch,
    connecting: Option<connection::Connecting>,
    /// The last failed attempt: (label, why).
    conn_error: Option<(String, String)>,
    /// `ssh.connect` waiting for the new daemon's `Hello`.
    connect_call: Option<crate::control::Call>,
    lsp: Vec<nib_proto::LspServer>,
    ssh: ssh::Panel,
    apps: apps::Panel,
    /// A control client's keys and commands are being handled (not the user's own): what only
    /// the user may do (approve an app) is refused meanwhile.
    ctl_input: bool,
    /// Files dropped on the window, waiting to be told where.
    drops: drop::Drops,
    pending: HashMap<ReqId, Pending>,
    /// Requests in flight: when each was sent and what it was (timed into `nib.log`).
    sent: HashMap<ReqId, (Instant, String)>,
    keymap: Keymap,
    theme: Theme,
    icons: Icons,
    tab_width: usize,
    /// Code font (editor, terminal, file browser).
    face: Typeface,
    /// Scale of the rendered views (preview, CSV table, SVG), on top of `face`; theirs alone.
    preview_zoom: f32,
    /// `[editor] font`; None = system monospace.
    font_name: Option<String>,
    /// The `[theme]` overrides `theme` was built from (edited live in Settings).
    theme_cfg: ThemeConfig,
    /// Beginner mode: shortcut cheat sheet at the far right.
    beginner: bool,
    settings: settings::State,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,
    tree: tree::Tree,
    mods: keyboard::Modifiers,
    terms: Vec<Term>,
    terms_visible: bool,
    next_term_key: u64,
    /// Terminal to return to when Ctrl+` leaves the editor.
    last_term: Option<u64>,
    leader: Option<Leader>,
    /// Non-modifier keys currently down (for chords like Ctrl+Q+`).
    held: Vec<Key>,
    /// Selected toolbar button, and where Esc / pressing a button returns focus to.
    toolbar_sel: usize,
    toolbar_return: Focus,
    /// Paths of closed tabs, most recent last (for reopen).
    closed: Vec<String>,
    /// `NIB_KEYLOG=1`: print key events and gesture decisions to stderr.
    keylog: bool,
    ai: ai::Panel,
    focus: Focus,
    /// The tab the preview pane last showed (its scroll position belongs to that tab).
    preview_tab: Option<u64>,
    rows: usize,
    cols: usize,
    root: String,
    label: String,
    /// What the daemon's git says about the project root (None until it answers).
    git: Option<nib_proto::GitInfo>,
    git_ui: git::Panel,
    diff: diff::View,
    web: web::Tabs,
    serve: serve::State,
    flash: Option<(String, bool, u64)>,
    flash_seq: u64,
    /// Recent flash messages, newest last (the control socket's `state` shows them).
    messages: VecDeque<(String, bool)>,
    quit_armed: bool,
    close_armed: bool,
    /// nib's own title bar, on WSL (`titlebar.rs`).
    titlebar: titlebar::State,
    force_save_armed: bool,
    disconnected: bool,
    connected: bool,
    /// What nibd was last told to watch (`ClientNotify::Watch`), on connection `gen`.
    watched: Option<(u64, Vec<String>)>,
    bench: Option<Bench>,
    headless: bool,
    code: code::State,
    search: search::State,
}

pub struct Init {
    /// Already connected (tests); normally None and the app connects to `launch` itself.
    pub conn: Option<Conn>,
    pub keymap: Keymap,
    pub theme: Theme,
    pub warnings: Vec<String>,
    pub tab_width: usize,
    pub font_size: f32,
    pub font: Option<String>,
    pub line_height: f32,
    pub beginner_mode: bool,
    pub theme_cfg: ThemeConfig,
    pub launch: crate::Launch,
    pub bench: bool,
    /// No window (`nib --headless`): driven only through the control socket.
    pub headless: bool,
    /// `[lsp.<name>]`
    pub lsp: Vec<nib_proto::LspServer>,
    /// `[ai]`
    pub ai: nib_editor::config::AiConfig,
}

const MOD: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };
/// Height of the macOS title bar the window content runs under (see main.rs); 0 elsewhere.
pub const TITLEBAR_H: f32 = if cfg!(target_os = "macos") { 28.0 } else { 0.0 };
pub use titlebar::own_frame;

impl App {
    pub fn boot(init: Init) -> (Self, Task<Message>) {
        let Init {
            conn,
            keymap,
            theme,
            warnings,
            tab_width,
            font_size,
            font,
            line_height,
            beginner_mode,
            theme_cfg,
            launch,
            bench,
            headless,
            lsp,
            ai,
        } = init;
        let pending = HashMap::new();
        let connected_already = conn.is_some();
        let icons = Icons::new(&hex(theme.dim), &hex(theme.accent));
        let mut app = Self {
            conn,
            gen: 0,
            epoch: 0,
            remote_home: None,
            next_gen: 0,
            launch: launch.clone(),
            home: launch.clone(),
            connecting: None,
            conn_error: None,
            connect_call: None,
            lsp,
            ssh: Default::default(),
            apps: Default::default(),
            ctl_input: false,
            drops: Default::default(),
            pending,
            sent: HashMap::new(),
            keymap,
            theme,
            icons,
            tab_width,
            face: Typeface { font: settings::font_for(font.as_deref()), size: font_size, line_height },
            preview_zoom: 1.0,
            font_name: font,
            theme_cfg,
            beginner: beginner_mode,
            settings: Default::default(),
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            tree: Default::default(),
            mods: Default::default(),
            terms: Vec::new(),
            serve: Default::default(),
            terms_visible: false,
            next_term_key: 1,
            last_term: None,
            leader: None,
            held: Vec::new(),
            toolbar_sel: 0,
            toolbar_return: Focus::Main,
            closed: Vec::new(),
            keylog: std::env::var_os("NIB_KEYLOG").is_some(),
            ai: ai::Panel::new(ai),
            focus: Focus::Main,
            preview_tab: None,
            rows: 40,
            cols: 120,
            root: launch.root.clone(),
            label: launch.label.clone(),
            git: None,
            git_ui: Default::default(),
            diff: Default::default(),
            web: Default::default(),
            flash: None,
            flash_seq: 0,
            messages: VecDeque::new(),
            quit_armed: false,
            close_armed: false,
            titlebar: Default::default(),
            force_save_armed: false,
            disconnected: false,
            connected: false,
            watched: None,
            bench: bench.then_some(Bench { frames: 0, started: None }),
            headless,
            code: Default::default(),
            search: Default::default(),
        };
        let mut t = Task::none();
        for w in warnings {
            t = app.flash_warn(w);
        }
        let connecting = if connected_already {
            app.hello(&launch)
        } else {
            app.connect(launch, false, false, None, false)
        };
        (app, Task::batch([t, connecting]))
    }

    fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }
    fn tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }
    fn doc(&self) -> Option<&Document> {
        self.tab().map(|t| &t.doc)
    }
    /// Keys edit the active tab's text.
    fn editing(&self) -> bool {
        self.tab().is_some_and(|t| !t.passive())
    }

    pub fn title(&self) -> String {
        let name = self
            .doc()
            .and_then(|d| d.path.as_ref())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "nibble".into());
        let dirty = if self.doc().is_some_and(|d| d.is_modified()) { "● " } else { "" };
        let remote = if self.label != "local" { format!("  ·  {}", self.label) } else { String::new() };
        format!("{dirty}{name}{remote}")
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let keys = keyboard::listen().filter_map(|ev| match ev {
            keyboard::Event::KeyPressed { key, physical_key, modifiers, text, repeat, .. } => {
                let chord = to_chord(&key, physical_key, modifiers)?;
                Some(Message::Key { chord, text: text.map(|t| t.to_string()), repeat })
            }
            keyboard::Event::KeyReleased { key, physical_key, modifiers, .. } => {
                Some(Message::KeyUp(to_chord(&key, physical_key, modifiers)?.key))
            }
            keyboard::Event::ModifiersChanged(m) => Some(Message::Mods(m)),
        });
        let blur = iced::event::listen_with(|ev, _, _| match ev {
            iced::Event::Window(iced::window::Event::Unfocused) => Some(Message::Blur),
            // The window moved to a screen of another density: web tabs are drawn at it.
            iced::Event::Window(iced::window::Event::Rescaled(s)) => Some(Message::Web(web::Msg::Scale(s))),
            iced::Event::Window(iced::window::Event::FileHovered(_)) => Some(Message::Drop(drop::Msg::Hovered)),
            iced::Event::Window(iced::window::Event::FilesHoveredLeft) => Some(Message::Drop(drop::Msg::Left)),
            iced::Event::Window(iced::window::Event::FileDropped(p)) => Some(Message::Drop(drop::Msg::File(p))),
            _ => None,
        });
        let resized = iced::window::resize_events().map(|(_, size)| Message::Settings(settings::Msg::Resized(size)));
        let mut subs = vec![keys, blur, resized];
        if titlebar::own_frame() {
            subs.push(iced::window::resize_events().map(|(id, _)| Message::TitleBar(titlebar::Msg::Resized(id))));
        }
        if self.bench.is_some() {
            subs.push(iced::window::frames().map(|_| Message::Frame));
        }
        if self.animating() || self.ai_follow_playing() {
            subs.push(iced::window::frames().map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn flash(&mut self, s: impl Into<String>, warn: bool) -> Task<Message> {
        self.flash_seq += 1;
        let seq = self.flash_seq;
        let s = s.into();
        if self.messages.len() == control::MESSAGE_LOG {
            self.messages.pop_front();
        }
        self.messages.push_back((s.clone(), warn));
        crate::control::log(if warn { "warn" } else { "info" }, &s);
        self.flash = Some((s, warn, seq));
        Task::perform(async_sleep(Duration::from_secs(4)), move |_| Message::FlashExpired(seq))
    }
    fn flash_info(&mut self, s: impl Into<String>) -> Task<Message> {
        self.flash(s, false)
    }
    fn flash_warn(&mut self, s: impl Into<String>) -> Task<Message> {
        self.flash(s, true)
    }

    fn request(&mut self, req: Request, p: Pending) -> Task<Message> {
        let Some(conn) = self.conn.as_mut().filter(|_| !self.disconnected) else {
            return self.flash_warn("not connected");
        };
        let (t0, name) = (Instant::now(), nib_proto::timing::label(&req));
        match conn.request(req) {
            Ok(id) => {
                self.sent.insert(id, (t0, name));
                self.pending.insert(id, p);
                Task::none()
            }
            Err(e) => {
                self.disconnected = true;
                self.flash_warn(format!("send failed: {e}"))
            }
        }
    }

    fn notify(&mut self, n: ClientNotify) {
        let Some(conn) = self.conn.as_mut().filter(|_| !self.disconnected) else { return };
        if conn.notify(n).is_err() {
            self.disconnected = true;
        }
    }

    pub fn update(&mut self, msg: Message) -> Task<Message> {
        let t0 = Instant::now();
        // Daemon traffic is named by what came, a response by the request it answers.
        let name = match &msg {
            Message::Conn(_, ConnEvent::Server(m)) => match m {
                ServerMsg::Response { id, .. } => {
                    format!("Server/Response/{}", self.sent.get(id).map_or("?", |(_, req)| req.as_str()))
                }
                ServerMsg::Notify(n) => format!("Server/Notify/{}", nib_proto::timing::label(n)),
            },
            Message::Conn(_, e) => format!("Conn/{}", nib_proto::timing::label(e)),
            // `nib ctl run save` is `Ctl/run/save`, `nib ctl open …` is `Ctl/open`.
            Message::Ctl(c) => match c.str_arg("command") {
                Some(name) => format!("Ctl/{}/{name}", c.cmd()),
                None => format!("Ctl/{}", c.cmd()),
            },
            _ => nib_proto::timing::label(&msg),
        };
        let task = self.handle(msg);
        let web = self.web_follow();
        let task = Task::batch([task, web]);
        self.ai_sync();
        self.terms_sync();
        self.sync_watch();
        let derived = self.refresh_derived();
        let task = Task::batch([task, derived, self.sync_preview(), self.pdf_tick()]);
        crate::timing::report("update", &name, t0.elapsed(), nib_proto::timing::SLOW_UI);
        task
    }

    /// Keep the agents tab and the file tabs from both being in front: a file tab that was
    /// opened or picked hides the agents tab, and the keys follow whichever shows.
    fn ai_sync(&mut self) {
        let seen = (self.active, self.tabs.len(), self.tab().map(|t| t.id));
        if self.ai.front && seen != self.ai_tabs_seen() {
            self.ai.front = false;
        }
        self.ai_set_tabs_seen(seen);
        match (self.ai.shown(), self.focus) {
            (true, Focus::Main) => self.focus = Focus::Ai,
            (false, Focus::Ai) => self.focus = Focus::Main,
            _ => {}
        }
        // One tab left is nothing to choose between.
        if self.focus == Focus::Tabs && self.tab_slots() < 2 {
            self.focus = self.content_focus();
        }
    }

    fn handle(&mut self, msg: Message) -> Task<Message> {
        match msg {
            #[cfg(test)]
            Message::Server(m) => self.on_server(m),
            Message::Conn(gen, ev) => self.on_conn(gen, ev),
            Message::SyntaxReady => {
                for t in &mut self.tabs {
                    if !t.doc.read_only {
                        t.doc.attach_syntax();
                    }
                }
                // Runtime grammars that didn't load (`[languages]` in config.toml).
                let mut task = Task::none();
                for e in nib_syntax::registry().errors() {
                    task = self.flash_warn(e.clone());
                }
                task
            }
            Message::FlashExpired(seq) => {
                if self.flash.as_ref().is_some_and(|f| f.2 == seq) {
                    self.flash = None;
                }
                Task::none()
            }
            Message::Key { chord, text, repeat } => {
                self.hover_reset();
                // Typing in the editor while an agent is followed: the editor is the user's.
                let typed = self.focus == Focus::Main && text.is_some() && !chord.ctrl && !chord.logo;
                let (stop, swallow) = if typed { self.ai_follow_interrupted() } else { (Task::none(), false) };
                // The tab still shows the followed agent's edit being typed: the key isn't typed.
                if swallow {
                    return stop;
                }
                Task::batch([stop, self.on_key(chord, text, repeat)])
            }
            Message::KeyUp(key) => self.on_key_up(key),
            Message::Blur => {
                self.leader = None;
                self.held.clear();
                Task::none()
            }
            Message::View(ev @ (ViewEvent::Hover { .. } | ViewEvent::HoverEnd)) => self.on_hover(ev),
            Message::View(ev) => {
                if !matches!(ev, ViewEvent::Resized { .. }) {
                    self.hover_reset();
                }
                if matches!(ev, ViewEvent::Click { .. } | ViewEvent::Drag { .. }) {
                    self.focus = Focus::Main;
                    self.code.picker = None;
                    self.code.rename = None;
                    self.search.open = false;
                }
                self.on_view(ev);
                if matches!(ev, ViewEvent::Click { jump: true, extend: false, .. }) {
                    return self.exec(Command::GoToDefinition);
                }
                if matches!(ev, ViewEvent::Click { extend: false, .. }) {
                    return self.reveal_scope_at_cursor();
                }
                Task::none()
            }
            Message::Term(key, ev) => self.on_term(key, ev),
            Message::Pasted(s) => {
                if let (Some(s), true) = (s, self.editing()) {
                    if let Some(t) = self.tab_mut() {
                        t.doc.paste(&s);
                    }
                    self.follow_cursor();
                }
                Task::none()
            }
            Message::PastedTerm(s) => {
                if let Some(s) = s {
                    let bracketed = self.focused_term().is_some_and(|t| t.parser.screen().bracketed_paste());
                    let data = if bracketed { format!("\x1b[200~{s}\x1b[201~") } else { s };
                    self.term_input(data.into_bytes());
                }
                Task::none()
            }
            Message::Tree(m) => self.on_tree_msg(m),
            Message::Mods(m) => {
                self.mods = m;
                Task::none()
            }
            Message::SelectTab(i) => {
                if i < self.tabs.len() {
                    self.active = i;
                    self.ai.front = false;
                    self.focus = Focus::Main;
                }
                Task::none()
            }
            Message::CloseTabAt(i) => {
                self.active = i.min(self.tabs.len().saturating_sub(1));
                self.close_tab()
            }
            Message::Control(Command::ToggleTerminal) => self.toggle_terminal(true),
            Message::Control(c) => self.exec(c),
            Message::WheelZoom(zone, steps) => self.wheel_zoom(zone, steps),
            Message::Frame => self.bench_frame(),
            Message::Settings(m) => self.on_settings(m),
            Message::Table(ev) => self.on_table(ev),
            Message::MediaDecoded(id, r) => self.on_media_decoded(id, r),
            Message::MediaPinned(id, i, r) => {
                if let Some(m) = self.tabs.iter_mut().find(|t| t.id == id).and_then(|t| t.media.as_mut()) {
                    m.pinned(i, r);
                }
                Task::none()
            }
            Message::Pdf(m) => self.on_pdf(m),
            Message::MediaClick => {
                self.focus = Focus::Main;
                if let Some(m) = self.tab_mut().and_then(|t| t.media.as_mut()) {
                    m.toggle_play();
                }
                Task::none()
            }
            Message::PreviewLink(url) => self.on_preview_link(url),
            Message::PreviewClick(pos) => {
                self.on_preview_click(pos);
                Task::none()
            }
            Message::PreviewScrolled(v) => {
                self.on_preview_scrolled(v);
                Task::none()
            }
            Message::Tick => Task::batch([self.media_tick(), self.ai_follow_tick()]),
            Message::GitPoll(epoch) if epoch == self.epoch => self.git_refresh(),
            // A poll from before a switch or root change: the new Hello starts its own.
            Message::GitPoll(_) => Task::none(),
            Message::Git(m) => self.on_git_msg(m),
            Message::Diff(m) => self.on_diff_msg(m),
            Message::Web(m) => self.on_web_msg(m),
            Message::Ai(m) => self.on_ai_msg(m),
            Message::Drop(m) => self.on_drop(m),
            Message::TitleBar(m) => self.on_titlebar(m),
            Message::Ssh(m) => self.on_ssh_msg(m),
            Message::Apps(m) => self.on_apps_msg(m),
            Message::PickerOpen(i) => self.picker_open(i),
            Message::HoverDue(seq) => self.hover_due(seq),
            Message::HoverPopup(over) => self.hover_popup(over),
            Message::Search(m) => self.on_search_msg(m),
            Message::Ctl(call) => self.on_control(call),
            Message::ControlShot(call, path, shot) => {
                Self::control_shot(call, path, shot);
                Task::none()
            }
        }
    }

    fn bench_frame(&mut self) -> Task<Message> {
        const FRAMES: u32 = 300;
        let rows = self.rows.max(1);
        let (cols, rows_) = (self.cols, self.rows);
        let Some(b) = &mut self.bench else { return Task::none() };
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        if b.started.is_none() {
            b.started = Some(Instant::now());
            crate::timing::reset_draw_stats();
        }
        b.frames += 1;
        let max_top = tab.doc.text.len_lines().saturating_sub(rows);
        tab.top = if tab.top >= max_top { 0 } else { tab.top + 1 };
        if b.frames >= FRAMES {
            let el = b.started.unwrap().elapsed();
            let (n, mean) = crate::timing::draw_stats();
            println!(
                "nib bench: {FRAMES} scroll frames in {:.0} ms = {:.1} fps; editor primitives {:.2} ms/frame over {n} draws; window {cols}x{rows_} cells",
                el.as_secs_f64() * 1e3,
                FRAMES as f64 / el.as_secs_f64(),
                mean.as_secs_f64() * 1e3,
            );
            return iced::exit();
        }
        Task::none()
    }

    // -----------------------------------------------------------------------------------------
    // server
    // -----------------------------------------------------------------------------------------

    fn on_server(&mut self, m: ServerMsg) -> Task<Message> {
        let (id, result) = match m {
            ServerMsg::Response { id, result } => (id, result),
            ServerMsg::Notify(ServerNotify::Log { msg, .. }) => return self.flash_info(msg),
            ServerMsg::Notify(ServerNotify::Pong { seq }) => {
                if let Some(d) = crate::transport::pong(seq) {
                    crate::timing::report("ping", "nibd", d, nib_proto::timing::SLOW_RPC);
                }
                return Task::none();
            }
            ServerMsg::Notify(ServerNotify::PtyOutput { id, data }) => {
                let mut reply = Vec::new();
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == Some(id)) {
                    t.parser.process(&data);
                    reply = status_replies(&data, t.parser.screen());
                }
                if !reply.is_empty() {
                    self.notify(ClientNotify::PtyInput { id, data: reply });
                }
                return Task::none();
            }
            ServerMsg::Notify(ServerNotify::AiEvent { id, event }) => return self.on_ai_event(id, event),
            ServerMsg::Notify(ServerNotify::FileChanged { path, mtime_ns }) => return self.on_file_changed(path, mtime_ns),
            // A model download's progress; batches are `nib ai`'s, the window never starts one.
            ServerMsg::Notify(ServerNotify::AiProgress { req, line }) => {
                if matches!(self.pending.get(&req), Some(Pending::AiPull { .. })) {
                    self.on_ai_pull_progress(&line);
                }
                return Task::none();
            }
            ServerMsg::Notify(ServerNotify::App(info)) => return self.on_app_info(info),
            ServerMsg::Notify(ServerNotify::Web { id, req }) => return self.on_agent_web(id, &req),
            ServerMsg::Notify(ServerNotify::PtyExit { id, .. }) => {
                // A server's terminal stays, for what it printed (`serve.rs`).
                if let Some(t) = self.terms.iter_mut().find(|t| t.id == Some(id) && t.server.is_some()) {
                    t.id = None;
                    return Task::none();
                }
                if let Some(key) = self.terms.iter().find(|t| t.id == Some(id)).map(|t| t.key) {
                    self.remove_term(key);
                    return self.ai_term_closed(key);
                }
                return Task::none();
            }
        };
        if let Some((t0, name)) = self.sent.remove(&id) {
            crate::timing::report("rpc", &name, t0.elapsed(), nib_proto::timing::SLOW_RPC);
        }
        let Some(p) = self.pending.remove(&id) else { return Task::none() };
        match (p, result) {
            (Pending::Hello { open }, Ok(Response::Hello { root, scratch, nibdir, lsp_extensions, .. })) => {
                self.root = root;
                self.code.lsp_extensions = lsp_extensions.into_iter().collect();
                self.tree.set_places(scratch, nibdir);
                self.connected = true;
                self.ai_tell_wpf();
                // The tree is there from the start; with no file to edit, the keyboard is in it.
                self.tree.open = true;
                let first = match open {
                    Some(path) => self.open_path(path, true),
                    None => {
                        self.focus = Focus::Tree;
                        Task::none()
                    }
                };
                Task::batch([first, self.git_refresh()])
            }
            (Pending::Git { poll }, result) => self.on_git(poll, result),
            (Pending::AiOpen { chat, generation }, result) => self.on_ai_opened(chat, generation, result),
            (Pending::AiAgents { then }, result) => self.on_ai_agents(then, result),
            (Pending::AiHistory, result) => self.on_ai_history(result),
            (Pending::AiMailHistory, result) => self.on_ai_mail_history(result),
            (Pending::AiHistoryLoad { replay }, result) => self.on_ai_past(replay, result),
            (Pending::AiHistoryDelete, result) => self.on_ai_history_deleted(result),
            (Pending::AiSkills { agent }, result) => {
                self.on_ai_skills(agent, result);
                Task::none()
            }
            (Pending::AiWarm { model, warm }, result) => self.on_ai_warmed(model, warm, result),
            (Pending::AiPull { model }, result) => self.on_ai_pulled(model, result),
            (Pending::AiDelete { model }, result) => self.on_ai_deleted(model, result),
            (Pending::Browse, result) => self.on_browse(result),
            (Pending::Search { seq }, result) => self.on_search_results(seq, result),
            (Pending::Serve { call, quiet }, result) => self.on_serve(call, quiet, result),
            (Pending::GitDetails, result) => self.on_git_details(result),
            (Pending::Diff { scope, epoch }, result) => self.on_diff(scope, epoch, result),
            (Pending::DiffGit, result) => self.on_diff_git(result),
            (Pending::WorkLand { id }, result) => self.on_diff_landed(id, result),
            (Pending::DiffReview { id }, result) => self.on_diff_review(id, result),
            (Pending::DiffPlan { id, stamp }, result) => self.on_diff_plan(id, stamp, result),
            (Pending::GitRun { action }, result) => self.on_git_ran(action, result),
            (Pending::Tree, result) => self.on_tree(result),
            (Pending::TreeGit, result) => self.on_tree_git(result),
            (Pending::TreeFs { reveal, renamed, open }, result) => self.on_tree_fs(reveal, renamed, open, result),
            (Pending::Reload { tab }, Ok(Response::File { data, mtime_ns, .. })) => {
                self.on_reload(tab, data, mtime_ns);
                Task::none()
            }
            // Deleted by the operation: the tab keeps what it had.
            (Pending::Reload { .. }, Err(_)) => Task::none(),
            (Pending::ReloadMedia { tab }, Ok(Response::File { data, .. })) => self.on_reload_media(tab, data),
            (Pending::ReloadMedia { .. }, Err(_)) => Task::none(),
            (Pending::Open { new_tab, .. }, Ok(Response::File { path, data, .. }))
                if Kind::of(std::path::Path::new(&path)) == Kind::Image =>
            {
                self.open_image(path, data, new_tab)
            }
            (Pending::Open { new_tab, .. }, Ok(Response::File { path, data, .. }))
                if Kind::of(std::path::Path::new(&path)) == Kind::Pdf =>
            {
                self.open_pdf(path, data, new_tab)
            }
            (Pending::PreviewImage { tab, url }, Ok(Response::File { data, .. })) => {
                self.preview_image(tab, url, data);
                Task::none()
            }
            // A missing preview image just stays as its alt text.
            (Pending::PreviewImage { .. }, Err(_)) => Task::none(),
            (Pending::AiImage { chat, url }, Ok(Response::File { data, .. })) => {
                self.ai_image(chat, url, data);
                Task::none()
            }
            (Pending::AiImage { .. }, Err(_)) => Task::none(),
            (Pending::Open { new_tab, goto, .. }, Ok(Response::File { path, data, mtime_ns })) => {
                let mut doc = Document::from_bytes(&data, Some(PathBuf::from(&path)));
                doc.tab_width = self.tab_width;
                doc.disk_mtime_ns = Some(mtime_ns);
                let ro = doc.read_only;
                self.put_doc(doc, new_tab);
                self.lsp_sync();
                if let Some(pos) = goto {
                    self.goto_in_active(pos);
                }
                if ro {
                    return self.flash_warn("file is not valid UTF-8 — opened read-only");
                }
                Task::none()
            }
            (Pending::Lsp { tab, version, op }, result) => self.on_lsp(tab, version, op, result),
            (Pending::ApplyEdits, Ok(_)) => Task::none(),
            (Pending::Open { path, new_tab, .. }, Err(e)) if e.kind == ErrorKind::NotFound => {
                let abs = nib_proto::path::join(&self.root, &path);
                let mut doc = Document::new(Default::default(), Some(PathBuf::from(abs)));
                doc.tab_width = self.tab_width;
                self.put_doc(doc, new_tab);
                self.flash_info(format!("new file: {path}"))
            }
            (Pending::Save { tab, revision }, Ok(Response::Written { mtime_ns, path })) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.doc.mark_saved(revision, mtime_ns);
                }
                self.force_save_armed = false;
                let saved = self.flash_info(format!("saved {}", path.rsplit('/').next().unwrap_or(&path)));
                Task::batch([saved, self.tree_refresh()])
            }
            (Pending::Save { .. }, Err(e)) if e.kind == ErrorKind::Conflict => {
                self.force_save_armed = true;
                self.flash_warn("file changed on disk since it was opened — save again to overwrite")
            }
            (Pending::PtyOpen { term }, Ok(Response::PtyOpened { id })) => {
                let Some(t) = self.terms.iter_mut().find(|t| t.key == term) else {
                    // Closed before the daemon answered.
                    self.notify(ClientNotify::PtyClose { id });
                    return Task::none();
                };
                {
                    t.id = Some(id);
                    let (rows, cols) = t.size;
                    let queued = std::mem::take(&mut t.queued);
                    self.notify(ClientNotify::PtyResize { id, cols, rows });
                    if !queued.is_empty() {
                        self.notify(ClientNotify::PtyInput { id, data: queued });
                    }
                }
                Task::none()
            }
            (Pending::PtyOpen { term }, Err(e)) => {
                self.remove_term(term);
                Task::batch([self.flash_warn(format!("terminal: {e}")), self.ai_term_closed(term)])
            }
            (_, Err(e)) => self.flash_warn(e.to_string()),
            (_, Ok(other)) => self.flash_warn(format!("unexpected response: {other:?}")),
        }
    }

    /// Show a freshly loaded document: in a new tab, or replacing the active (unmodified) tab.
    fn put_doc(&mut self, doc: Document, new_tab: bool) {
        let tab = Tab::new(self.next_tab_id, doc);
        self.next_tab_id += 1;
        let replace = !new_tab && self.tab().is_some_and(|t| !t.doc.is_modified());
        if replace {
            self.lsp_close(self.active);
            self.tabs[self.active] = tab;
        } else {
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
        self.focus = Focus::Main;
    }

    fn current_dir(&self) -> String {
        // A local tab's folder doesn't exist on a remote target.
        self.tab()
            .filter(|t| !t.local)
            .and_then(|t| t.doc.path.as_ref())
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.clone())
    }

    fn save(&mut self) -> Task<Message> {
        let force = std::mem::take(&mut self.force_save_armed);
        if self.tab().is_some_and(|t| t.local) {
            return self.save_local_tab();
        }
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        if tab.doc.read_only {
            return self.flash_warn("read-only buffer");
        }
        let Some(path) = tab.doc.path.clone() else { return Task::none() };
        let revision = tab.doc.save_point();
        let req = Request::WriteFile {
            path: path.to_string_lossy().into_owned(),
            data: tab.doc.to_bytes(),
            expected_mtime_ns: if force { None } else { tab.doc.disk_mtime_ns },
        };
        let tab = tab.id;
        self.request(req, Pending::Save { tab, revision })
    }

    // -----------------------------------------------------------------------------------------
    // terminal
    // -----------------------------------------------------------------------------------------

    fn focused_term(&self) -> Option<&Term> {
        match self.focus {
            Focus::Terminal(k) if self.term_shown(k) => self.terms.iter().find(|t| t.key == k),
            _ => None,
        }
    }

    /// The terminals that belong where the window is now (a server's only on its app's page).
    fn terms_here(&self) -> impl Iterator<Item = &Term> {
        self.terms.iter().filter(|t| self.term_here(t))
    }

    /// Terminal `k` is on screen.
    fn term_shown(&self, k: u64) -> bool {
        self.terms_visible && self.terms_here().any(|t| t.key == k)
    }

    /// The keyboard leaves a terminal that is no longer on screen (its app's page went behind).
    fn terms_sync(&mut self) {
        if let Focus::Terminal(k) = self.focus {
            if !self.term_shown(k) {
                self.focus = self.content_focus();
            }
        }
    }

    fn set_focus(&mut self, f: Focus) {
        if let Focus::Terminal(k) = f {
            self.last_term = Some(k);
        }
        self.focus = f;
    }

    /// Panes keyboard focus can cycle through: the file tree first, the main view, then each
    /// visible terminal.
    fn focus_ring(&self) -> Vec<Focus> {
        let mut ring = Vec::new();
        if self.tree.open {
            ring.push(Focus::Tree);
        }
        if self.tab_slots() >= 2 {
            ring.push(Focus::Tabs);
        }
        ring.push(if self.ai.shown() { Focus::Ai } else { Focus::Main });
        if self.terms_visible {
            ring.extend(self.terms_here().map(|t| Focus::Terminal(t.key)));
        }
        if self.git_ui.open {
            ring.push(Focus::Git);
        }
        if self.apps.open {
            ring.push(Focus::Apps);
        }
        if self.ssh.open {
            ring.push(Focus::Ssh);
        }
        ring.push(Focus::Toolbar);
        ring
    }

    fn cycle_focus(&mut self, back: bool) {
        let ring = self.focus_ring();
        let n = ring.len();
        let i = ring.iter().position(|f| *f == self.focus).or_else(|| ring.iter().position(|f| *f == Focus::Main)).unwrap_or(0);
        let next = ring[if back { (i + n - 1) % n } else { (i + 1) % n }];
        if next == Focus::Toolbar {
            self.enter_toolbar();
        } else {
            self.set_focus(next);
        }
    }

    /// Ctrl+Tab. In the editor with several files open it walks the files; anywhere else there
    /// is nothing to switch between, so it walks the panes.
    fn switch_tab_or_pane(&mut self, back: bool) {
        let n = self.tabs.len();
        // The agents tab is one more tab, after the files.
        let all = n + usize::from(self.ai.open);
        let at = if self.ai.shown() { n } else { self.active };
        if matches!(self.focus, Focus::Main | Focus::Ai) && all >= 2 {
            let to = if back { (at + all - 1) % all } else { (at + 1) % all };
            self.ai.front = to == n;
            if to < n {
                self.active = to;
                self.focus = Focus::Main;
            } else {
                self.focus = Focus::Ai;
            }
        } else {
            self.cycle_focus(back);
        }
    }

    /// How many tabs the bar holds: the files, then the agents tab, then its history tab.
    fn tab_slots(&self) -> usize {
        self.tabs.len() + usize::from(self.ai.open) + usize::from(self.ai.open && self.ai.history_open())
    }

    /// The tab in front, counted along the bar.
    fn tab_slot(&self) -> usize {
        let n = self.tabs.len();
        match (self.ai.shown(), self.ai_history_front()) {
            (false, _) => self.active,
            (true, false) => n,
            (true, true) => n + 1,
        }
    }

    /// The pane under the tab bar that takes the keys.
    fn content_focus(&self) -> Focus {
        if self.ai.shown() { Focus::Ai } else { Focus::Main }
    }

    /// Keys while the tab bar has focus: the tab picked shows at once, the keys stay in the bar.
    fn tabs_key(&mut self, chord: KeyChord) -> Task<Message> {
        let (all, at) = (self.tab_slots().max(1), self.tab_slot());
        let plain = !(chord.ctrl || chord.logo || chord.alt || chord.shift);
        let cmd = self.keymap.lookup(&chord);
        let to = match (chord.key, cmd) {
            (Key::Left, _) if plain => (at + all - 1) % all,
            (Key::Right, _) if plain => (at + 1) % all,
            (_, Some(Command::PrevTab)) => (at + all - 1) % all,
            (_, Some(Command::NextTab)) => (at + 1) % all,
            (Key::Enter | Key::Down | Key::Esc, _) if plain => {
                self.focus = self.content_focus();
                return Task::none();
            }
            (_, Some(c)) if app_wide(c) => return self.exec(c),
            _ => return Task::none(),
        };
        let n = self.tabs.len();
        let task = if to < n {
            self.active = to;
            self.ai.front = false;
            Task::none()
        } else if to == n {
            self.handle(Message::Ai(ai::Msg::Front))
        } else {
            self.handle(Message::Ai(ai::Msg::History))
        };
        self.focus = Focus::Tabs;
        task
    }

    fn enter_toolbar(&mut self) {
        if self.focus != Focus::Toolbar {
            self.toolbar_return = self.focus;
        }
        self.focus = Focus::Toolbar;
    }

    /// Back to the pane the toolbar was entered from (if it still exists).
    fn leave_toolbar(&mut self) {
        let back = self.toolbar_return;
        let alive = match back {
            Focus::Terminal(k) => self.term_shown(k),
            Focus::Git => self.git_ui.open,
            Focus::Ai => self.ai.shown(),
            Focus::Ssh => self.ssh.open,
            Focus::Apps => self.apps.open,
            Focus::Tree => self.tree.open,
            Focus::Tabs => self.tab_slots() >= 2,
            _ => true,
        };
        self.focus = if alive && back != Focus::Toolbar { back } else { Focus::Main };
    }

    /// Ctrl + ` + H. Never opens a terminal.
    fn hide_show_terms(&mut self) -> Task<Message> {
        if self.terms_here().next().is_none() {
            return self.flash_info("no terminals (Ctrl+` opens one)");
        }
        self.toggle_terminal(true)
    }

    fn new_terminal(&mut self) -> Task<Message> {
        let cwd = self.current_dir();
        self.open_terminal(|cols, rows| Request::PtyOpen { cwd, cmd: None, cols, rows }).1
    }

    /// A terminal running what `req` starts (it answers `PtyOpened`), with the keyboard in it.
    fn open_terminal(&mut self, req: impl FnOnce(u16, u16) -> Request) -> (u64, Task<Message>) {
        let (rows, cols) = (24, 80);
        let key = self.next_term_key;
        self.next_term_key += 1;
        self.terms.push(Term {
            key,
            id: None,
            parser: vt100::Parser::new(rows.try_into().unwrap(), cols.try_into().unwrap(), 5000),
            size: (rows, cols),
            queued: Vec::new(),
            server: None,
        });
        self.terms_visible = true;
        self.set_focus(Focus::Terminal(key));
        (key, self.request(req(cols, rows), Pending::PtyOpen { term: key }))
    }

    fn remove_term(&mut self, key: u64) {
        let Some(i) = self.terms.iter().position(|t| t.key == key) else { return };
        self.terms.remove(i);
        if self.last_term == Some(key) {
            self.last_term = None;
        }
        if self.focus == Focus::Terminal(key) {
            // Next terminal up (the slot before it in the focus ring).
            self.focus = match i.checked_sub(1) {
                Some(j) => Focus::Terminal(self.terms[j].key),
                None => Focus::Main,
            };
        }
        if self.terms.is_empty() {
            self.terms_visible = false;
        }
    }

    fn close_focused_term(&mut self) {
        let Focus::Terminal(key) = self.focus else { return };
        if !self.terms_visible {
            return;
        }
        if let Some(id) = self.terms.iter().find(|t| t.key == key).and_then(|t| t.id) {
            self.notify(ClientNotify::PtyClose { id });
        }
        self.remove_term(key);
    }

    /// Ctrl+`: open a terminal if there is none, show the terminals if hidden, otherwise
    /// switch between the editor and the last-used terminal. The strip button shows/hides.
    fn toggle_terminal(&mut self, from_button: bool) -> Task<Message> {
        // Another app's servers are not terminals to come back to here.
        let Some(first) = self.terms_here().next().map(|t| t.key) else {
            return self.new_terminal();
        };
        let back_to = self.last_term.filter(|k| self.terms_here().any(|t| t.key == *k)).unwrap_or(first);
        if !self.terms_visible {
            self.terms_visible = true;
            self.set_focus(Focus::Terminal(back_to));
        } else if from_button {
            self.terms_visible = false;
            self.focus = Focus::Main;
        } else if self.focus == Focus::Main {
            self.set_focus(Focus::Terminal(back_to));
        } else {
            self.focus = Focus::Main;
        }
        Task::none()
    }

    fn term_input(&mut self, data: Vec<u8>) {
        let Focus::Terminal(k) = self.focus else { return };
        let Some(t) = self.terms.iter_mut().find(|t| t.key == k) else { return };
        if t.parser.screen().scrollback() > 0 {
            t.parser.screen_mut().set_scrollback(0);
        }
        match t.id {
            Some(id) => self.notify(ClientNotify::PtyInput { id, data }),
            None => t.queued.extend(data),
        }
    }

    fn on_term(&mut self, key: u64, ev: TermEvent) -> Task<Message> {
        let Some(t) = self.terms.iter_mut().find(|t| t.key == key) else { return Task::none() };
        match ev {
            TermEvent::Focus => self.set_focus(Focus::Terminal(key)),
            // A page of the machine the shell runs on: a web tab (its port forwarded first, if
            // that is another machine). Anything else: the system's browser, on the machine at
            // the keyboard (a remote shell has none).
            TermEvent::OpenUrl(url) => self.open_link(&url),
            TermEvent::Scroll(lines) => {
                let s = t.parser.screen_mut();
                let cur = s.scrollback() as i64;
                s.set_scrollback((cur + lines as i64).max(0) as usize);
            }
            TermEvent::Resized { rows, cols } => {
                t.size = (rows, cols);
                t.parser.screen_mut().set_size(rows.try_into().unwrap(), cols.try_into().unwrap());
                if let Some(id) = t.id {
                    self.notify(ClientNotify::PtyResize { id, cols, rows });
                }
            }
        }
        Task::none()
    }

    /// Keys while the terminal has focus: app shortcuts stay with the app (Cmd on macOS,
    /// Ctrl+Shift elsewhere, Ctrl+`/Ctrl+Tab everywhere); everything else goes to the shell.
    fn terminal_key(&mut self, chord: KeyChord, text: Option<String>) -> Option<Task<Message>> {
        let cmd = self.keymap.lookup(&chord);
        let app_chord = if cfg!(target_os = "macos") { chord.logo } else { chord.ctrl && chord.shift };
        // Ctrl chords shells don't use; others (Ctrl+Q, Ctrl+W, ...) stay with the shell.
        let always_app = cmd.is_some_and(|c| {
            tab_number(c).is_some()
                || matches!(
                    c,
                    Command::ToggleTerminal
                        | Command::NewTerminal
                        | Command::NextTab
                        | Command::PrevTab
                        | Command::NextPane
                        | Command::PrevPane
                        | Command::FontBigger
                        | Command::FontSmaller
                )
        });
        if app_chord || always_app {
            let is_copy_paste = matches!(chord.key, Key::Char('v'));
            if is_copy_paste {
                return Some(iced::clipboard::read().map(Message::PastedTerm));
            }
            return match cmd {
                Some(Command::Copy | Command::Paste | Command::Cut | Command::Undo | Command::Redo | Command::SelectAll) => {
                    Some(Task::none())
                }
                Some(c) => Some(self.exec(c)),
                None => Some(Task::none()),
            };
        }
        let app_cursor = self.focused_term().is_some_and(|t| t.parser.screen().application_cursor());
        let bytes = key_bytes(&chord, text.as_deref(), app_cursor)?;
        self.term_input(bytes);
        Some(Task::none())
    }

    // -----------------------------------------------------------------------------------------
    // input
    // -----------------------------------------------------------------------------------------

    /// Pane gestures around the terminal key. `Some` = the key was consumed.
    fn pane_gesture(&mut self, chord: KeyChord, repeat: bool) -> Option<Task<Message>> {
        let done = Some(Task::none());
        let cmd = self.keymap.lookup(&chord);
        if let Some(l) = &mut self.leader {
            if chord.key == l.key {
                return done; // auto-repeat while held
            }
            if chord.ctrl && chord.key == HIDE_TERMS_KEY {
                if repeat {
                    return done;
                }
                l.used = true;
                return Some(self.hide_show_terms());
            }
            if chord.ctrl && chord.key == TOOLBAR_KEY {
                if !repeat {
                    l.used = true;
                    if self.focus == Focus::Toolbar {
                        self.leave_toolbar();
                    } else {
                        self.enter_toolbar();
                    }
                }
                return done;
            }
            self.leader = None; // anything else cancels; the key is handled normally
        }
        // One step per press: a held key never races through panes or tabs.
        if repeat && matches!(cmd, Some(Command::NextTab | Command::PrevTab | Command::NextPane | Command::PrevPane)) {
            return done;
        }
        let tap = match cmd {
            Some(c @ (Command::ToggleTerminal | Command::NewTerminal)) => c,
            _ => return None,
        };
        if repeat {
            return done; // never repeat-open terminals
        }
        let mut leader = Leader { key: chord.key, tap, used: false };
        if self.held.contains(&Key::Tab) {
            // Pressed on top of Ctrl+Tab (the old pane gesture): Tab already did its job, and
            // this must not open a terminal.
            leader.used = true;
        }
        self.leader = Some(leader);
        done
    }

    fn on_key(&mut self, chord: KeyChord, text: Option<String>, repeat: bool) -> Task<Message> {
        if !repeat && !self.held.contains(&chord.key) {
            self.held.push(chord.key);
        }
        // Settings > Keyboard Shortcuts is waiting for a new chord: it gets the key, whatever it is.
        if let Some(cmd) = self.settings.recording.filter(|_| !repeat) {
            return self.record_key(cmd, chord);
        }
        if self.settings.menu && chord == KeyChord::new(Key::Esc) {
            self.settings.menu = false;
            return Task::none();
        }
        if self.search.open {
            return self.search_key(chord, text);
        }
        if self.focus == Focus::Main && self.code.modal() {
            return self.code_key(chord, text);
        }
        let gesture = self.pane_gesture(chord, repeat);
        if self.keylog {
            eprintln!(
                "key down {chord:?} repeat={repeat} held={:?} -> leader={:?} focus={:?} consumed={}",
                self.held, self.leader, self.focus, gesture.is_some()
            );
        }
        if let Some(t) = gesture {
            return t;
        }
        if self.focus == Focus::Toolbar {
            return self.toolbar_key(chord);
        }
        if self.focus == Focus::Tabs {
            return self.tabs_key(chord);
        }
        if self.focus == Focus::Git {
            return self.git_key(chord, text);
        }
        if self.focus == Focus::Ai {
            return self.ai_key_in_view(chord, text);
        }
        if self.focus == Focus::Ssh {
            return self.ssh_key(chord, text);
        }
        if self.focus == Focus::Apps {
            return self.apps_key(chord, text);
        }
        if self.focus == Focus::Tree && self.tree.open {
            return self.tree_key(chord, text);
        }
        if self.focused_term().is_some() {
            if let Some(t) = self.terminal_key(chord, text.clone()) {
                return t;
            }
            return Task::none();
        }
        if self.web_front().is_some() && self.focus == Focus::Main {
            if let Some(t) = self.web_key(chord, text.clone()) {
                return t;
            }
        }
        if self.diff_active() {
            if let Some(t) = self.diff_key(chord, text.clone()) {
                return t;
            }
        }
        let cmd = self.keymap.lookup(&chord);
        if cmd != Some(Command::Quit) {
            self.quit_armed = false;
        }
        if cmd != Some(Command::CloseTab) {
            self.close_armed = false;
        }
        if cmd != Some(Command::Save) {
            self.force_save_armed = false;
        }
        if let Some(c) = cmd {
            return self.exec(c);
        }
        if chord == KeyChord::new(Key::Char(' ')) && self.pdf_active() {
            return self.exec(Command::PageDown);
        }
        if chord == KeyChord::new(Key::Char(' ')) && self.tab().is_some_and(|t| t.media.is_some()) {
            return self.handle(Message::MediaClick);
        }
        // Unbound: insert the produced text (covers Shift, AltGr, Option-symbols on macOS).
        if !(chord.ctrl || chord.logo) && self.editing() {
            if let (Some(t), Some(tab)) = (text, self.tabs.get_mut(self.active)) {
                for c in t.chars().filter(|c| !c.is_control()) {
                    tab.doc.type_char(c);
                }
                self.follow_cursor();
            }
        }
        Task::none()
    }

    fn toolbar_key(&mut self, chord: KeyChord) -> Task<Message> {
        let n = self.toolbar_items().len();
        let plain = !(chord.ctrl || chord.logo || chord.alt);
        match chord.key {
            Key::Left if plain => self.toolbar_sel = self.toolbar_sel.saturating_sub(1),
            Key::Right if plain => self.toolbar_sel = (self.toolbar_sel + 1).min(n - 1),
            Key::Enter | Key::Char(' ') if plain => {
                let msg = self.toolbar_items().swap_remove(self.toolbar_sel.min(n - 1)).2;
                self.leave_toolbar();
                return self.update(msg);
            }
            Key::Esc => self.leave_toolbar(),
            _ => {
                if let Some(c) = self.keymap.lookup(&chord).filter(|c| app_wide(*c)) {
                    return self.exec(c);
                }
            }
        }
        Task::none()
    }

    fn reopen_closed_tab(&mut self) -> Task<Message> {
        let Some(path) = self.closed.pop() else { return self.flash_info("no closed tabs") };
        self.open_path(path, true)
    }

    fn on_key_up(&mut self, key: Key) -> Task<Message> {
        self.held.retain(|k| *k != key);
        let task = match self.leader.take() {
            Some(l) if l.key == key => {
                if l.used {
                    Task::none()
                } else {
                    self.exec(l.tap)
                }
            }
            other => {
                self.leader = other;
                Task::none()
            }
        };
        if self.keylog {
            eprintln!("key up {key:?} held={:?} -> leader={:?} focus={:?}", self.held, self.leader, self.focus);
        }
        task
    }

    fn exec(&mut self, c: Command) -> Task<Message> {
        use Command::*;
        match c {
            Quit => return self.quit(),
            Save => return self.save(),
            ToggleExplorer => return self.toggle_tree(),
            Find => return self.search_start(),
            // Only means something in the file tree, which handles it itself.
            OpenInNewTab => return Task::none(),
            ToggleTerminal => return self.toggle_terminal(false),
            NewTerminal => return self.new_terminal(),
            AppUp => return self.app_op(nib_proto::AppOp::Up { names: Vec::new() }),
            AppDown => return self.app_op(nib_proto::AppOp::Down { names: Vec::new() }),
            AppRestart => return self.serve_restart_focused(),
            ToggleAi => return self.on_ai_msg(ai::Msg::Toggle),
            ToggleGit => return self.on_git_msg(git::Msg::Toggle),
            ToggleDiff => return self.diff_toggle(),
            ToggleSsh => return self.on_ssh_msg(ssh::Msg::Toggle),
            ToggleApps => return self.on_apps_msg(apps::Msg::Toggle),
            OpenFolder => {
                let root = self.root.clone();
                return self.ssh_browse(root);
            }
            FocusToolbar => {
                if self.focus == Focus::Toolbar {
                    self.leave_toolbar();
                } else {
                    self.enter_toolbar();
                }
                return Task::none();
            }
            OpenSettings => return self.open_settings(settings::Page::Appearance),
            KeyboardShortcuts => return self.open_settings(settings::Page::Shortcuts),
            ToggleBeginnerMode => return self.toggle_beginner(!self.beginner),
            ToggleFullscreen => return self.set_fullscreen(!self.settings.window.fullscreen),
            NextTab | PrevTab => {
                self.switch_tab_or_pane(c == PrevTab);
                return Task::none();
            }
            NextAgent | PrevAgent => {
                if self.ai.shown() {
                    self.ai_cycle(c == PrevAgent);
                }
                return Task::none();
            }
            NewAgent => return self.ai_new_agent(),
            KillAgent if self.ai.shown() => return self.on_ai_msg(ai::Msg::Kill(self.ai_current())),
            KillAgent => return Task::none(),
            FoldAgents => return self.on_ai_msg(ai::Msg::Fold),
            AgentHistory => return self.ai_history_open(),
            Panic => return self.ai_panic(),
            NextPane | PrevPane => {
                self.cycle_focus(c == PrevPane);
                return Task::none();
            }
            CloseTab => return self.close_focused(),
            ReopenClosedTab => return self.reopen_closed_tab(),
            FontBigger | FontSmaller | TogglePreview if self.pdf_active() => {
                return self.pdf_command(c).unwrap_or_else(Task::none);
            }
            FontBigger | FontSmaller if self.preview_shown() && self.focus == Focus::Main => {
                return self.zoom_preview(if c == FontBigger { 1 } else { -1 });
            }
            // Over a web tab they zoom the page, as in a browser.
            FontBigger | FontSmaller if self.web_front().is_some() && self.focus == Focus::Main => {
                return self.web_zoom_by(if c == FontBigger { 1 } else { -1 });
            }
            FontBigger | FontSmaller => return self.zoom_font(if c == FontBigger { 1 } else { -1 }),
            c if tab_number(c).is_some() => {
                let i = tab_number(c).unwrap();
                if i < self.tabs.len() {
                    self.active = i;
                }
                return Task::none();
            }
            TogglePreview => return self.toggle_preview(),
            Paste => return iced::clipboard::read().map(Message::Pasted),
            NavigateBack | NavigateForward => return self.navigate(c == NavigateForward),
            GoToDefinition | FindReferences | RenameSymbol | ChangeAllOccurrences if self.editing() => {
                return self.code_command(c);
            }
            _ => {}
        }
        if self.diff_active() {
            return self.diff_command(c);
        }
        if !self.editing() {
            if let Some(task) = self.pdf_command(c) {
                return task;
            }
            self.passive_command(c);
            return Task::none();
        }
        let page = self.rows.max(2) as isize - 1;
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        let doc = &mut tab.doc;
        let mut task = Task::none();
        match c {
            Undo => {
                if !doc.undo() {
                    task = self.flash_info("nothing to undo");
                }
            }
            Redo => {
                if !doc.redo() {
                    task = self.flash_info("nothing to redo");
                }
            }
            Copy => task = iced::clipboard::write(doc.copy_text().0),
            Cut => task = iced::clipboard::write(doc.cut()),
            SelectAll => doc.select_all(),
            Indent => doc.indent(),
            Dedent => doc.dedent(),
            Newline => doc.newline(),
            DeleteBackward => doc.delete_backward(),
            DeleteForward => doc.delete_forward(),
            DeleteWordBackward => doc.delete_word_backward(),
            DeleteLine => doc.delete_lines(),
            DuplicateLine => doc.duplicate_line(),
            ToggleComment => doc.toggle_comment(),
            MoveLeft => doc.move_horizontal(false, false),
            MoveRight => doc.move_horizontal(true, false),
            MoveUp => doc.move_vertical(-1, false),
            MoveDown => doc.move_vertical(1, false),
            MoveWordLeft => doc.move_word(false, false),
            MoveWordRight => doc.move_word(true, false),
            MoveLineStart => doc.move_line_start(false),
            MoveLineEnd => doc.move_line_end(false),
            MoveDocStart => doc.move_doc(false, false),
            MoveDocEnd => doc.move_doc(true, false),
            PageUp => doc.move_vertical(-page, false),
            PageDown => doc.move_vertical(page, false),
            SelectLeft => doc.move_horizontal(false, true),
            SelectRight => doc.move_horizontal(true, true),
            SelectUp => doc.move_vertical(-1, true),
            SelectDown => doc.move_vertical(1, true),
            SelectWordLeft => doc.move_word(false, true),
            SelectWordRight => doc.move_word(true, true),
            SelectLineStart => doc.move_line_start(true),
            SelectLineEnd => doc.move_line_end(true),
            SelectPageUp => doc.move_vertical(-page, true),
            SelectPageDown => doc.move_vertical(page, true),
            SelectDocStart => doc.move_doc(false, true),
            SelectDocEnd => doc.move_doc(true, true),
            CollapseSelection => doc.collapse_selection(),
            _ => {}
        }
        self.follow_cursor();
        task
    }

    /// Close only what has focus: the focused terminal, the file tree, or the editor tab. The bottom toolbar has nothing to close.
    fn close_focused(&mut self) -> Task<Message> {
        match self.focus {
            Focus::Terminal(k) if self.term_shown(k) => {
                self.close_focused_term();
                Task::none()
            }
            Focus::Toolbar => Task::none(),
            // The tab shown, whole: the agents tab closes, its agents keep working.
            Focus::Tabs if self.ai_history_front() && self.ai.shown() => self.handle(Message::Ai(ai::Msg::HistoryClose)),
            Focus::Tabs if self.ai.shown() => {
                self.ai_close();
                Task::none()
            }
            Focus::Git => {
                self.git_close();
                Task::none()
            }
            Focus::Ai => self.ai_close_key(),
            Focus::Ssh => {
                self.ssh_close();
                Task::none()
            }
            Focus::Apps => {
                self.apps_close();
                Task::none()
            }
            Focus::Tree if self.tree.open => {
                self.close_tree();
                Task::none()
            }
            _ => self.close_tab(),
        }
    }

    fn close_tab(&mut self) -> Task<Message> {
        let Some(tab) = self.tab() else { return Task::none() };
        if tab.doc.is_modified() && !self.close_armed {
            self.close_armed = true;
            return self.flash_warn(format!("unsaved changes — {MOD}W again to discard"));
        }
        self.close_armed = false;
        self.lsp_close(self.active);
        let tab = self.tabs.remove(self.active);
        if let (false, Some(p)) = (tab.local, tab.doc.path.as_ref()) {
            self.closed.push(p.to_string_lossy().into_owned());
        }
        if self.tabs.is_empty() && self.ai.open {
            // The agents tab is the one tab left: it comes forward, like any neighbour would.
            self.active = 0;
            self.ai.front = true;
            self.focus = Focus::Ai;
            self.ai_set_tabs_seen((0, 0, None));
            return Task::none();
        }
        if self.tabs.is_empty() {
            // Nothing left to edit: back to choosing a file.
            self.active = 0;
            self.tree.open = true;
            self.focus = Focus::Tree;
            return self.tree_refresh();
        }
        self.active = self.active.min(self.tabs.len() - 1);
        Task::none()
    }

    /// A thin bar for `fraction` (0..=1) of a step done, in the theme's accent.
    fn progress(&self, fraction: f32) -> Element<'_, Message> {
        let (bar, track) = (self.theme.accent, self.theme.divider);
        progress_bar(0.0..=1.0, fraction.clamp(0.0, 1.0))
            .girth(4)
            .style(move |_| progress_bar::Style {
                background: track.into(),
                bar: bar.into(),
                border: Border { radius: 2.0.into(), ..Border::default() },
            })
            .into()
    }

    fn quit(&mut self) -> Task<Message> {
        let dirty = self.tabs.iter().filter(|t| t.doc.is_modified()).count();
        if dirty > 0 && !self.quit_armed {
            self.quit_armed = true;
            return self.flash_warn(format!("{dirty} unsaved file(s) — quit again to discard"));
        }
        crate::forward::clear_now();
        iced::exit()
    }

    fn follow_cursor(&mut self) {
        let (rows, cols) = (self.rows, self.cols);
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let (line, col) = tab.doc.cursor_line_col();
        let so = 3.min(rows.saturating_sub(1) / 2);
        if line < tab.top + so {
            tab.top = line.saturating_sub(so);
        } else if line + so >= tab.top + rows {
            tab.top = line + so + 1 - rows;
        }
        if col < tab.left {
            tab.left = col.saturating_sub(4);
        } else if col >= tab.left + cols {
            tab.left = col + 5 - cols.min(col + 5);
        }
    }

    fn on_view(&mut self, ev: ViewEvent) {
        if let ViewEvent::Resized { rows, cols } = ev {
            self.rows = rows;
            self.cols = cols;
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let doc = &mut tab.doc;
        let t = doc.text.slice(..);
        let to_pos = |line: usize, col: usize| {
            let line = line.min(t.len_lines().saturating_sub(1));
            nib_editor::document::char_at_visual_col(t, line, col, doc.tab_width)
        };
        match ev {
            ViewEvent::Click { line, col, extend, .. } => {
                let p = to_pos(line, col);
                doc.set_cursor(p, extend);
            }
            ViewEvent::Drag { line, col } => {
                let p = to_pos(line, col);
                doc.set_cursor(p, true);
                self.follow_cursor();
            }
            ViewEvent::Scroll { lines, cols } => {
                let max = doc.text.len_lines().saturating_sub(1) as f32;
                tab.top = (tab.top as f32 + lines.round()).clamp(0.0, max) as usize;
                tab.left = (tab.left as f32 + cols.round()).max(0.0) as usize;
            }
            ViewEvent::Resized { .. } | ViewEvent::Hover { .. } | ViewEvent::HoverEnd => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // view
    // -----------------------------------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        let t0 = Instant::now();
        let e = zoom::ZoomRoot::new(self.build_view(), Message::WheelZoom).into();
        crate::timing::report("view", "window", t0.elapsed(), nib_proto::timing::SLOW_UI);
        e
    }

    fn build_view(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let bg = th.background;
        let main: Element<'_, Message> = if self.ai_history_front() && self.ai.shown() {
            self.ai_history_view()
        } else if self.ai.shown() {
            self.ai_stage()
        } else if let Some(tab) = self.tab() {
            self.tab_view(tab)
        } else {
            self.empty_state()
        };

        // The halo marks where keys go; on the main view only when another pane competes for them.
        let here: Vec<&Term> = if self.terms_visible { self.terms_here().collect() } else { Vec::new() };
        let halo = !here.is_empty() || self.tree.open;
        let mut center = row![];
        if self.tree.open {
            center = center.push(self.tree_view()).push(self.vrule());
        }
        center = center.push(self.pane(main, halo && matches!(self.focus, Focus::Main | Focus::Ai)).id(drop::MAIN).width(Length::FillPortion(3)));
        if !here.is_empty() {
            let mut stack = column![];
            for (i, t) in here.into_iter().enumerate() {
                if i > 0 {
                    stack = stack.push(self.hrule());
                }
                let focused = self.focus == Focus::Terminal(t.key);
                let key = t.key;
                let tv = TerminalView::new(t.parser.screen(), th, self.face.font, self.face.size, focused, move |ev| Message::Term(key, ev));
                let body: Element<'_, Message> = match self.serve_header(t) {
                    Some(head) => column![head, Element::from(tv)].into(),
                    None => tv.into(),
                };
                stack = stack.push(self.pane(body, focused));
            }
            center = center.push(self.vrule());
            center = center.push(stack.width(Length::FillPortion(2)).height(Length::Fill));
        }
        // Far right, so terminals open between the code and the cheat sheet.
        if self.beginner {
            center = center.push(self.vrule());
            center = center.push(self.beginner_pane());
        }
        // One side panel at a time: Git, SSH, Apps and Settings share the spot.
        if let Some(panel) = self.git_panel().or_else(|| self.ssh_panel()).or_else(|| self.apps_panel()).or_else(|| self.settings_panel()) {
            center = center.push(self.vrule());
            center = center.push(panel);
        }

        let mut col = column![];
        if TITLEBAR_H > 0.0 {
            // The content runs under the transparent title bar; keep it clear of the traffic lights.
            col = col.push(space().height(TITLEBAR_H));
        }
        if let Some(bar) = self.titlebar() {
            col = col.push(bar).push(self.hrule());
        }
        if !self.tabs.is_empty() || self.ai.open {
            col = col.push(self.tab_bar());
        }
        col = col.push(center).push(self.hrule()).push(self.control_strip());
        let base = container(col)
            .style(move |_| container::Style::default().background(bg))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        self.with_frame_edges(self.with_settings_menu(self.with_code_overlay(base)))
    }

    /// What the editor area shows with no tab open: the logo, plus a few shortcuts to get going.
    fn empty_state(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let h = 28.0;
        let logo = svg(self.icons.logo.clone()).width(h * crate::icons::LOGO_ASPECT).height(h);
        let name = text("nibble").size(18).font(crate::fonts::mono()).color(th.dim);
        let mut col = column![logo, name].spacing(10).align_x(iced::Alignment::Center);

        if self.connected {
            let mut hints = column![].spacing(8);
            for (cmd, what) in [
                (Command::ToggleExplorer, "open a file"),
                (Command::ReopenClosedTab, "reopen closed tab"),
                (Command::NewTerminal, "new terminal"),
                (Command::ToggleGit, "git panel"),
            ] {
                // Follow the user's keymap; skip anything they unbound.
                let Some(k) = self.keymap.chords_for(cmd).first().copied() else { continue };
                hints = hints.push(
                    row![container(self.chord_caps(&k, false, true)).width(110).align_x(iced::Alignment::End), text(what).size(12).color(th.dim)]
                        .spacing(14)
                        .align_y(iced::Alignment::Center),
                );
            }
            col = col.push(space().height(24)).push(hints);
        } else if let Some(c) = &self.connecting {
            col = col.push(space().height(24)).push(text(c.status.clone()).size(12).color(th.dim));
            if let Some(f) = c.fraction {
                col = col.push(space().height(8)).push(container(self.progress(f)).width(320));
            }
        } else if let Some((label, e)) = &self.conn_error {
            let hint = if self.ssh.open { String::new() } else { " — the SSH panel can test and fix it".to_owned() };
            col = col.push(space().height(24)).push(
                container(text(format!("couldn't connect to {label}: {e}{hint}")).size(12).color(th.warn)).max_width(560),
            );
        } else if self.disconnected {
            col = col.push(space().height(24)).push(text(format!("disconnected from {}", self.label)).size(12).color(th.warn));
        }
        container(col).center(Length::Fill).into()
    }

    /// A pane with a focus halo (border drawn inside the padding so layout never shifts).
    fn pane<'a>(&self, content: Element<'a, Message>, focused: bool) -> container::Container<'a, Message> {
        let c = if focused { self.theme.accent } else { Color::TRANSPARENT };
        container(content).padding(2).width(Length::Fill).height(Length::Fill).style(move |_| container::Style {
            border: Border { color: c, width: 2.0, radius: 3.0.into() },
            ..Default::default()
        })
    }

    fn vrule(&self) -> Element<'_, Message> {
        let c = self.theme.divider;
        container(space()).width(1).height(Length::Fill).style(move |_| container::Style::default().background(c)).into()
    }
    fn hrule(&self) -> Element<'_, Message> {
        let c = self.theme.divider;
        container(space()).height(1).width(Length::Fill).style(move |_| container::Style::default().background(c)).into()
    }

    fn tab_bar(&self) -> Element<'_, Message> {
        let th = &self.theme;
        // The keys are in the bar: the tab in front is filled in, like a selected row.
        let in_bar = self.focus == Focus::Tabs;
        let tabs = self.tabs.iter().enumerate().map(|(i, t)| {
            let name = t.doc.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // The tab a followed agent opened.
            let name = if t.diff { "± diff".to_string() } else if let Some(page) = t.web { self.web_title(page) } else { name };
            let name = if t.doc.path.as_deref().is_some_and(|p| self.ai_follow_tab(p)) { format!("◉ {name}") } else { name };
            let active = i == self.active && !self.ai.shown();
            let fg = if active { th.foreground } else { th.dim };
            let dot = if t.doc.is_modified() { "●" } else { "×" };
            let underline = if active { th.accent } else { Color::TRANSPARENT };
            let held = (active && in_bar).then_some(iced::Background::Color(th.browser_sel_bg));
            let label = row![
                text(name).size(12).color(fg).font(crate::fonts::mono()),
                button(text(dot).size(11).color(th.dim)).on_press(Message::CloseTabAt(i)).padding([0, 4]).style(|_, _| button::Style::default()),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            let b = button(label).on_press(Message::SelectTab(i)).padding([5, 12]).style(move |_, _| button::Style {
                background: held,
                border: Border { color: underline, width: 0.0, radius: 0.0.into() },
                ..Default::default()
            });
            column![b, container(space()).height(2).width(Length::Fill).style(move |_| container::Style::default().background(underline))]
                .width(Length::Shrink)
                .into()
        });
        let mut bar = row![row(tabs)].align_y(iced::Alignment::Center);
        // The agents tab: last, and always the same one.
        if self.ai.open {
            let active = self.ai.shown() && !self.ai_history_front();
            let fg = if active { th.foreground } else { th.dim };
            let underline = if active { th.accent } else { Color::TRANSPARENT };
            let label = row![
                text(self.ai_tab_title()).size(12).color(fg).font(crate::fonts::mono()),
                button(text("×").size(11).color(th.dim)).on_press(Message::Ai(ai::Msg::Close)).padding([0, 4]).style(|_, _| button::Style::default()),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            let held = (active && in_bar).then_some(iced::Background::Color(th.browser_sel_bg));
            let b = button(label)
                .on_press(Message::Ai(ai::Msg::Front))
                .padding([5, 12])
                .style(move |_, _| button::Style { background: held, ..Default::default() });
            bar = bar.push(column![b, container(space()).height(2).width(Length::Fill).style(move |_| container::Style::default().background(underline))].width(Length::Shrink));
        }
        // The history tab, when open: after the agents.
        if self.ai.open && self.ai.history_open() {
            let active = self.ai.shown() && self.ai_history_front();
            let fg = if active { th.foreground } else { th.dim };
            let underline = if active { th.accent } else { Color::TRANSPARENT };
            let label = row![
                text("✦ history").size(12).color(fg).font(crate::fonts::mono()),
                button(text("×").size(11).color(th.dim)).on_press(Message::Ai(ai::Msg::HistoryClose)).padding([0, 4]).style(|_, _| button::Style::default()),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            let held = (active && in_bar).then_some(iced::Background::Color(th.browser_sel_bg));
            let b = button(label)
                .on_press(Message::Ai(ai::Msg::History))
                .padding([5, 12])
                .style(move |_, _| button::Style { background: held, ..Default::default() });
            bar = bar.push(column![b, container(space()).height(2).width(Length::Fill).style(move |_| container::Style::default().background(underline))].width(Length::Shrink));
        }
        bar = bar.push(space::horizontal());
        if let Some(b) = self.preview_button() {
            bar = bar.push(b);
        }
        column![bar, self.hrule()].into()
    }

    fn hint<'a>(&self, key: &str, what: &'a str) -> Element<'a, Message> {
        let th = &self.theme;
        let dim = th.dim;
        let cap = container(text(key.to_string()).size(11).font(crate::fonts::mono()).color(th.foreground))
            .padding([1, 6])
            .style(move |_| container::Style {
                border: Border { color: dim, width: 1.0, radius: 3.0.into() },
                ..Default::default()
            });
        row![cap, text(what).size(11).color(th.dim)].spacing(6).align_y(iced::Alignment::Center).into()
    }

    /// "label (chord)" with the command's current binding, or just the label if it has none.
    fn tip_with_key(&self, label: &str, cmd: Command) -> String {
        match self.keymap.chords_for(cmd).first() {
            Some(c) => format!("{label} ({c})"),
            None => label.to_owned(),
        }
    }

    /// Toolbar buttons in order: icon, tooltip, what pressing it sends.
    fn toolbar_items(&self) -> Vec<(&svg::Handle, String, Message)> {
        let i = &self.icons;
        let gear_on = self.settings.menu || self.settings.page.is_some();
        vec![
            (
                if self.tree.open { &i.folder_on } else { &i.folder },
                format!("File tree ({MOD}B)"),
                Message::Control(Command::ToggleExplorer),
            ),
            (if self.ai.shown() { &i.network_on } else { &i.network }, self.tip_with_key("Agents", Command::ToggleAi), Message::Control(Command::ToggleAi)),
            (
                if self.terms_visible { &i.terminal_on } else { &i.terminal },
                "Terminals: show / hide (Ctrl+` H) · new: Ctrl+`".into(),
                Message::Control(Command::ToggleTerminal),
            ),
            (
                // Lit while the panel is open or any app runs.
                if self.apps.open || self.serve.list.iter().any(|a| a.running()) { &i.apps_on } else { &i.apps },
                self.tip_with_key("Apps: run and open this project's apps", Command::ToggleApps),
                Message::Control(Command::ToggleApps),
            ),
            (if self.git_ui.open { &i.git_on } else { &i.git }, self.tip_with_key("Git", Command::ToggleGit), Message::Control(Command::ToggleGit)),
            (if self.diff_active() { &i.diff_on } else { &i.diff }, self.tip_with_key("Diff: what changed", Command::ToggleDiff), Message::Control(Command::ToggleDiff)),
            (if self.ssh.open { &i.remote_on } else { &i.remote }, self.tip_with_key("SSH: connections", Command::ToggleSsh), Message::Control(Command::ToggleSsh)),
            (if gear_on { &i.gear_on } else { &i.gear }, "Settings".into(), Message::Settings(settings::Msg::ToggleMenu)),
        ]
    }

    fn control_strip(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let accent = th.accent;
        let ctl = |icon: &svg::Handle, tip: String, msg: Message, selected: bool| -> Element<'_, Message> {
            let b = button(svg(icon.clone()).width(21.6).height(21.6))
                .on_press(msg)
                .padding([6.0, 9.6])
                .style(move |_, st| button::Style {
                    background: matches!(st, button::Status::Hovered).then(|| Color::from_rgb8(0x1a, 0x1a, 0x1a).into()),
                    border: Border {
                        radius: 4.8.into(),
                        width: if selected { 2.0 } else { 0.0 },
                        color: if selected { accent } else { Color::TRANSPARENT },
                    },
                    ..Default::default()
                });
            tooltip(b, container(text(tip).size(13.2).color(th.foreground)).padding([3.6, 7.2]).style(|_| {
                container::Style::default().background(Color::from_rgb8(0x1a, 0x1a, 0x1a))
            }), tooltip::Position::Top)
            .into()
        };
        let in_toolbar = self.focus == Focus::Toolbar;
        let buttons = row(self
            .toolbar_items()
            .into_iter()
            .enumerate()
            .map(|(n, (icon, tip, msg))| ctl(icon, tip, msg, in_toolbar && n == self.toolbar_sel)))
        .spacing(2.4);

        let mut status = row![].spacing(19.2).align_y(iced::Alignment::Center);
        if let Some((f, warn, _)) = &self.flash {
            status = status.push(text(f.clone()).size(14.4).wrapping(text::Wrapping::None).color(if *warn { th.warn } else { th.foreground }));
        }
        if let Some(s) = self.passive_status() {
            status = status.push(text(s).size(14.4).wrapping(text::Wrapping::None).color(th.dim));
        }
        // The agents, whatever tab is in front: how many are at work, and the way to stop them.
        if let Some(agents) = self.ai_strip() {
            status = status.push(agents);
        }
        if self.editing() {
            if let Some(d) = self.doc() {
                let (l, c) = d.cursor_line_col();
                let n: usize = d.selection.ranges().iter().map(|r| r.len()).sum();
                let cursors = d.selection.len();
                if cursors > 1 {
                    status = status.push(text(format!("{cursors} cursors")).size(14.4).wrapping(text::Wrapping::None).color(th.accent));
                }
                if n > 0 {
                    status = status.push(text(format!("{n} selected")).size(14.4).wrapping(text::Wrapping::None).color(th.dim));
                }
                status = status.push(text(format!("Ln {}, Col {}", l + 1, c + 1)).size(14.4).wrapping(text::Wrapping::None).color(th.dim));
                status = status.push(text(d.language_name()).size(14.4).wrapping(text::Wrapping::None).color(th.dim));
            }
        }
        if self.label != "local" {
            status = status.push(text(self.label.clone()).size(14.4).wrapping(text::Wrapping::None).color(th.dim));
        }
        let strip = row![buttons].spacing(14.4).align_y(iced::Alignment::Center);
        // The git line takes what room is left and gives way to the status on the right: cut to
        // fit, ending in "…" (iced can't ellipsize). The strip's font is monospace (~0.6 em per
        // character); the clip catches a font that runs wider than that.
        let git = self.git_status().map(|(s, warn)| {
            let color = if warn { th.warn } else { th.dim };
            let line = iced::widget::responsive(move |room| {
                let fit = (room.width / (14.4 * 0.6)).floor() as usize;
                let s = if fit == 0 { String::new() } else { git::ellipsize(&s, fit) };
                text(s).size(14.4).color(color).wrapping(text::Wrapping::None).into()
            });
            // Responsive fills both ways; a fixed height keeps the strip one line tall.
            container(line).width(Length::Fill).height(19).clip(true)
        });
        // A small signature mark at the far right; hover for the version.
        let (w, h) = crate::icons::LOGO_SMALL_SIZE;
        let mark = tooltip(
            svg(self.icons.logo_small.clone()).width(w).height(h),
            container(text(format!("nib {}", env!("CARGO_PKG_VERSION"))).size(13.2).color(th.foreground))
                .padding([3.6, 7.2])
                .style(|_| container::Style::default().background(Color::from_rgb8(0x1a, 0x1a, 0x1a))),
            tooltip::Position::Top,
        );
        // The git line is the spacer when there is one.
        let strip = match git {
            Some(g) => strip.push(g),
            None => strip.push(space::horizontal()),
        };
        let strip = strip.push(status).push(mark);
        mouse_area(container(strip).padding([2.4, 9.6]).width(Length::Fill)).into()
    }
}

pub(crate) fn async_sleep(d: Duration) -> impl std::future::Future<Output = ()> {
    let (tx, rx) = iced::futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(d);
        let _ = tx.send(());
    });
    async move {
        let _ = rx.await;
    }
}

pub fn to_chord(key: &IKey, physical: keyboard::key::Physical, m: keyboard::Modifiers) -> Option<KeyChord> {
    let mut shift = m.shift();
    let k = match key {
        IKey::Named(n) => match n {
            Named::ArrowUp => Key::Up,
            Named::ArrowDown => Key::Down,
            Named::ArrowLeft => Key::Left,
            Named::ArrowRight => Key::Right,
            Named::Home => Key::Home,
            Named::End => Key::End,
            Named::PageUp => Key::PageUp,
            Named::PageDown => Key::PageDown,
            Named::Tab => Key::Tab,
            Named::Enter => Key::Enter,
            Named::Backspace => Key::Backspace,
            Named::Delete => Key::Delete,
            Named::Insert => Key::Insert,
            Named::Escape => Key::Esc,
            Named::Space => Key::Char(' '),
            Named::F1 => Key::F(1),
            Named::F2 => Key::F(2),
            Named::F3 => Key::F(3),
            Named::F4 => Key::F(4),
            Named::F5 => Key::F(5),
            Named::F6 => Key::F(6),
            Named::F7 => Key::F(7),
            Named::F8 => Key::F(8),
            Named::F9 => Key::F(9),
            Named::F10 => Key::F(10),
            Named::F11 => Key::F(11),
            Named::F12 => Key::F(12),
            _ => return None,
        },
        // The key left of 1 is always "`" for shortcuts, even shifted ("~") or on other layouts.
        IKey::Character(_) if physical == keyboard::key::Physical::Code(keyboard::key::Code::Backquote) => Key::Char('`'),
        // With Ctrl/Cmd, =, - and digits by position: Ctrl+"+" is Ctrl+Shift+=, Cmd+1 on AZERTY.
        IKey::Character(_) if (m.control() || m.logo()) && shortcut_key(physical).is_some() => shortcut_key(physical)?,
        IKey::Character(_) => {
            // Layout-independent Latin key for shortcuts (Cmd+Z on any layout).
            let c = key.to_latin(physical)?;
            if !c.is_alphabetic() && !(m.control() || m.logo() || m.alt()) {
                shift = false;
            }
            Key::Char(c.to_ascii_lowercase())
        }
        IKey::Unidentified => return None,
    };
    Some(KeyChord { ctrl: m.control(), alt: m.alt(), shift, logo: m.logo(), key: k })
}

fn shortcut_key(physical: keyboard::key::Physical) -> Option<Key> {
    use keyboard::key::Code;
    let keyboard::key::Physical::Code(code) = physical else { return None };
    Some(Key::Char(match code {
        Code::Equal => '=',
        Code::Minus => '-',
        Code::Digit1 => '1',
        Code::Digit2 => '2',
        Code::Digit3 => '3',
        Code::Digit4 => '4',
        Code::Digit5 => '5',
        Code::Digit6 => '6',
        Code::Digit7 => '7',
        Code::Digit8 => '8',
        Code::Digit9 => '9',
        _ => return None,
    }))
}

/// Pane focus and terminal gestures, driven through `App::update` with raw key-down / key-up
/// sequences (incl. auto-repeat, odd release orders, lost releases).
#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn app() -> App {
        let (theme, _) = Theme::from_config(&Default::default());
        let init = Init {
            conn: Some(crate::transport::sink()),
            keymap: Keymap::default(),
            theme,
            warnings: vec![],
            tab_width: 4,
            font_size: 13.0,
            font: None,
            line_height: 1.4,
            beginner_mode: false,
            theme_cfg: Default::default(),
            lsp: vec![],
            ai: Default::default(),
            launch: crate::Launch {
                target: nib_editor::Target::Local { nibd: None },
                root: "/tmp".into(),
                open: None,
                label: "local".into(),
                profile: None,
            },
            bench: false,
            headless: false,
        };
        let mut a = App::boot(init).0;
        // As a daemon with only the built-in python server would say in its Hello.
        a.code.lsp_extensions = ["py", "pyi"].map(String::from).into();
        a
    }

    fn chord(s: &str) -> KeyChord {
        KeyChord::parse(s).unwrap()
    }
    fn down(a: &mut App, s: &str) {
        let _ = a.update(Message::Key { chord: chord(s), text: None, repeat: false });
    }
    fn repeat(a: &mut App, s: &str) {
        let _ = a.update(Message::Key { chord: chord(s), text: None, repeat: true });
    }
    fn up(a: &mut App, key: &str) {
        let _ = a.update(Message::KeyUp(chord(key).key));
    }
    fn tap(a: &mut App, s: &str) {
        down(a, s);
        up(a, s.rsplit('+').next().unwrap());
    }
    /// Answer every outstanding PtyOpen like the daemon would.
    fn answer_ptys(a: &mut App) {
        let ids: Vec<ReqId> =
            a.pending.iter().filter(|(_, p)| matches!(p, Pending::PtyOpen { .. })).map(|(id, _)| *id).collect();
        for id in ids {
            let _ = a.on_server(ServerMsg::Response { id, result: Ok(Response::PtyOpened { id: 100 + id as PtyId }) });
        }
    }
    /// App with `n` terminals open (focus on the last one).
    fn with_terms(n: usize) -> App {
        let mut a = app();
        for _ in 0..n {
            tap(&mut a, "ctrl+shift+`");
        }
        answer_ptys(&mut a);
        assert_eq!(a.terms.len(), n);
        a
    }
    fn term(a: &App, i: usize) -> Focus {
        Focus::Terminal(a.terms[i].key)
    }

    /// Next pane `n` times (previous with `back`).
    fn cycle(a: &mut App, n: usize, back: bool) {
        for _ in 0..n {
            tap(a, if back { "shift+f6" } else { "f6" });
        }
    }
    fn open_file(a: &mut App, name: &str) {
        let doc = Document::new(Default::default(), Some(PathBuf::from(format!("/tmp/{name}"))));
        a.put_doc(doc, true);
    }

    #[test]
    fn every_tap_opens_a_new_terminal() {
        let mut a = app();
        tap(&mut a, "ctrl+`");
        assert_eq!(a.terms.len(), 1);
        assert_eq!(a.focus, term(&a, 0));
        tap(&mut a, "ctrl+`");
        tap(&mut a, "ctrl+shift+`"); // "Ctrl+~"
        assert_eq!(a.terms.len(), 3);
        assert_eq!(a.focus, term(&a, 2), "focus goes to the new one");
    }

    #[test]
    fn cycling_with_only_the_editor_never_opens_a_terminal() {
        let mut a = app(); // ring = [Main, Toolbar]
        cycle(&mut a, 1, false);
        assert_eq!(a.focus, Focus::Toolbar);
        cycle(&mut a, 1, false);
        assert_eq!(a.focus, Focus::Main);
        cycle(&mut a, 3, true);
        assert_eq!(a.focus, Focus::Toolbar);
        cycle(&mut a, 1, true);
        assert!(a.terms.is_empty(), "must not open a terminal");
        assert_eq!(a.focus, Focus::Main);
        assert!(a.leader.is_none());
    }

    #[test]
    fn ctrl_tab_walks_the_panes_when_there_are_no_files_to_switch() {
        let mut a = with_terms(2); // focus: B; ring = [Main, A, B, Toolbar]
        for want in [Focus::Toolbar, Focus::Main, term(&a, 0)] {
            tap(&mut a, "ctrl+tab");
            assert_eq!(a.focus, want);
        }
        for want in [Focus::Main, Focus::Toolbar, term(&a, 1)] {
            tap(&mut a, "ctrl+shift+tab");
            assert_eq!(a.focus, want);
        }
        // One file is still nothing to switch between.
        open_file(&mut a, "x.py");
        assert_eq!(a.focus, Focus::Main);
        tap(&mut a, "ctrl+tab");
        assert_eq!(a.focus, term(&a, 0));
        assert_eq!(a.terms.len(), 2, "never opens a terminal");
    }

    #[test]
    fn closing_the_last_file_shows_the_agents_tab_when_it_is_open() {
        let mut a = app();
        let _ = a.update(Message::Control(Command::ToggleAi));
        open_file(&mut a, "x.py");
        a.ai_sync();
        assert!(a.ai.open && !a.ai.shown(), "the file is in front of the agents");
        let _ = a.update(Message::Control(Command::CloseTab));
        assert!(a.tabs.is_empty() && a.ai.shown(), "not the start page");
        assert_eq!(a.focus, Focus::Ai);
        // With no agents tab, the last file closing goes back to choosing a file.
        let mut a = app();
        open_file(&mut a, "x.py");
        let _ = a.update(Message::Control(Command::CloseTab));
        assert_eq!((a.ai.shown(), a.focus), (false, Focus::Tree));
    }

    #[test]
    fn the_pane_keys_stop_on_the_tab_bar_where_arrows_pick_a_tab() {
        let mut a = app();
        open_file(&mut a, "x.py");
        tap(&mut a, "f6");
        assert_eq!(a.focus, Focus::Toolbar, "one tab is nothing to choose between");
        tap(&mut a, "f6");
        let _ = a.update(Message::Control(Command::ToggleAi));
        open_file(&mut a, "y.py");
        a.focus = Focus::Main;
        a.ai_sync(); // tabs: x.py, y.py (in front), agents
        tap(&mut a, "shift+f6");
        assert_eq!(a.focus, Focus::Tabs);
        tap(&mut a, "right");
        assert!(a.ai.shown(), "the agents tab shows at once");
        assert_eq!(a.focus, Focus::Tabs, "and the keys stay in the bar");
        tap(&mut a, "right");
        assert_eq!((a.active, a.ai.shown(), a.focus), (0, false, Focus::Tabs), "wraps");
        tap(&mut a, "ctrl+shift+tab");
        assert!(a.ai.shown());
        tap(&mut a, "left");
        assert_eq!((a.active, a.ai.shown()), (1, false));
        tap(&mut a, "enter");
        assert_eq!(a.focus, Focus::Main, "into the file");
        tap(&mut a, "shift+f6");
        tap(&mut a, "right");
        tap(&mut a, "down");
        assert_eq!(a.focus, Focus::Ai, "into the agents");
        // Close acts on the tab shown; with one left the bar hands the keys to it.
        tap(&mut a, "shift+f6");
        let _ = a.update(Message::Control(Command::CloseTab));
        assert!(!a.ai.open && a.tabs.len() == 2 && a.focus == Focus::Tabs, "the agents tab closed, whole");
        let _ = a.update(Message::Control(Command::CloseTab));
        assert_eq!((a.tabs.len(), a.focus), (1, Focus::Main));
    }

    #[test]
    fn ctrl_tab_switches_files_in_the_editor_and_the_pane_keys_leave_it() {
        let mut a = with_terms(2);
        open_file(&mut a, "x.py");
        open_file(&mut a, "y.py"); // focus: Main, ring = [Main, A, B, Toolbar]
        assert_eq!(a.active, 1);
        tap(&mut a, "ctrl+tab");
        assert_eq!((a.active, a.focus), (0, Focus::Main), "files, not panes");
        tap(&mut a, "ctrl+shift+tab");
        assert_eq!((a.active, a.focus), (1, Focus::Main));
        // The pane keys always move between panes.
        tap(&mut a, "f6");
        assert_eq!(a.focus, term(&a, 0));
        if cfg!(target_os = "macos") {
            tap(&mut a, "alt+shift+tab");
            assert_eq!(a.focus, Focus::Main);
            tap(&mut a, "alt+tab");
            assert_eq!(a.focus, term(&a, 0));
        }
        // Outside the editor Ctrl+Tab moves on to the next pane, files untouched.
        tap(&mut a, "ctrl+tab");
        assert_eq!((a.active, a.focus), (1, term(&a, 1)));
        assert_eq!(a.terms.len(), 2, "no terminal opened");
    }

    #[test]
    fn the_old_backtick_tab_gesture_never_opens_a_terminal() {
        let mut a = with_terms(1); // focus: A; ring = [Main, A, Toolbar]
        down(&mut a, "ctrl+`");
        down(&mut a, "ctrl+tab"); // ` first: Tab cancels the hold and acts on its own
        up(&mut a, "tab");
        up(&mut a, "`");
        assert_eq!(a.focus, Focus::Toolbar);
        down(&mut a, "ctrl+tab"); // Tab first, ` on top
        down(&mut a, "ctrl+`");
        up(&mut a, "`");
        up(&mut a, "tab");
        assert_eq!(a.focus, Focus::Main);
        assert_eq!(a.terms.len(), 1);
        assert!(a.leader.is_none());
    }

    #[test]
    fn ctrl_tab_switches_editor_tabs_on_press() {
        let mut a = app();
        open_file(&mut a, "x.py");
        open_file(&mut a, "y.py");
        assert_eq!(a.active, 1);
        down(&mut a, "ctrl+tab");
        assert_eq!(a.active, 0);
        up(&mut a, "tab");
        assert_eq!(a.active, 0);
        tap(&mut a, "ctrl+shift+tab");
        assert_eq!(a.active, 1);
        assert!(a.terms.is_empty());
    }

    #[test]
    fn auto_repeat_never_retriggers() {
        let mut a = with_terms(2); // focus: B
        down(&mut a, "ctrl+tab"); // -> Toolbar
        for _ in 0..5 {
            repeat(&mut a, "ctrl+tab");
        }
        up(&mut a, "tab");
        assert_eq!(a.focus, Focus::Toolbar, "moved exactly one step");
        down(&mut a, "f6"); // -> Main
        for _ in 0..5 {
            repeat(&mut a, "f6");
        }
        up(&mut a, "f6");
        assert_eq!(a.focus, Focus::Main);
        assert_eq!(a.terms.len(), 2);
    }

    #[test]
    fn holding_the_terminal_key_opens_exactly_one() {
        let mut a = app();
        down(&mut a, "ctrl+`");
        for _ in 0..10 {
            repeat(&mut a, "ctrl+`");
        }
        up(&mut a, "`");
        assert_eq!(a.terms.len(), 1);
    }

    #[test]
    fn other_key_cancels_hold_and_later_repeats_dont_rearm() {
        let mut a = with_terms(1);
        down(&mut a, "ctrl+`");
        down(&mut a, "ctrl+x");
        repeat(&mut a, "ctrl+`");
        up(&mut a, "x");
        up(&mut a, "`");
        assert_eq!(a.terms.len(), 1, "cancelled hold must not open a terminal on release");
        assert!(a.leader.is_none());
    }

    #[test]
    fn lost_release_is_cleared_on_blur() {
        let mut a = with_terms(2);
        down(&mut a, "ctrl+`");
        let _ = a.update(Message::Blur); // e.g. Cmd+Tab away; the key-up never arrives
        assert!(a.leader.is_none() && a.held.is_empty());
        up(&mut a, "`");
        assert_eq!(a.terms.len(), 2);
    }


    #[test]
    fn plain_tab_never_cycles() {
        let mut a = with_terms(1);
        cycle(&mut a, 1, true); // -> Main
        tap(&mut a, "tab");
        assert_eq!(a.focus, Focus::Main);
    }

    #[test]
    fn closing_before_the_daemon_answers_is_safe() {
        let mut a = app();
        tap(&mut a, "ctrl+`"); // PtyOpen pending
        tap(&mut a, if cfg!(target_os = "macos") { "cmd+w" } else { "ctrl+shift+w" });
        assert!(a.terms.is_empty());
        answer_ptys(&mut a); // late answer: must not resurrect it
        assert!(a.terms.is_empty());
    }

    #[test]
    fn shell_exit_moves_focus_sensibly() {
        let mut a = with_terms(2);
        let first = a.terms[0].id.unwrap();
        let second = a.terms[1].id.unwrap();
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: second, code: Some(0) }));
        assert_eq!(a.focus, term(&a, 0));
        let _ = a.on_server(ServerMsg::Notify(ServerNotify::PtyExit { id: first, code: Some(0) }));
        assert_eq!(a.focus, Focus::Main);
        assert!(!a.terms_visible);
    }

    #[test]
    fn strip_button_hides_and_hidden_terminals_leave_the_ring() {
        let mut a = with_terms(2);
        let _ = a.update(Message::Control(Command::ToggleTerminal));
        assert!(!a.terms_visible);
        assert_eq!(a.focus, Focus::Main);
        cycle(&mut a, 1, false);
        assert_eq!(a.focus, Focus::Toolbar, "hidden terminals are skipped");
        let _ = a.update(Message::Control(Command::ToggleTerminal));
        assert!(a.terms_visible);
        assert_eq!(a.terms.len(), 2);
    }

    #[test]
    fn keys_reach_the_focused_terminal_only() {
        let mut a = app();
        tap(&mut a, "ctrl+`");
        tap(&mut a, "ctrl+`"); // both still waiting for the daemon: input is queued
        let _ = a.update(Message::Key { chord: chord("a"), text: Some("a".into()), repeat: false });
        cycle(&mut a, 3, false); // B -> Toolbar -> Main -> A
        let _ = a.update(Message::Key { chord: chord("b"), text: Some("b".into()), repeat: false });
        assert_eq!(a.terms[0].queued, b"b");
        assert_eq!(a.terms[1].queued, b"a");
    }

    #[test]
    fn toolbar_jump_select_press_and_back() {
        let mut a = with_terms(1); // focus: A
        down(&mut a, "ctrl+`");
        down(&mut a, "ctrl+b");
        up(&mut a, "b");
        up(&mut a, "`");
        assert_eq!(a.focus, Focus::Toolbar);
        assert_eq!(a.terms.len(), 1, "no terminal opened");
        tap(&mut a, "x"); // typing does nothing here
        tap(&mut a, "right");
        tap(&mut a, "right");
        tap(&mut a, "right");
        tap(&mut a, "right");
        tap(&mut a, "right");
        tap(&mut a, "right");
        tap(&mut a, "right");
        tap(&mut a, "right"); // clamps at the last button
        assert_eq!(a.toolbar_sel, 7);
        tap(&mut a, "left");
        tap(&mut a, "left");
        tap(&mut a, "left");
        tap(&mut a, "left");
        tap(&mut a, "left");
        assert_eq!(a.toolbar_sel, 2); // Terminals button
        tap(&mut a, "enter");
        assert!(!a.terms_visible, "pressed: terminals hidden");
        assert_eq!(a.focus, Focus::Main, "the terminal it came from is hidden, so back to main");
        // Esc returns to where we came from
        cycle(&mut a, 1, false);
        assert_eq!(a.focus, Focus::Toolbar);
        tap(&mut a, "esc");
        assert_eq!(a.focus, Focus::Main);
        // B again from the toolbar = back
        cycle(&mut a, 1, false);
        down(&mut a, "ctrl+`");
        down(&mut a, "ctrl+b");
        up(&mut a, "`");
        assert_eq!(a.focus, Focus::Main);
    }

    #[test]
    fn backtick_h_hides_and_shows_terminals() {
        let mut a = with_terms(2); // focus: B
        let gesture = |a: &mut App| {
            down(a, "ctrl+`");
            down(a, "ctrl+h");
            up(a, "h");
            up(a, "`");
        };
        gesture(&mut a);
        assert!(!a.terms_visible);
        assert_eq!(a.focus, Focus::Main);
        assert_eq!(a.terms.len(), 2, "hidden, still running");
        gesture(&mut a);
        assert!(a.terms_visible);
        assert_eq!(a.focus, term(&a, 1), "back to the last-used terminal");
        let mut b = app();
        gesture(&mut b);
        assert!(b.terms.is_empty(), "H never opens a terminal");
    }

    #[test]
    fn go_to_tab_n_and_reopen_closed() {
        let mut a = app();
        for f in ["a.py", "b.py", "c.py"] {
            open_file(&mut a, f);
        }
        tap(&mut a, "mod+1");
        assert_eq!(a.active, 0);
        tap(&mut a, "mod+3");
        assert_eq!(a.active, 2);
        tap(&mut a, "mod+9"); // no 9th tab: nothing
        assert_eq!(a.active, 2);
        tap(&mut a, "mod+w"); // close c.py
        assert_eq!(a.tabs.len(), 2);
        tap(&mut a, "mod+shift+t");
        let reopen = a.pending.values().any(|p| matches!(p, Pending::Open { path, new_tab: true, .. } if path == "/tmp/c.py"));
        assert!(reopen, "asks the daemon for the closed file again");
        tap(&mut a, "mod+1");
        tap(&mut a, "mod+w"); // close a.py
        open_file(&mut a, "a.py"); // reopened by hand meanwhile
        let n = a.tabs.len();
        tap(&mut a, "mod+shift+t");
        assert_eq!(a.tabs.len(), n, "already open: just switches to it");
        assert_eq!(a.tabs[a.active].doc.path.as_deref(), Some(std::path::Path::new("/tmp/a.py")));
    }

    #[test]
    fn tab_numbers_work_from_a_terminal() {
        let mut a = with_terms(1);
        open_file(&mut a, "a.py");
        open_file(&mut a, "b.py");
        cycle(&mut a, 1, false); // Main -> A
        assert_eq!(a.focus, term(&a, 0));
        tap(&mut a, "mod+1");
        assert_eq!(a.active, 0);
        assert!(a.terms[0].queued.is_empty(), "not sent to the shell");
    }

    #[test]
    fn font_size_keys() {
        let mut a = app();
        let s0 = a.face.size;
        tap(&mut a, "mod+=");
        assert_eq!(a.face.size, s0 + 1.0);
        tap(&mut a, "mod+shift+="); // Mod+"+"
        assert_eq!(a.face.size, s0 + 2.0);
        tap(&mut a, "mod+-");
        tap(&mut a, "mod+-");
        tap(&mut a, "mod+-");
        assert_eq!(a.face.size, s0 - 1.0);
        for _ in 0..50 {
            tap(&mut a, "mod+-");
        }
        assert_eq!(a.face.size, 8.0, "clamped");
    }

    /// What winit/iced actually deliver for the new shortcuts maps onto the bound chords.
    #[test]
    fn os_key_events_map_to_bound_chords() {
        use keyboard::key::{Code, Physical};
        use keyboard::Modifiers as M;
        let c = |s: &str, code: Code, m: M| to_chord(&IKey::Character(s.into()), Physical::Code(code), m).unwrap();
        let km = Keymap::default();
        let ctrl_shift = M::CTRL | M::SHIFT;
        let cmd_shift = M::COMMAND | M::SHIFT;
        assert_eq!(km.lookup(&c("+", Code::Equal, cmd_shift)), Some(Command::FontBigger), "Mod+\"+\" (US)");
        assert_eq!(km.lookup(&c("=", Code::Equal, M::COMMAND)), Some(Command::FontBigger));
        assert_eq!(km.lookup(&c("-", Code::Minus, M::COMMAND)), Some(Command::FontSmaller));
        assert_eq!(km.lookup(&c("+", Code::NumpadAdd, M::COMMAND)), Some(Command::FontBigger), "numpad +");
        if cfg!(target_os = "macos") {
            assert_eq!(km.lookup(&c("-", Code::Minus, M::CTRL)), Some(Command::NavigateBack));
            assert_eq!(km.lookup(&c("_", Code::Minus, ctrl_shift)), Some(Command::NavigateForward));
        }
        assert_eq!(km.lookup(&c("~", Code::Backquote, ctrl_shift)), Some(Command::NewTerminal), "Ctrl+~");
        let mod_ = if cfg!(target_os = "macos") { M::LOGO } else { M::CTRL };
        assert_eq!(km.lookup(&c("&", Code::Digit1, mod_)), Some(Command::GoToTab1), "AZERTY 1 key");
        assert_eq!(km.lookup(&c("T", Code::KeyT, mod_ | M::SHIFT)), Some(Command::ReopenClosedTab));
        // without Ctrl/Cmd, Shift+= stays "+" for typing
        assert_eq!(c("+", Code::Equal, M::SHIFT).key, Key::Char('+'));
    }

    fn open_text(a: &mut App, name: &str, text: &str) {
        let doc = Document::new(text.into(), Some(PathBuf::from(format!("/tmp/{name}"))));
        a.put_doc(doc, true);
        // Any message brings derived views up to date.
        let _ = a.update(Message::Tick);
    }
    fn type_str(a: &mut App, text: &str) {
        for c in text.chars() {
            let _ = a.update(Message::Key { chord: KeyChord::new(Key::Char(c)), text: Some(c.to_string()), repeat: false });
        }
    }

    #[test]
    fn csv_opens_as_a_sortable_table_and_toggles_to_text() {
        let mut a = app();
        open_text(&mut a, "data.csv", "name,qty\nb,10\na,9\nc,100\n");
        let first = |a: &App| a.tab().unwrap().table.as_ref().unwrap().row(0).unwrap().1[0].clone();
        assert!(a.tab().unwrap().passive() && !a.editing());
        assert_eq!(first(&a), "b");
        assert_eq!(a.passive_status().unwrap(), "3 rows × 2 columns");

        let _ = a.update(Message::Table(TableEvent::Sort(1)));
        assert_eq!(first(&a), "a");
        let _ = a.update(Message::Table(TableEvent::Sort(1)));
        assert_eq!(first(&a), "c");
        assert_eq!(a.passive_status().unwrap(), "3 rows × 2 columns · sorted by qty ↓");

        // Typing does nothing to a table.
        type_str(&mut a, "zzz");
        assert!(!a.doc().unwrap().is_modified());

        // Text mode edits the file; back in the table the edit shows and the sort is kept.
        let _ = a.update(Message::Control(Command::TogglePreview));
        assert!(a.editing());
        type_str(&mut a, "x");
        assert!(a.doc().unwrap().text.to_string().starts_with("xname,qty"));
        let _ = a.update(Message::Control(Command::TogglePreview));
        let t = a.tab().unwrap().table.as_ref().unwrap();
        assert_eq!(t.header[0], "xname");
        assert_eq!(t.sort.map(|s| (s.col, s.descending)), Some((1, true)));
    }

    #[test]
    fn preview_zooms_apart_from_the_code_font() {
        use crate::zoom::Zone;
        let mut a = app();
        open_text(&mut a, "a.md", "# Title\n");
        let size = a.face.size;
        // Not in preview: the keys and the wheel change the code font, as before.
        let _ = a.update(Message::Control(Command::FontBigger));
        assert_eq!((a.face.size, a.preview_zoom), (size + 1.0, 1.0));
        let _ = a.update(Message::WheelZoom(None, -1));
        assert_eq!(a.face.size, size);

        // In preview the keys zoom the preview only; there and back is 100% again.
        let _ = a.update(Message::Control(Command::TogglePreview));
        let _ = a.update(Message::Control(Command::FontBigger));
        assert_eq!(a.face.size, size);
        assert!((a.preview_zoom - 1.1).abs() < 1e-5);
        let _ = a.update(Message::Control(Command::FontSmaller));
        assert_eq!(a.preview_zoom, 1.0);

        // The wheel zooms what is under the pointer: the preview, or (over the editor) the font.
        let _ = a.update(Message::WheelZoom(Some(Zone::Preview), 3));
        assert!((a.preview_zoom - 1.1f32.powi(3)).abs() < 1e-5);
        let _ = a.update(Message::WheelZoom(None, 2));
        assert_eq!(a.face.size, size + 2.0);
        let _ = a.update(Message::WheelZoom(Some(Zone::Preview), -100));
        assert_eq!(a.preview_zoom, 0.5);

        // A terminal with the keys: the code font, even with a preview on screen.
        let _ = a.update(Message::WheelZoom(Some(Zone::Preview), 5));
        a.focus = Focus::Terminal(1);
        let z = a.preview_zoom;
        let _ = a.update(Message::Control(Command::FontSmaller));
        assert_eq!((a.face.size, a.preview_zoom), (size + 1.0, z));

        // The CSV table is a rendered view too.
        a.focus = Focus::Main;
        open_text(&mut a, "d.csv", "a,b\n1,2\n");
        let _ = a.update(Message::Control(Command::FontBigger));
        assert!(a.preview_zoom > z && a.face.size == size + 1.0);
    }

    #[test]
    fn table_scrolling_is_clamped() {
        let mut a = app();
        let rows: String = (0..100).map(|i| format!("{i},x\n")).collect();
        open_text(&mut a, "big.csv", &format!("n,v\n{rows}"));
        let _ = a.update(Message::Table(TableEvent::Resized { rows: 10 }));
        tap(&mut a, "pagedown");
        assert_eq!(a.tab().unwrap().top, 9);
        let _ = a.update(Message::Table(TableEvent::Scroll { lines: 1000.0, cols: 1000.0 }));
        assert_eq!(a.tab().unwrap().top, 90);
        let _ = a.update(Message::Table(TableEvent::Scroll { lines: -5000.0, cols: -5000.0 }));
        assert_eq!((a.tab().unwrap().top, a.tab().unwrap().left), (0, 0));
    }

    #[test]
    fn markdown_and_html_preview_follow_edits() {
        for (name, text, typed) in [("a.md", "# Title\n", "new "), ("a.html", "<h1>Title</h1>\n", "<p>new</p>")] {
            let mut a = app();
            open_text(&mut a, name, text);
            assert!(a.tab().unwrap().preview.is_none() && a.preview_button().is_some());
            let _ = a.update(Message::Control(Command::TogglePreview));
            let version = |a: &App| a.tab().unwrap().preview.as_ref().map(|p| p.version);
            let v0 = version(&a).expect("preview opens");
            // Still an editor: typing changes the text and the preview is re-rendered at once.
            assert!(a.editing());
            type_str(&mut a, typed);
            assert!(a.doc().unwrap().is_modified());
            assert!(version(&a).unwrap() > v0, "{name}");
            assert_eq!(version(&a), Some(a.doc().unwrap().version()));
            let _ = a.update(Message::Control(Command::TogglePreview));
            assert!(!a.tab().unwrap().rendered);
        }
        // No preview for plain code.
        let mut a = app();
        open_text(&mut a, "a.py", "x = 1\n");
        assert!(a.preview_button().is_none());
        let _ = a.update(Message::Control(Command::TogglePreview));
        assert!(!a.tab().unwrap().rendered);
    }

    #[test]
    fn svg_opens_next_to_its_picture_and_survives_bad_edits() {
        let mut a = app();
        let src = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\"><rect width=\"10\" height=\"10\"/></svg>\n";
        open_text(&mut a, "logo.svg", src);
        let _ = a.refresh_derived();
        let t = a.tab().unwrap();
        assert!(t.rendered && a.editing() && a.preview_button().is_some());
        let p = t.svg.as_ref().expect("picture");
        assert!(p.handle.is_some() && p.error.is_none());
        let good = p.handle.clone();
        // Mid-edit the text doesn't parse: the last good picture stays, with the reason.
        type_str(&mut a, "<");
        let p = a.tab().unwrap().svg.as_ref().unwrap();
        assert_eq!(p.version, a.doc().unwrap().version());
        assert!(p.error.is_some());
        assert_eq!(p.handle, good);
        tap(&mut a, "backspace");
        assert!(a.tab().unwrap().svg.as_ref().unwrap().error.is_none());
        // Back to plain source and again.
        let _ = a.update(Message::Control(Command::TogglePreview));
        assert!(!a.tab().unwrap().rendered);
    }

    #[test]
    fn images_and_videos_open_as_viewers() {
        let mut a = app();
        a.connected = true;
        // A video is never read from the daemon.
        let pending = a.pending.len();
        let _ = a.open_path("clip.mp4".into(), true);
        assert_eq!(a.pending.len(), pending);
        assert!(matches!(a.tab().unwrap().media, Some(Media::Unsupported("Video"))));
        assert_eq!(a.doc().unwrap().path.as_deref(), Some(std::path::Path::new("/tmp/clip.mp4")));

        // An image is requested, shown as loading, then decoded.
        let _ = a.open_path("/tmp/pic.png".into(), true);
        let (&id, _) = a.pending.iter().find(|(_, p)| matches!(p, Pending::Open { .. })).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(3, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let data = png.into_inner();
        let _ = a.update(Message::Server(ServerMsg::Response {
            id,
            result: Ok(Response::File { path: "/tmp/pic.png".into(), data: data.clone(), mtime_ns: 0 }),
        }));
        let tab = a.tab().unwrap();
        assert!(matches!(tab.media, Some(Media::Loading)) && tab.doc.read_only);
        let id = tab.id;
        let _ = a.update(Message::MediaDecoded(id, crate::media::decode(&data).map(Arc::new)));
        assert_eq!(a.passive_status().unwrap(), format!("3×2 · PNG · {} B", data.len()));
        // Keys never edit it, and it can't be saved over.
        type_str(&mut a, "abc ");
        assert_eq!(a.doc().unwrap().text.len_chars(), 0);
        assert_eq!(a.tabs.len(), 2);
        // Opening it again switches to the tab instead of loading twice.
        a.active = 0;
        let _ = a.open_path("/tmp/pic.png".into(), true);
        assert_eq!((a.active, a.tabs.len()), (1, 2));
    }

    #[test]
    fn pdfs_open_scroll_zoom_and_follow_links() {
        use crate::pdf::{self, Msg};
        let mut a = app();
        a.connected = true;
        let _ = a.open_path("/tmp/paper.pdf".into(), true);
        let (&id, _) = a.pending.iter().find(|(_, p)| matches!(p, Pending::Open { .. })).unwrap();
        let data = pdf::tests::sample();
        let _ = a.update(Message::Server(ServerMsg::Response {
            id,
            result: Ok(Response::File { path: "/tmp/paper.pdf".into(), data: data.clone(), mtime_ns: 0 }),
        }));
        let tab_id = a.tab().unwrap().id;
        assert!(matches!(a.tab().unwrap().media, Some(Media::Loading)));
        let _ = a.update(Message::Pdf(Msg::Loaded(tab_id, pdf::load(data).map(Arc::new))));
        assert!(a.pdf_active() && a.tab().unwrap().passive());
        let _ = a.update(Message::Pdf(Msg::Resized(iced::Size::new(246.0, 200.0))));
        let pdf = |a: &App| a.tab().unwrap().media.as_ref().unwrap().pdf().unwrap().state_json();
        assert_eq!((pdf(&a)["page"].clone(), pdf(&a)["pages"].clone(), pdf(&a)["zoom"].clone()), (1.into(), 2.into(), 1.0.into()));
        assert!(a.passive_status().unwrap().starts_with("page 1 / 2 · 100% (fit width)"));

        // Keys scroll and zoom; nothing is typed into the placeholder document.
        type_str(&mut a, "abc");
        assert_eq!(a.doc().unwrap().text.len_chars(), 0);
        let _ = a.update(Message::Control(Command::MoveDocEnd));
        assert_eq!(pdf(&a)["page"], 2);
        let _ = a.update(Message::Control(Command::FontBigger));
        assert_eq!((pdf(&a)["zoom"].clone(), pdf(&a)["fit_width"].clone()), (1.25.into(), false.into()));
        let _ = a.update(Message::Pdf(Msg::Fit));
        assert_eq!(pdf(&a)["fit_width"], true);

        // Clicking the link on page 1 goes to its destination on page 2.
        let _ = a.update(Message::Control(Command::MoveDocStart));
        assert_eq!(pdf(&a)["page"], 1);
        let _ = a.update(Message::Pdf(Msg::Hover(0, iced::Point::new(50.0, 70.0))));
        assert!(a.passive_status().unwrap().ends_with("→ page 2"));
        let _ = a.update(Message::Pdf(Msg::Click));
        assert_eq!(pdf(&a)["page"], 2);
        // The outline toggles with the preview key.
        assert_eq!(pdf(&a)["outline_open"], true);
        let _ = a.update(Message::Control(Command::TogglePreview));
        assert_eq!(pdf(&a)["outline_open"], false);
    }

    // --- language features (daemon answers are scripted) ---------------------------------------

    /// The single pending language-server query: (request id, op).
    fn lsp_pending(a: &App) -> (ReqId, LspOp) {
        let v: Vec<_> = a
            .pending
            .iter()
            .filter_map(|(id, p)| match p {
                Pending::Lsp { op, .. } => Some((*id, op.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(v.len(), 1, "one pending query");
        v[0].clone()
    }
    fn answer(a: &mut App, id: ReqId, r: Response) {
        let _ = a.update(Message::Server(ServerMsg::Response { id, result: Ok(r) }));
    }
    fn loc(path: &str, line: u32, c0: u32, c1: u32, text: &str) -> nib_proto::Location {
        nib_proto::Location {
            path: path.into(),
            start: Pos { line, col: c0 },
            end: Pos { line, col: c1 },
            line_text: text.into(),
        }
    }

    #[test]
    fn hover_shows_docs_while_the_mouse_stays_on_the_symbol() {
        let mut a = app();
        open_text(&mut a, "main.py", "x = greet(1)\n\nprint(x)\n");
        let r = iced::Rectangle { x: 0.0, y: 0.0, width: 8.0, height: 16.0 };
        let area = iced::Rectangle { x: 0.0, y: 0.0, width: 800.0, height: 600.0 };
        let hover = |a: &mut App, line, col| {
            let _ = a.update(Message::View(ViewEvent::Hover { line, col, cell: r, area }));
        };
        let due = |a: &mut App| {
            let seq = a.hover_seq();
            let _ = a.update(Message::HoverDue(seq));
        };
        // Blank space and punctuation ask nothing.
        hover(&mut a, 0, 1);
        due(&mut a);
        hover(&mut a, 0, 9);
        due(&mut a);
        assert!(!a.pending.values().any(|p| matches!(p, Pending::Lsp { .. })));
        // Resting on `greet` asks once the delay is over; a stale timer does nothing.
        hover(&mut a, 0, 6);
        let stale = a.hover_seq();
        hover(&mut a, 0, 7);
        let _ = a.update(Message::HoverDue(stale));
        assert!(!a.pending.values().any(|p| matches!(p, Pending::Lsp { .. })));
        due(&mut a);
        let (id, op) = lsp_pending(&a);
        assert_eq!(op, LspOp::Hover);
        let span = (Pos { line: 0, col: 4 }, Pos { line: 0, col: 9 });
        let info = nib_proto::HoverInfo { markdown: "```python\ndef greet(n)\n```".into(), start: span.0, end: span.1 };
        answer(&mut a, id, Response::Hover(Some(info.clone())));
        assert_eq!(a.hover_shown(), Some(span));
        // Elsewhere on the same symbol: stays, nothing new asked.
        hover(&mut a, 0, 4);
        due(&mut a);
        assert_eq!(a.hover_shown(), Some(span));
        assert!(!a.pending.values().any(|p| matches!(p, Pending::Lsp { .. })));
        // Off the text: closes once the linger is over.
        let _ = a.update(Message::View(ViewEvent::HoverEnd));
        assert_eq!(a.hover_shown(), Some(span));
        due(&mut a);
        assert_eq!(a.hover_shown(), None);
        // An answer that arrives after the mouse moved on is dropped.
        hover(&mut a, 0, 6);
        due(&mut a);
        let (id, _) = lsp_pending(&a);
        hover(&mut a, 2, 1);
        answer(&mut a, id, Response::Hover(Some(info.clone())));
        assert_eq!(a.hover_shown(), None);
        due(&mut a); // asks about `print`
        let (id, _) = lsp_pending(&a);
        answer(&mut a, id, Response::Hover(None));
        assert_eq!(a.hover_shown(), None);
        // Typing closes it.
        hover(&mut a, 0, 6);
        due(&mut a);
        let (id, _) = lsp_pending(&a);
        answer(&mut a, id, Response::Hover(Some(info)));
        assert_eq!(a.hover_shown(), Some(span));
        tap(&mut a, "right");
        assert_eq!(a.hover_shown(), None);
    }

    #[test]
    fn go_to_definition_jumps_into_another_file_and_back() {
        let mut a = app();
        open_text(&mut a, "main.py", "from lib import f\n\nf()\n");
        let p = a.tabs[0].doc.char_at(2, 0);
        a.tabs[0].doc.set_cursor(p, false);
        tap(&mut a, "f12");
        let (id, op) = lsp_pending(&a);
        assert_eq!(op, LspOp::Definition);
        assert!(a.tabs[0].lsp_synced.is_some(), "buffer synced before the query");
        answer(&mut a, id, Response::Locations(vec![loc("/tmp/lib.py", 1, 4, 5, "def f():")]));
        // lib.py isn't open: it's read, then the cursor lands on the definition.
        let (&rid, _) = a
            .pending
            .iter()
            .find(|(_, p)| matches!(p, Pending::Open { goto: Some(_), .. }))
            .expect("file requested");
        answer(
            &mut a,
            rid,
            Response::File { path: "/tmp/lib.py".into(), data: b"# lib\ndef f():\n    pass\n".to_vec(), mtime_ns: 1 },
        );
        assert_eq!(a.active, 1);
        assert_eq!(a.tabs[1].doc.cursor_line_col(), (1, 4));
        tap(&mut a, if cfg!(target_os = "macos") { "ctrl+-" } else { "alt+left" });
        assert_eq!(a.active, 0);
        assert_eq!(a.tabs[0].doc.cursor_line_col(), (2, 0));
        tap(&mut a, if cfg!(target_os = "macos") { "ctrl+shift+-" } else { "alt+right" });
        assert_eq!(a.active, 1);
    }

    #[test]
    fn references_list_picks_with_keys() {
        let mut a = app();
        open_text(&mut a, "main.py", "x = 1\nprint(x)\nx += 2\n");
        tap(&mut a, "shift+f12");
        let (id, op) = lsp_pending(&a);
        assert_eq!(op, LspOp::References);
        let refs = vec![
            loc("/tmp/main.py", 0, 0, 1, "x = 1"),
            loc("/tmp/main.py", 1, 6, 7, "print(x)"),
            loc("/tmp/main.py", 2, 0, 1, "x += 2"),
        ];
        answer(&mut a, id, Response::Locations(refs));
        assert!(a.code.modal());
        tap(&mut a, "down");
        tap(&mut a, "enter");
        assert!(!a.code.modal());
        assert_eq!(a.tabs[0].doc.cursor_line_col(), (1, 6), "typing keys didn't reach the buffer");
        assert_eq!(a.tabs[0].doc.text, "x = 1\nprint(x)\nx += 2\n");
    }

    #[test]
    fn change_all_occurrences_uses_the_server_then_types_everywhere() {
        let mut a = app();
        open_text(&mut a, "main.py", "val = 1\nprint(val, 'val')\n");
        tap(&mut a, "mod+f2");
        let (id, op) = lsp_pending(&a);
        assert_eq!(op, LspOp::Highlights);
        // The server skips the string; plain text matching wouldn't.
        answer(
            &mut a,
            id,
            Response::Locations(vec![loc("/tmp/main.py", 0, 0, 3, ""), loc("/tmp/main.py", 1, 6, 9, "")]),
        );
        assert_eq!(a.tabs[0].doc.selection.len(), 2);
        type_str(&mut a, "n");
        assert_eq!(a.tabs[0].doc.text, "n = 1\nprint(n, 'val')\n");
        tap(&mut a, "esc");
        assert_eq!(a.tabs[0].doc.selection.len(), 1);
    }

    #[test]
    fn change_all_occurrences_without_a_server_matches_words() {
        let mut a = app();
        open_text(&mut a, "run.sh", "x=1\necho $x $xx\n");
        tap(&mut a, "mod+f2");
        assert!(a.pending.values().all(|p| !matches!(p, Pending::Lsp { .. })), "no server for bash");
        assert_eq!(a.tabs[0].doc.selection.len(), 2);
        // Server unavailable for Python too: same fallback.
        open_text(&mut a, "m.py", "ab = ab + 1\n");
        tap(&mut a, "mod+f2");
        let (id, _) = lsp_pending(&a);
        let err = nib_proto::RpcError::new(ErrorKind::NotFound, "ty not found");
        let _ = a.update(Message::Server(ServerMsg::Response { id, result: Err(err) }));
        assert_eq!(a.tabs[1].doc.selection.len(), 2);
    }

    #[test]
    fn rename_edits_open_buffers_and_sends_the_rest_to_disk() {
        let mut a = app();
        open_text(&mut a, "main.py", "def go(): pass\ngo()\n");
        tap(&mut a, "f2");
        let (id, op) = lsp_pending(&a);
        assert_eq!(op, LspOp::PrepareRename);
        answer(&mut a, id, Response::RenameRange(Some((Pos { line: 0, col: 4 }, Pos { line: 0, col: 6 }, "go".into()))));
        assert!(a.code.modal());
        type_str(&mut a, "run"); // replaces the selected old name
        tap(&mut a, "enter");
        let (id, op) = lsp_pending(&a);
        assert_eq!(op, LspOp::Rename { new_name: "run".into() });
        let edit = |line, c0, c1| nib_proto::TextEdit {
            start: Pos { line, col: c0 },
            end: Pos { line, col: c1 },
            new_text: "run".into(),
        };
        answer(
            &mut a,
            id,
            Response::Edits(vec![
                nib_proto::FileEdit { path: "/tmp/main.py".into(), edits: vec![edit(0, 4, 6), edit(1, 0, 2)] },
                nib_proto::FileEdit { path: "/tmp/other.py".into(), edits: vec![edit(3, 0, 2)] },
            ]),
        );
        assert_eq!(a.tabs[0].doc.text, "def run(): pass\nrun()\n");
        assert!(a.tabs[0].doc.is_modified());
        assert!(a.pending.values().any(|p| matches!(p, Pending::ApplyEdits)));
        tap(&mut a, "mod+z");
        assert_eq!(a.tabs[0].doc.text, "def go(): pass\ngo()\n", "one undo step");
    }

    #[test]
    fn close_shortcut_closes_only_what_has_focus() {
        let close = if cfg!(target_os = "macos") { "cmd+w" } else { "ctrl+shift+w" };
        let mut a = with_terms(2); // focus: the second terminal
        open_file(&mut a, "a.py");
        open_file(&mut a, "b.py");
        let (ka, kb) = (a.terms[0].key, a.terms[1].key);
        a.focus = Focus::Terminal(kb);
        tap(&mut a, close);
        assert_eq!(a.terms.iter().map(|t| t.key).collect::<Vec<_>>(), vec![ka]);
        assert_eq!(a.tabs.len(), 2, "editor tabs untouched");
        assert_eq!(a.focus, Focus::Terminal(ka));
        // Editor focused: closes the tab, terminals untouched.
        a.focus = Focus::Main;
        tap(&mut a, close);
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.terms.len(), 1);
        // File tree focused: hides the tree, not the tab.
        a.tree.open = true;
        a.focus = Focus::Tree;
        tap(&mut a, close);
        assert!(!a.tree.open);
        assert_eq!((a.tabs.len(), a.focus), (1, Focus::Main));
    }

    #[test]
    fn find_walks_the_file_then_tab_searches_the_project() {
        let mut a = app();
        open_text(&mut a, "main.py", "Foo = 1\nfoo(2)\nbar\nfoo\n");
        a.focus = Focus::Main;
        tap(&mut a, "mod+f");
        assert!(a.search.open);
        type_str(&mut a, "foo");
        let sel = |a: &App| a.tabs[0].doc.selection.primary();
        assert_eq!(a.search_json()["matches"], 3, "smart case: lower case matches any case");
        assert_eq!((sel(&a).from(), sel(&a).to()), (0, 3));
        tap(&mut a, "enter");
        assert_eq!(sel(&a).from(), 8);
        tap(&mut a, "shift+enter");
        tap(&mut a, "shift+enter");
        assert_eq!(sel(&a).from(), 19, "around the start to the last match");
        assert_eq!(a.tabs[0].doc.text, "Foo = 1\nfoo(2)\nbar\nfoo\n", "keys didn't reach the buffer");
        // A capital makes it exact.
        tap(&mut a, "mod+backspace");
        type_str(&mut a, "Foo");
        assert_eq!(a.search_json()["matches"], 1);

        // Tab: the project, asked of the daemon (at once from the control socket).
        tap(&mut a, "tab");
        assert_eq!(a.search_json()["scope"], "project");
        let (call, rx) = crate::control::Call::test(serde_json::json!({ "cmd": "search", "scope": "project", "query": "foo" }));
        let _ = a.update(Message::Ctl(call));
        let id = *a.pending.iter().find(|(_, p)| matches!(p, Pending::Search { .. })).unwrap().0;
        let found = nib_proto::SearchResults {
            files: vec!["src/foo.py".into()],
            lines: vec![loc("/tmp/src/bar.py", 4, 0, 3, "foo()")],
            truncated: false,
        };
        answer(&mut a, id, Response::Search(found));
        let reply = rx.try_recv().unwrap();
        assert_eq!(reply["results"]["lines"][0]["line"], 5);
        assert_eq!(reply["results"]["files"][0], "src/foo.py");
        tap(&mut a, "down");
        tap(&mut a, "enter");
        assert!(!a.search.open);
        assert!(a.pending.values().any(|p| matches!(p, Pending::Open { path, goto: Some(_), .. } if path == "/tmp/src/bar.py")));
    }
}
