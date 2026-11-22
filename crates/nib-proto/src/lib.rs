//! Wire protocol between `nib` (client) and `nibd` (daemon).
//!
//! Transport: any byte stream (child stdio of `ssh host nibd`, `wsl.exe ... nibd`, or local `nibd`).
//! Framing: `u32` big-endian payload length, then a msgpack payload (`rmp_serde`, named fields so
//! new optional fields stay backward compatible). See `.claude/PROTOCOL.md` (local notes).

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub mod frame;
pub mod path;
pub mod skills;
pub mod timing;
pub mod web;
pub use frame::{read_frame, write_frame, FrameError, MAX_FRAME};

/// Bumped on any incompatible change. Client and daemon must match exactly (the daemon binary is
/// content-addressed and uploaded by the client, so they are always built from the same tree).
pub const PROTOCOL_VERSION: u32 = 1;

/// Time of day in UTC, `HH:MM:SS.mmmZ`: the stamp on every line nib and nibd write to their logs.
pub fn log_clock() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let day = (ms / 1000) % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", day / 3600, day / 60 % 60, day % 60, ms % 1000)
}

/// How often a client sends `ClientNotify::Ping`. A daemon that hears nothing from its client
/// for a couple of minutes assumes it is gone and exits.
pub const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

/// Printed by the remote bootstrap shell snippet when the content-addressed nibd is missing.
pub const NEED_UPLOAD_MARKER: &str = "NIB_NEED_UPLOAD";

/// Written by nibd on stdout before the first frame. Anything the client sees before it (e.g. a
/// noisy `.bashrc`, or the bootstrap's NEED_UPLOAD line) is not protocol data.
pub const MAGIC: &[u8; 4] = b"NIB\x01";

pub type ReqId = u64;

// ---------------------------------------------------------------------------------------------
// client -> daemon
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClientMsg {
    Request { id: ReqId, req: Request },
    Notify(ClientNotify),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Request {
    /// First message on every connection.
    Hello {
        protocol: u32,
        client_version: String,
        /// Project root as given by the profile/CLI (may contain `~`). Daemon resolves it.
        root: String,
        /// Language servers from the client config (`[lsp.<name>]`), applied over the daemon's
        /// built-in ones.
        #[serde(default)]
        lsp: Vec<LspServer>,
    },
    /// Read a whole file. Path is relative to the root or absolute.
    ReadFile {
        path: String,
    },
    /// Write a whole file atomically (tmp + rename). If `expected_mtime_ns` is set and the file
    /// changed on disk since, the daemon refuses with `ErrorKind::Conflict`.
    WriteFile {
        path: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        expected_mtime_ns: Option<u128>,
    },
    ListDir {
        path: String,
    },
    /// Start a shell (or `cmd`) on a pseudo-terminal in `cwd` (relative to root or absolute).
    PtyOpen {
        cwd: String,
        cmd: Option<Vec<String>>,
        cols: u16,
        rows: u16,
    },
    /// Ask the language server about the symbol at `pos` in `path` (send `DocSync` first if the
    /// buffer changed since the last sync).
    Lsp {
        path: String,
        pos: Pos,
        op: LspOp,
    },
    /// Apply edits to files on disk (a rename's changes to files that aren't open in the client).
    /// Answered with `Ok`.
    ApplyEdits {
        files: Vec<FileEdit>,
    },
    /// What git says about the project root -> `Git`.
    GitInfo,
    /// The newest `log_limit` commits and the uncommitted changes -> `GitDetails`.
    GitDetails {
        log_limit: u32,
    },
    /// Run one git operation at the project root -> `GitRan` (also when git reports failure).
    GitRun {
        op: GitOp,
    },
    /// What changed under the project root, as a parsed `git diff` -> `Diff`. `known`: the
    /// `stamp` of the diff the client already has; when it is still that one, the answer leaves
    /// `files` out (the client asks again and again while its diff view is open).
    Diff {
        scope: DiffScope,
        #[serde(default)]
        known: Option<u64>,
    },
    /// The plan agent `id` recorded (`nib-work plan`, or the one it asked approval for), as
    /// Markdown -> `WorkPlan` (`None`: it has none).
    WorkPlan {
        id: AiId,
    },
    /// Land the branch of agent `id` on the branch it left, as one commit with `message`
    /// (`git merge --squash`), then remove its work tree and branch -> `GitRan`. When it cannot
    /// (the base moved and the two conflict, the project is on another branch, something is
    /// staged), nothing is changed and `output` says why.
    WorkLand {
        id: AiId,
        message: String,
    },
    /// The review of agent `id`'s work: the comments on it, each put back on the lines it is
    /// about where those moved -> `Review`.
    Review {
        id: AiId,
    },
    /// Change that review -> `Review` as it is afterwards (`ReviewOp::Send`: `ReviewSent`).
    ReviewEdit {
        id: AiId,
        op: ReviewOp,
    },
    /// Every file and folder under the project root, with git's view of them -> `Tree`.
    /// `known`: the `stamp` of the listing the client already has; when nothing was added or
    /// removed since, the answer leaves `entries` out.
    Tree {
        known: Option<u64>,
        /// Which directory to list.
        #[serde(default)]
        place: Place,
        /// Ignored folders to list the contents of (otherwise listed without them).
        #[serde(default)]
        open: Vec<String>,
    },
    /// Create, rename or delete under the project root (or `place`) -> `Ok`.
    Fs {
        op: FsOp,
        #[serde(default)]
        place: Place,
    },
    /// Which coding assistants this machine has, and whether each is signed in -> `AiAgents`.
    /// Quick: it starts none of them.
    AiAgents,
    /// The skills on this machine an AI chat with `agent` can use from the `/` menu:
    /// the ones the assistant loads by itself (`SkillGroup::Provider`) and the project's
    /// (`SkillGroup::Repo`) -> `AiSkills`.
    AiSkills { agent: AiAgent },
    /// Start a chat session with `agent` at the project root -> `AiOpened`. Fails with `NotFound`
    /// when it isn't installed and `PermissionDenied` when it isn't signed in (`AiSetup` fixes
    /// both). nib never sees the credentials: the assistant signs itself in. `model`: one of
    /// its `AiAgentInfo::models`; `None` runs the one it's set up with.
    /// `tools`: an agent, not a chat: it may read, edit and run commands in the project
    /// (in the assistant's auto permission mode), and what it does arrives as `AiEvent::Step`s.
    /// Only Claude Code so far; the others stay read-only chats.
    AiOpen {
        /// Replace this live session while retaining its conversation and work ownership.
        #[serde(default)]
        replace: Option<AiId>,
        agent: AiAgent,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        tools: bool,
        /// How much an agent may do without asking.
        #[serde(default)]
        mode: AiMode,
        /// Go on with this past conversation (an `AiPast::id`): the assistant resumes its
        /// own session of it, and what follows is added to its record.
        #[serde(default)]
        resume: Option<String>,
        /// How hard the model thinks: one of `AiAgent::efforts`; `None`: the assistant's own
        /// setting.
        #[serde(default)]
        effort: Option<String>,
        /// Added to the assistant's own system prompt for the whole session.
        #[serde(default)]
        prompt: Option<String>,
    },
    /// This project's past conversations with agents, the latest first -> `AiHistory`.
    /// `query`: only the ones with this somewhere in them (asked, said, run, changed).
    AiHistory { query: String },
    /// One past conversation, whole -> `AiPastLoaded`.
    AiHistoryLoad { id: String },
    /// Delete a past conversation -> `Ok`.
    AiHistoryDelete { id: String },
    /// Every message agents in this project sent each other through the mailbox, the latest
    /// first -> `AiMailHistory`. `query`: only the ones with this in them (sender,
    /// recipient or text).
    AiMailHistory { query: String },
    /// A terminal that installs `agent` if it is missing (with its own curl installer) and then
    /// signs it in if it isn't -> `PtyOpened`. The user watches it and answers its prompts
    /// (a device code, a confirmation); it exits 0 when the assistant is ready. `login`: sign in
    /// even if it looks signed in (an expired or revoked sign-in looks the same on disk); an
    /// older daemon ignores it.
    AiSetup {
        agent: AiAgent,
        cols: u16,
        rows: u16,
        #[serde(default)]
        login: bool,
    },
    /// A terminal that updates installed `agent` to its latest version (its own updater, or its
    /// installer again) -> `PtyOpened`.
    AiUpdate { agent: AiAgent, cols: u16, rows: u16 },
    /// Load a local model into memory and keep it there (`warm`), or unload it -> `Ok`.
    /// `model` is OpenCode's id (`ollama/qwen3:8b`). Warming starts Ollama if it isn't running,
    /// loads the model and has it read OpenCode's instructions once (so a chat's first reply is
    /// quick), and answers when that's done: a minute or more for a big model.
    AiWarm {
        model: String,
        warm: bool,
        /// Which GPUs' Ollama (`"all"`, `"cpu"`, `"0"`, `"0,1"`); `None`: all.
        #[serde(default)]
        gpu: Option<String>,
    },
    /// One job, as JSON (the format `nib ai` documents: a model call or an agent run) ->
    /// `AiJobDone` with its result as JSON. Its files are on the daemon's machine.
    AiJob { job: String },
    /// Every job in the JSONL file `jobs`, results under `out` (both on the daemon's machine,
    /// relative to the root): resumable, `concurrency` at a time, each tried `retries` more times
    /// -> `AiProgress` per job, then `AiBatchDone`.
    AiBatch { jobs: String, out: String, concurrency: u32, retries: u32 },
    /// Install `agent` without a terminal (what `AiSetup` does, its output sent as `AiProgress`
    /// lines `{"line": …}`) -> `Ok` once it's installed. OpenCode comes with Ollama: either
    /// one that's missing is installed. A step that needs a password (`sudo`) fails here; `AiSetup`
    /// asks for it in its terminal.
    AiInstall { agent: AiAgent },
    /// Download an Ollama model (`qwen3:8b`) -> `AiProgress` lines (Ollama's own:
    /// `{"status", "completed", "total"}`), then `Ok` once it's there. Starts Ollama if it
    /// isn't running.
    AiPull { model: String },
    /// Delete an Ollama model's weights from disk -> `Ok`.
    AiDelete { model: String },
    /// Search the project -> `Search`: files whose path matches `query` fuzzily, and lines
    /// that contain it (smart case: case-insensitive unless it has a capital). Folders whose name
    /// starts with `.`, gitignored files and binary files are skipped. At most `max` lines.
    Search { query: String, max: u32 },
    /// The project's apps (`$NIBDIR/apps.json` on the daemon's machine): read, define, start,
    /// stop, approve -> `Apps`, every app as it is after `op`. Starting and stopping are begun,
    /// not waited for: `ServerNotify::App` says how each goes on. `user`: the person at the
    /// window asked (a click, a key they pressed); false for `nib ctl` and whatever else the
    /// window passes on. Only the user approves an app, and the user's requests start only
    /// approved ones (see `AppOp::Approve`).
    App { op: AppOp, user: bool },
    Shutdown,
}

/// What to do with the project's apps (`Request::App`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AppOp {
    List,
    /// Add or replace one app, whole. Running servers keep the definition they were started
    /// with until restarted. Not from the user: the app waits for the user's approval.
    Set { name: String, def: AppDef },
    /// Stop its servers and forget it.
    Remove { name: String },
    /// Start these apps (none named: every approved one), each server after those it comes
    /// `after`.
    Up { names: Vec<String> },
    /// Stop these apps (none named: all).
    Down { names: Vec<String> },
    /// Stop the app's servers (or just `server`), wait for their ports to be free, start them.
    Restart { name: String, server: Option<String> },
    /// The user saw the app's commands (`AppInfo::hash` names that definition) and lets them
    /// run. Only with `user`; refused if the app changed since (another hash).
    Approve { name: String, hash: String },
}

/// One app of the project, as `apps.json` defines it: the processes it runs, and the page to
/// show once they answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppDef {
    /// What the user calls it (absent: its name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The page to show once its server's port answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<AppPage>,
    /// Its processes, by name.
    #[serde(default)]
    pub servers: std::collections::BTreeMap<String, ServerDef>,
}

/// An app's page: `http://127.0.0.1:PORT<path>`, the port being `server`'s.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppPage {
    pub server: String,
    pub path: String,
}

/// Who last defined an app, and when.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppOrigin {
    /// `user`, `agent:NAME`, or `nib ctl`.
    pub by: String,
    /// Seconds since 1970.
    pub at: u64,
}

/// An app and how it is doing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AppInfo {
    pub name: String,
    pub def: AppDef,
    /// Its servers, in the order of their names.
    pub servers: Vec<ServerInfo>,
    #[serde(default)]
    pub origin: Option<AppOrigin>,
    /// What `AppOp::Approve` gives to approve this definition.
    #[serde(default)]
    pub hash: String,
    /// The user approved this definition, as it is now.
    #[serde(default)]
    pub approved: bool,
    /// Its definition was removed: forget it.
    #[serde(default)]
    pub removed: bool,
}

impl AppInfo {
    /// What the user calls it.
    pub fn title(&self) -> &str {
        self.def.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or(&self.name)
    }

    /// How it is, said once: the most telling of its servers' states (a failure, then one on
    /// its way, then ready; stopped only when all are).
    pub fn state(&self) -> ServerState {
        let rank = |s: &ServerState| match s {
            ServerState::Exited(_) | ServerState::PortTaken => 3,
            ServerState::Waiting | ServerState::Starting => 2,
            ServerState::Ready => 1,
            ServerState::Stopped => 0,
        };
        self.servers.iter().map(|s| s.state).max_by_key(rank).unwrap_or_default()
    }

    /// Any of its servers runs or is on its way.
    pub fn running(&self) -> bool {
        self.servers.iter().any(|s| s.state == ServerState::Ready || s.state.coming())
    }
}

/// One process of an app.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerDef {
    /// Run by the user's shell.
    pub cmd: String,
    /// Where it listens (on 127.0.0.1): how nib tells that it is ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Servers of the same app whose ports must answer before this one is started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Where it runs, relative to the project root (absent: the root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerState {
    #[default]
    Stopped,
    /// For the servers it comes after.
    Waiting,
    /// Running; its port does not answer yet.
    Starting,
    /// Running, and its port answers (or it has none).
    Ready,
    /// It ended by itself, with this exit code.
    Exited(Option<i32>),
    /// Not started: something else already listens on its port.
    PortTaken,
}

impl ServerState {
    /// As `nib ctl app list` and `nib-app` say it.
    pub fn name(self) -> String {
        match self {
            ServerState::Stopped => "stopped".into(),
            ServerState::Waiting => "waiting".into(),
            ServerState::Starting => "starting".into(),
            ServerState::Ready => "ready".into(),
            ServerState::Exited(Some(code)) => format!("exited({code})"),
            ServerState::Exited(None) => "exited".into(),
            ServerState::PortTaken => "port-taken".into(),
        }
    }

    /// On its way to `Ready`.
    pub fn coming(self) -> bool {
        matches!(self, ServerState::Waiting | ServerState::Starting)
    }
}

/// A server of an app and how it is doing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerInfo {
    /// Its name in its app.
    pub name: String,
    pub def: ServerDef,
    pub state: ServerState,
    /// Its terminal, while it runs (`PtyOutput`, `PtyInput`, `PtyResize` as for any other;
    /// `PtyClose` stops the server).
    #[serde(default)]
    pub pty: Option<PtyId>,
}

/// The directories the file tree can show. Paths in `Tree` / `Fs` are relative to it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Place {
    /// The project root.
    #[default]
    Project,
    /// This daemon session's scratch directory (`$NIB_SCRATCH`), deleted a day after it ends.
    Scratch,
    /// The project's persistent directory (`$NIBDIR`), outside the project.
    Nibdir,
}

/// The operations nib can ask for. Deliberately a fixed list, not a command line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GitOp {
    Fetch,
    Pull,
    Push,
    /// Commits what is staged; when nothing is staged, stages every change first (`add -A`).
    Commit { message: String },
    Stash,
    StashPop,
    /// `git add -A -- paths` (relative to the project root; records deletions too).
    Stage { paths: Vec<String> },
    /// Take `paths` out of the index again; the work tree is not touched.
    Unstage { paths: Vec<String> },
}

/// Which two states of the project a diff compares.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DiffScope {
    /// The last commit against the work tree: everything not committed, untracked files included.
    #[default]
    Head,
    /// The index against the work tree: what `git add` would stage (untracked files included).
    Unstaged,
    /// The last commit against the index: what a commit would record.
    Staged,
    /// One agent's work, like its pull request. An agent on its own branch (`nib-work start`):
    /// where the branch left its base against its work tree, committed or not. An agent in the
    /// shared work tree: `Head`, only the files it changed.
    Agent(AiId),
    /// Everybody's work: `Head` of the shared work tree, then each agent's branch.
    Everyone,
    /// What an agent on its own branch changed since the last review was sent to it
    /// (`ReviewOp::Send`): that moment's work tree against the work tree now.
    Since(AiId),
}

/// A diff: its files in git's order (by path). Paths are relative to the project root, with `/`
/// separators; changes outside the root are left out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DiffSet {
    /// Identifies this diff (a hash of what git printed).
    pub stamp: u64,
    /// `None`: the same diff as the `known` stamp of the request.
    pub files: Option<Vec<DiffFile>>,
    /// The root is inside a git work tree. `false`: there is nothing to compare.
    pub repo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DiffFile {
    /// Where the file is now (a deleted file: where it was).
    pub path: String,
    /// A renamed file's path before.
    pub old_path: Option<String>,
    pub status: DiffStatus,
    /// Not text: no hunks.
    pub binary: bool,
    /// Lines added and removed, over the whole file (also when `truncated`).
    pub adds: u32,
    pub dels: u32,
    /// The diff is longer than one answer carries; the last hunks are missing.
    pub truncated: bool,
    pub hunks: Vec<DiffHunk>,
    /// The agents whose work this is (`DiffScope::Agent` / `Everyone`): on its branch, or having
    /// changed the file in the shared work tree. Empty: nobody nib knows of.
    #[serde(default)]
    pub agents: Vec<AiId>,
    /// The folder `path` is relative to when that is not the project root: an agent's work tree
    /// (absolute, on the daemon's machine).
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    #[default]
    Modified,
    Deleted,
    Renamed,
}

/// One `@@ -old_start,old_len +new_start,new_len @@ header` block. Line numbers are 1-based.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// What git prints after the second `@@`: the enclosing function, usually.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Without its line ending.
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiffLineKind {
    /// In both.
    Context,
    Add,
    Del,
}

/// File operations of the file tree. Paths are relative to the project root and may not leave it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FsOp {
    /// An empty file (missing parent folders are created). Fails if `path` exists.
    CreateFile { path: String },
    CreateDir { path: String },
    /// `git mv` when git tracks `from`, a plain rename otherwise. Fails if `to` exists.
    Rename { from: String, to: String },
    /// Files, and folders with everything in them.
    Delete { paths: Vec<String> },
    /// Files dropped on the tree from the client's machine: `dirs` are made, then each
    /// file is written with its bytes. Fails before writing anything if a file already exists.
    Put { dirs: Vec<String>, files: Vec<PutFile> },
}

/// One file of `FsOp::Put`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PutFile {
    pub path: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LspOp {
    /// -> `Locations`
    Definition,
    /// -> `Locations` (declaration included)
    References,
    /// Every occurrence of the symbol in this file -> `Locations`
    Highlights,
    /// -> `RenameRange` (`None`: nothing renameable here)
    PrepareRename,
    /// -> `Edits`
    Rename { new_name: String },
    /// Signature and docs of the symbol -> `Hover` (`None`: nothing to show)
    Hover,
}

/// What the language server says about a symbol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HoverInfo {
    /// Markdown (signatures come as fenced code blocks).
    pub markdown: String,
    /// The symbol the text is about, on one line or spanning several.
    pub start: Pos,
    pub end: Pos,
}

/// A language server the client config adds or changes (`[lsp.<name>]`). The daemon has
/// built-in servers (python, rust, go, typescript, c); an entry with one of their names changes
/// only the fields it sets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LspServer {
    pub name: String,
    /// argv. `None`: the built-in command; empty: the server is turned off.
    pub command: Option<Vec<String>>,
    /// File extensions it serves, without the dot. Claiming one takes it from any other server.
    pub extensions: Vec<String>,
    /// Files that mark a project root (`Cargo.toml`, `go.mod`): the nearest one above a file,
    /// within the nib root, is the server's root. Empty: the built-in list.
    pub roots: Vec<String>,
}

/// 0-based line and column. Columns count Unicode scalar values (Rust `char`s), which is what
/// ropey's char indices use.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Location {
    /// Absolute path on the daemon side.
    pub path: String,
    pub start: Pos,
    pub end: Pos,
    /// The text of `start.line` (for listing results), without its line ending.
    pub line_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextEdit {
    pub start: Pos,
    pub end: Pos,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEdit {
    /// Absolute path on the daemon side.
    pub path: String,
    pub edits: Vec<TextEdit>,
}

pub type PtyId = u32;
pub type AiId = u32;

/// The coding assistants nib can chat through: each one's own CLI, installed and signed in on the
/// daemon's machine. A fixed list, not a command line.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AiAgent {
    Claude,
    Codex,
    Grok,
    /// OpenCode with whatever model it's set up for (e.g. a local Ollama one). nib never
    /// installs it: it needs a provider set up first.
    OpenCode,
}

impl AiAgent {
    /// What the other agents call the agent of session `id` in the mailbox: `claude-3`.
    pub fn mail_name(self, id: AiId) -> String {
        let slug = match self {
            AiAgent::Claude => "claude",
            AiAgent::Codex => "codex",
            AiAgent::Grok => "grok",
            AiAgent::OpenCode => "opencode",
        };
        format!("{slug}-{id}")
    }

    pub const ALL: [AiAgent; 4] = [AiAgent::Claude, AiAgent::Codex, AiAgent::Grok, AiAgent::OpenCode];

    pub fn name(self) -> &'static str {
        match self {
            AiAgent::Claude => "Claude Code",
            AiAgent::Codex => "Codex",
            AiAgent::Grok => "Grok",
            AiAgent::OpenCode => "OpenCode",
        }
    }

    /// Who makes it.
    pub fn vendor(self) -> &'static str {
        match self {
            AiAgent::Claude => "Anthropic",
            AiAgent::Codex => "OpenAI",
            AiAgent::Grok => "xAI",
            AiAgent::OpenCode => "open source, your own models",
        }
    }

    /// Its name in config.toml (`[ai] agent`).
    pub fn id(self) -> &'static str {
        match self {
            AiAgent::Claude => "claude",
            AiAgent::Codex => "codex",
            AiAgent::Grok => "grok",
            AiAgent::OpenCode => "opencode",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    /// The thinking levels it takes (`AiOpen::effort`), least first. Empty: it has no such
    /// setting (or nib doesn't pass one yet).
    pub fn efforts(self) -> &'static [&'static str] {
        match self {
            // `claude --effort`.
            AiAgent::Claude => &["low", "medium", "high", "xhigh", "max"],
            // Codex's `model_reasoning_effort`.
            AiAgent::Codex => &["low", "medium", "high", "xhigh", "max", "ultra"],
            AiAgent::Grok | AiAgent::OpenCode => &[],
        }
    }
}

/// One assistant as the daemon's machine has it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiAgentInfo {
    pub agent: AiAgent,
    pub installed: bool,
    /// `None`: it couldn't tell (the chat reports its own error if it isn't).
    pub signed_in: Option<bool>,
    /// `AiSetup` can install it here (a Unix machine with `curl`).
    pub can_install: bool,
    /// The models it can chat with, best first. Empty: it can't say, and runs the one
    /// it's set up with.
    #[serde(default)]
    pub models: Vec<AiModel>,
    /// OpenCode, which runs local models: the GPUs that run them fast on that machine. Empty:
    /// only the CPU (or another assistant).
    #[serde(default)]
    pub gpus: Vec<AiGpu>,
    /// OpenCode, which comes with Ollama: Ollama on that machine. `None` for the others.
    #[serde(default)]
    pub ollama: Option<OllamaInfo>,
}

/// Ollama on the daemon's machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OllamaInfo {
    pub installed: bool,
    /// The models whose weights are on disk, by name.
    pub models: Vec<OllamaModel>,
    /// What they take on disk together (a file two models share counts once).
    pub disk_bytes: u64,
}

/// One downloaded Ollama model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OllamaModel {
    /// `name:tag`, as Ollama names it (`qwen3:8b`); OpenCode's id is `ollama/<name>`.
    pub name: String,
    /// Its files' size on disk.
    pub bytes: u64,
    /// Loaded in memory on the user's own Ollama server.
    pub warm: bool,
}

/// One GPU of the daemon's machine, for local models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiGpu {
    /// What `gpu` selections name it by (`"0"`, `"0,1"`): the driver's own order.
    pub index: u32,
    pub name: String,
    /// Its memory (unified memory on Apple Silicon), when known.
    pub memory_mb: Option<u64>,
    /// `apple`, `nvidia` or `amd`.
    pub kind: String,
}

/// One model an assistant can run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiModel {
    /// What the assistant's CLI takes (`opus`, `gpt-6-sol`, `ollama/qwen3.8:27b-mlx`).
    pub id: String,
    /// What to show.
    pub name: String,
    /// Reasoning levels supported by this model, when its catalog supplies them.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Its weights are on that machine (an Ollama model that's been pulled); not a cloud one.
    #[serde(default)]
    pub local: bool,
    /// A local model: loaded in memory, so it answers at once (`AiWarm` changes it). `None`
    /// for a cloud one.
    #[serde(default)]
    pub warm: Option<bool>,
}

impl AiAgentInfo {
    /// Installed and not known to be signed out: a chat can start.
    pub fn ready(&self) -> bool {
        self.installed && self.signed_in != Some(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClientNotify {
    /// Liveness / latency probe; daemon answers with `ServerNotify::Pong`.
    Ping { seq: u64 },
    PtyInput {
        id: PtyId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    PtyResize { id: PtyId, cols: u16, rows: u16 },
    PtyClose { id: PtyId },
    /// The full text of an open buffer, for the language server. `version` increases with every
    /// change; the daemon ignores a sync that isn't newer than the last one.
    DocSync { path: String, version: u64, text: String },
    /// The buffer was closed: the language server goes back to the file on disk.
    DocClose { path: String },
    /// The files the client has open, replacing the last set. The daemon watches them and sends
    /// `ServerNotify::FileChanged` when one changes on disk or goes away, other than through
    /// its own `WriteFile`.
    Watch { paths: Vec<String> },
    /// The user's next chat message. `attach`: the files and folders it mentions with `@`;
    /// the daemon puts their contents after the text (a folder: its listing and as many of its
    /// files as fit). Ones that don't exist are left out, so `@someone` is just text.
    /// `images`: pictures pasted into the message, shown to the model with it.
    AiSend {
        id: AiId,
        text: String,
        #[serde(default)]
        attach: Vec<AiAttach>,
        #[serde(default)]
        images: Vec<AiImage>,
    },
    /// Change how much the agent may do without asking -> `AiEvent::Mode`. Takes effect
    /// at once for Claude Code, with the next message for the others.
    AiSetMode { id: AiId, mode: AiMode },
    /// The user's answer to `AiEvent::Ask`. `message`: said to the agent with a refusal
    /// (what to do instead). `answers`: with a yes to questions, one per question, in order
    /// (several picks joined by ", ").
    AiAnswer { id: AiId, request: String, allow: bool, message: Option<String>, answers: Vec<String> },
    /// Make room in the chat's context: the conversation so far is replaced by a summary of
    /// it, focused on `focus` if given. Answered with `AiEvent::Compacted`, then `TurnDone`.
    AiCompact { id: AiId, focus: Option<String> },
    /// End the chat session (the assistant's process exits).
    AiClose { id: AiId },
    /// Stop the reply under way: the command it is running is killed, the turn ends
    /// (`TurnDone`), and the session takes the next message.
    AiInterrupt { id: AiId },
    /// Freeze (`on`) or thaw every process of the session, mid-command or mid-request,
    /// without it noticing -> `AiEvent::Suspended`. Unix only.
    AiSuspend { id: AiId, on: bool },
    /// The answer to `ServerNotify::Web` `id`: the JSON object the control socket would have
    /// answered (`{"ok": true, …}` or `{"ok": false, "error": "…"}`).
    WebReply { id: u64, reply: String },
}

// ---------------------------------------------------------------------------------------------
// daemon -> client
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServerMsg {
    Response {
        id: ReqId,
        result: Result<Response, RpcError>,
    },
    Notify(ServerNotify),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Response {
    Hello {
        protocol: u32,
        daemon_version: String,
        /// Absolute, canonical project root on the daemon's machine.
        root: String,
        /// `$NIB_SCRATCH` and `$NIBDIR` on the daemon's machine (empty if they couldn't be
        /// made).
        #[serde(default)]
        scratch: String,
        #[serde(default)]
        nibdir: String,
        hostname: String,
        os: String,
        arch: String,
        /// File extensions a language server is set up for: the client syncs buffers and
        /// asks about symbols only in these files.
        #[serde(default)]
        lsp_extensions: Vec<String>,
    },
    File {
        /// Absolute path on the daemon side.
        path: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        mtime_ns: u128,
    },
    Written {
        path: String,
        mtime_ns: u128,
    },
    Dir {
        path: String,
        entries: Vec<DirEntry>,
    },
    PtyOpened {
        id: PtyId,
    },
    AiOpened {
        id: AiId,
    },
    /// In `AiAgent::ALL` order.
    AiAgents(Vec<AiAgentInfo>),
    AiSkills(Vec<AiSkill>),
    AiHistory(Vec<AiPast>),
    AiPastLoaded { past: AiPast, entries: Vec<AiPastEntry> },
    AiMailHistory(Vec<AiLetter>),
    /// The job's result, as JSON.
    AiJobDone { result: String },
    /// The batch's summary, as JSON.
    AiBatchDone { summary: String },
    Locations(Vec<Location>),
    /// The range a rename would replace, and the current name.
    RenameRange(Option<(Pos, Pos, String)>),
    Edits(Vec<FileEdit>),
    Hover(Option<HoverInfo>),
    Git(GitInfo),
    GitDetails {
        /// Newest first.
        commits: Vec<GitCommit>,
        changes: Vec<GitChange>,
    },
    GitRan {
        /// git exited with success.
        ok: bool,
        /// What git printed (stdout and stderr).
        output: String,
    },
    Tree(TreeSnapshot),
    Diff(DiffSet),
    WorkPlan(Option<String>),
    Review(Review),
    /// The review was sent: `message` is what to say to the agent (the open comments, and how
    /// to answer them).
    ReviewSent { review: Review, message: String },
    Search(SearchResults),
    /// In the order of their names.
    Apps(Vec<AppInfo>),
    Ok,
}

/// The project's files and what git says about them. Every path is relative to the project root,
/// with `/` separators.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TreeSnapshot {
    /// Identifies `entries` (a hash of the listing).
    pub stamp: u64,
    /// `None`: same listing as the `known` stamp of the request.
    pub entries: Option<Vec<TreeEntry>>,
    /// The project has more entries than a listing carries; the deepest ones are missing.
    pub truncated: bool,
    /// The root is inside a git work tree. `false`: the fields below are empty.
    pub repo: bool,
    /// `git status`, untracked files listed one by one.
    pub changes: Vec<GitChange>,
    /// Files that differ between the merge base and the upstream branch as of the last fetch:
    /// what a pull would touch.
    pub upstream: Vec<String>,
    /// Commits only here / only on the upstream branch.
    pub ahead: u32,
    pub behind: u32,
}

/// What `Request::Search` found.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchResults {
    /// Files whose path matches the query fuzzily, best first, relative to the root.
    pub files: Vec<String>,
    /// Lines containing the query, file by file in the file tree's order (breadth first; `path`
    /// absolute, `start` / `end` the first match on the line, columns in chars).
    pub lines: Vec<Location>,
    /// More lines matched than were sent, or the project is too big to read whole.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    pub path: String,
    pub dir: bool,
    /// Matches a gitignore rule. An ignored folder is listed without its contents.
    pub ignored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GitInfo {
    /// No usable `git` on the daemon's machine.
    NoGit,
    /// The project root is not inside a git work tree.
    NotRepo,
    Repo(GitRepo),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitRepo {
    /// Name of the work tree's top directory.
    pub name: String,
    /// `None`: detached HEAD.
    pub branch: Option<String>,
    /// `user.name` / `user.email` as git resolves them for this repository.
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    /// `None`: no commits yet.
    pub last_commit: Option<GitCommit>,
}

/// One line of `git status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitChange {
    /// Relative to the top of the work tree.
    pub path: String,
    /// The two status letters (index, work tree), e.g. `M `, ` M`, `??`.
    pub code: String,
    /// Part of it is in the index, so a plain commit includes it.
    pub staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitCommit {
    /// Abbreviated hash.
    pub hash: String,
    pub subject: String,
    pub author: String,
    /// Commit time, seconds since the Unix epoch.
    pub time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServerNotify {
    Pong {
        seq: u64,
    },
    PtyOutput {
        id: PtyId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    PtyExit {
        id: PtyId,
        code: Option<i32>,
    },
    /// A batch job finished (one progress line, as JSON), for request `req`.
    AiProgress { req: ReqId, line: String },
    AiEvent {
        id: AiId,
        event: AiEvent,
    },
    /// Free-form log line surfaced in the client's status bar / log.
    Log {
        level: LogLevel,
        msg: String,
    },
    /// A watched file (`ClientNotify::Watch`, `path` as the client sent it) changed on disk:
    /// its new modification time, or `None` if it is gone.
    FileChanged {
        path: String,
        mtime_ns: Option<u128>,
    },
    /// One of the project's apps (`Request::App`) changed: a server's state or terminal, its
    /// definition, its approval. Sent before any `PtyOutput` of a terminal it names.
    App(AppInfo),
    /// An agent's `nib-web …`: a request for the window's web tab, as the JSON object its
    /// control socket takes (`{"cmd": "web.read", …}`; `web::agent_may` says which). The
    /// window answers with `ClientNotify::WebReply` carrying the same `id`.
    Web { id: u64, req: String },
}

/// What a chat session says, already taken out of the assistant's own event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AiEvent {
    /// The assistant is up (Claude Code reports this when the first message arrives): the model
    /// it runs.
    Started { model: String },
    /// More of the reply being written.
    Text(String),
    /// The reply is complete. `error`: why it stopped early (rate limit, network, ...).
    TurnDone { error: Option<String> },
    /// The session ended; nothing more will arrive. `msg`: what the assistant said on the way out.
    Exit { msg: Option<String> },
    /// The conversation was replaced by a summary (`AiCompact`): the context's tokens before
    /// and after, when the assistant tells.
    Compacted { before: Option<u64>, after: Option<u64> },
    /// How full the chat's context is and what it costs, as of the reply just finished.
    /// Each one replaces the last; a field it can't tell is `None` (or empty).
    Usage(AiUsage),
    /// The agent started doing something: a tool call, as the harness reports it.
    Step(AiStep),
    /// That step ended.
    StepDone(AiStepDone),
    /// That step (a sub-agent) was started in the background: it goes on after its launch,
    /// past the end of the turn, and a later `StepDone` ends it.
    Background { id: String },
    /// The agent wants to do something its mode doesn't let it do unasked; it waits for
    /// `AiAnswer`.
    Ask(AiAsk),
    /// A pending question was resolved by the agent before the user answered it.
    AskResolved { request: String },
    /// The mode the agent works in now.
    Mode(AiMode),
    /// What the other agents call this one in the mailbox: made from its task once it has
    /// one (`fix-calc-sum`); until then `AiAgent::mail_name`.
    Name(String),
    /// A message between agents, in the chat of the one who sent it and of the one it is
    /// for.
    Mail(AiMail),
    /// `AiSuspend` was carried out: the session is frozen (`on`) or running again.
    /// `error`: it couldn't be (this machine can't suspend processes), and nothing changed.
    Suspended { on: bool, error: Option<String> },
    /// The agent believes the job it was given is done (`nib-mail done`): what was asked,
    /// found and done, for the user to accept (the agent is then ended) or keep working.
    Finished(AiFinished),
    /// The agent recorded its plan or started its own branch (`nib-work`): how things stand.
    Work(AiWork),
}

/// An agent's organized work (`nib-work`): its plan, and the branch it works on in a work tree
/// of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AiWork {
    /// `nib/feat/<slug>` or `nib/fix/<slug>`; `None`: it has no branch (yet), and works in the
    /// shared work tree.
    pub branch: Option<String>,
    /// The folder it works in (the work tree, at the project root's place in it), absolute on
    /// the daemon's machine.
    pub dir: Option<String>,
    /// The branch its own left, and lands on.
    pub base: Option<String>,
    /// It recorded a plan; the number changes when the plan does (`Request::WorkPlan` reads it).
    pub plan: Option<u64>,
    /// Its work has review comments; the number changes when they do (`Request::Review`).
    #[serde(default)]
    pub review: Option<u64>,
}

/// The user's review of an agent's work, as nibd keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Review {
    /// How many times it was sent to the agent.
    pub round: u32,
    /// A review was sent while the agent was on its own branch, so `DiffScope::Since` has
    /// something to compare with.
    #[serde(default)]
    pub since: bool,
    pub comments: Vec<ReviewComment>,
}

/// One comment: on lines of a file, on a file (`start_line` 0), on lines of the agent's plan
/// (`plan`), or on the work as a whole (no `path`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReviewComment {
    /// `c1`, `c2`, …
    pub id: String,
    /// Relative to where the agent works; `None`: about the whole work.
    pub path: Option<String>,
    /// On the file as it was (a removed line), not as it is now.
    #[serde(default)]
    pub old: bool,
    /// On lines of the plan the agent recorded (`nib-work plan`), not of a file: no `path`.
    #[serde(default)]
    pub plan: bool,
    /// 1-based, both included; 0 and 0: about the whole file.
    pub start_line: u32,
    pub end_line: u32,
    /// The text of those lines when it was written: where they went is found by it.
    pub anchor: String,
    pub body: String,
    pub author: String,
    /// The round it was first sent in; 0: not sent yet.
    pub round: u32,
    pub resolved: bool,
    /// Its lines changed and their text wasn't found again: the line numbers are a guess.
    pub drifted: bool,
    pub replies: Vec<ReviewReply>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReviewReply {
    pub id: String,
    pub body: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReviewOp {
    /// A new comment by the user (`id`, `author`, `round`, `replies` are filled in).
    Comment(ReviewComment),
    /// The user's reply to comment `to`.
    Reply { to: String, body: String },
    Resolve { comment: String, resolved: bool },
    /// Take a comment back.
    Remove { comment: String },
    /// Send the open comments to the agent: the next round.
    Send,
}

/// An agent's word that its job is done, at most `AiFinished::WORDS` words in all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AiFinished {
    /// What it was asked to do.
    pub asked: String,
    /// What it found out on the way, when that matters.
    pub found: Option<String>,
    /// What it did.
    pub did: String,
}

impl AiFinished {
    pub const WORDS: usize = 100;

    pub fn words(&self) -> usize {
        [Some(&self.asked), self.found.as_ref(), Some(&self.did)].into_iter().flatten().map(|s| s.split_whitespace().count()).sum()
    }

    /// As one block of text: a line per part.
    pub fn text(&self) -> String {
        let mut t = format!("Asked: {}", self.asked.trim());
        if let Some(f) = self.found.as_deref().filter(|f| !f.trim().is_empty()) {
            t.push_str(&format!("\nFound: {}", f.trim()));
        }
        t.push_str(&format!("\nDone: {}", self.did.trim()));
        t
    }
}

/// How much an agent may do without asking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum AiMode {
    /// Asks before every edit and command.
    Manual,
    /// Edits files unasked; asks before commands.
    AcceptEdits,
    /// Reads and plans; changes nothing until the plan is approved.
    Plan,
    /// Asks nothing; the harness's own safety checks still apply.
    #[default]
    Auto,
}

impl AiMode {
    pub const ALL: [AiMode; 4] = [AiMode::Manual, AiMode::AcceptEdits, AiMode::Plan, AiMode::Auto];
    pub fn name(self) -> &'static str {
        match self {
            AiMode::Manual => "manual",
            AiMode::AcceptEdits => "accept-edits",
            AiMode::Plan => "plan",
            AiMode::Auto => "auto",
        }
    }
    pub fn from_name(s: &str) -> Option<AiMode> {
        AiMode::ALL.into_iter().find(|m| m.name() == s)
    }
}

impl std::fmt::Display for AiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A past conversation with an agent, in short.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiPast {
    pub id: String,
    pub agent: AiAgent,
    pub model: Option<String>,
    /// What the agent was called (after its task).
    pub name: Option<String>,
    /// The first line of what the user first asked.
    pub title: String,
    /// Unix seconds.
    pub started: i64,
    pub updated: i64,
    /// How many times the user spoke.
    pub messages: u32,
    pub steps: u32,
    /// The files it changed, relative to the project.
    pub files: Vec<String>,
    /// The end of what the agent last said.
    pub last: String,
    /// Where a search matched.
    pub hit: Option<String>,
}

/// One thing that happened in a past conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AiPastEntry {
    User(String),
    /// A paragraph of what the agent said.
    Text(String),
    Step(AiStep),
    StepDone { done: AiStepDone, seconds: f32 },
    Mail(AiMail),
    Note(String),
}

/// A message between agents through nib's mailbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiMail {
    /// An agent's mailbox name (`AiAgent::mail_name`), or `nib`.
    pub from: String,
    pub to: String,
    pub text: String,
    /// This chat received it (otherwise it sent it).
    pub incoming: bool,
}

/// A message that went through the mailbox, as the project keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiLetter {
    pub from: String,
    /// An agent's mailbox name, or `all`.
    pub to: String,
    pub text: String,
    /// Unix seconds.
    pub at: i64,
}

/// A picture sent with a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiImage {
    /// `image/png`, `image/jpeg`, …
    pub media_type: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Something an agent asks leave for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiAsk {
    /// Names the question in `AiAnswer`.
    pub request: String,
    /// The step it is about, once that step shows.
    pub step: Option<String>,
    /// The harness's tool: `Bash`, `Edit`, `ExitPlanMode` (approve the plan), `AskUserQuestion`.
    pub tool: String,
    /// What it wants to do, or its plan: text to show. Empty for questions.
    pub what: String,
    /// Questions to answer (`AskUserQuestion`), not something to allow: their answers go back
    /// with a yes, one for each (`AiAnswer::answers`).
    pub questions: Vec<AiQuestion>,
}

/// One question an agent asks, with the answers it offers. The user may answer in their own
/// words instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiQuestion {
    /// A word or two naming it (a chip).
    pub header: String,
    pub question: String,
    pub options: Vec<AiOption>,
    /// Any number of the options may be picked, not just one.
    pub multi: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiOption {
    pub label: String,
    /// What picking it means.
    pub description: String,
    /// Something to look at while it is considered (code, a mockup), shown monospaced.
    pub preview: Option<String>,
}

/// One thing an agent does. nib only shows it: the harness runs it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiStep {
    /// The harness's id for the call; `AiStepDone` names it.
    pub id: String,
    /// The step (a sub-agent) this one happens inside.
    pub parent: Option<String>,
    pub kind: AiStepKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AiStepKind {
    /// A shell command. `timeout_ms`: when the harness will stop waiting for it.
    Command { command: String, description: Option<String>, timeout_ms: Option<u64>, background: bool },
    /// Reading a file: `limit` lines from line `offset` (1-based), or all of it.
    Read { path: String, offset: Option<u64>, limit: Option<u64> },
    /// Replacing `old` with `new` in a file.
    Edit { path: String, old: String, new: String },
    /// Writing a whole file.
    Write { path: String, content: String },
    /// Loading a skill, by the agent's own choice.
    Skill { name: String },
    /// Searching file names or contents.
    Search { pattern: String, path: Option<String> },
    /// Starting a sub-agent.
    Agent { description: String },
    /// Any other tool: its name and its input as JSON.
    Other { tool: String, input: String },
}

/// How a step ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiStepDone {
    pub id: String,
    pub ok: bool,
    /// What it printed or answered; the middle of a long one is left out.
    pub output: String,
    /// An edit: the first line it changed (1-based).
    pub line: Option<u32>,
}

/// A file or folder attached to a chat message with `@`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiAttach {
    /// Relative to the project root (or absolute). A trailing `/` is allowed on a folder.
    pub path: String,
    /// The file as open in the editor, unsaved edits included; `None`: read it from disk.
    pub text: Option<String>,
    /// A skill invoked with `/name`: `path` is its `SKILL.md` (or `text` its contents) and it
    /// goes before the message, without its frontmatter.
    #[serde(default)]
    pub skill: Option<String>,
}

/// Where a skill comes from, in the `/` menu's order.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkillGroup {
    /// Where the assistant itself looks, in the user's home (`~/.claude/skills`,
    /// `~/.agents/skills`, …): it would load them without nib too.
    Provider,
    /// nib's own (`nib-*`), shipped next to the nib binary.
    Nib,
    /// The user's skills for nib: `skills/` next to nib's `config.toml`.
    User,
    /// The project's: `.skills/`, `.claude/skills/`, `.agents/skills/` in the root.
    Repo,
}

impl SkillGroup {
    pub fn label(self) -> &'static str {
        match self {
            SkillGroup::Provider => "model provider",
            SkillGroup::Nib => "nib",
            SkillGroup::User => "user",
            SkillGroup::Repo => "repo",
        }
    }
}

/// A skill: a folder with a `SKILL.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiSkill {
    pub name: String,
    pub description: String,
    pub group: SkillGroup,
    /// Its `SKILL.md`, on the machine that listed it.
    pub path: String,
}

/// What a chat uses: its context, and either the cloud provider's usage limits / spend or
/// a local model's speed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AiUsage {
    /// Tokens the conversation takes up in the model's context: the last request's prompt and
    /// its reply.
    pub context_tokens: Option<u64>,
    /// The model's context window, in tokens.
    pub context_window: Option<u64>,
    /// The account's usage limits on the provider's side (a subscription's 5-hour / weekly
    /// windows), shortest window first.
    pub limits: Vec<AiLimit>,
    /// What the chat has cost so far at API prices, in USD (an API-key account pays this).
    pub cost_usd: Option<f64>,
    /// A local model: how fast it wrote the last reply, in output tokens per second.
    pub tokens_per_sec: Option<f64>,
}

/// One usage-limit window of the provider's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiLimit {
    /// How long the window is (300: five hours).
    pub window_minutes: u32,
    /// How much of it is used, 0.0 to 1.0.
    pub used: f32,
    /// When it resets, in Unix seconds.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, thiserror::Error)]
#[error("{kind:?}: {msg}")]
pub struct RpcError {
    pub kind: ErrorKind,
    pub msg: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    Conflict,
    ProtocolMismatch,
    BadRequest,
    Io,
}

impl RpcError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Self {
            kind,
            msg: msg.into(),
        }
    }
}

impl From<io::Error> for RpcError {
    fn from(e: io::Error) -> Self {
        let kind = match e.kind() {
            io::ErrorKind::NotFound => ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Io,
        };
        Self::new(kind, e.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// typed helpers
// ---------------------------------------------------------------------------------------------

pub fn send<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let buf = rmp_serde::to_vec_named(msg).map_err(|e| FrameError::Encode(e.to_string()))?;
    write_frame(w, &buf)
}

/// Returns `Ok(None)` on clean EOF at a frame boundary.
pub fn recv<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>, FrameError> {
    match read_frame(r)? {
        None => Ok(None),
        Some(buf) => rmp_serde::from_slice(&buf)
            .map(Some)
            .map_err(|e| FrameError::Decode(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = vec![
            ClientMsg::Request {
                id: 1,
                req: Request::Hello {
                    protocol: PROTOCOL_VERSION,
                    client_version: "x".into(),
                    root: "~".into(),
                    lsp: vec![],
                },
            },
            ClientMsg::Request {
                id: 2,
                req: Request::WriteFile {
                    path: "a.py".into(),
                    data: vec![0, 159, 146, 150],
                    expected_mtime_ns: Some(42),
                },
            },
            ClientMsg::Notify(ClientNotify::Ping { seq: 7 }),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            send(&mut buf, m).unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            let got: ClientMsg = recv(&mut r).unwrap().unwrap();
            assert_eq!(&got, m);
        }
        assert!(recv::<_, ClientMsg>(&mut r).unwrap().is_none());
    }

    #[test]
    fn error_roundtrip() {
        let m = ServerMsg::Response {
            id: 3,
            result: Err(RpcError::new(ErrorKind::Conflict, "changed")),
        };
        let mut buf = Vec::new();
        send(&mut buf, &m).unwrap();
        let got: ServerMsg = recv(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(got, m);
    }
}
