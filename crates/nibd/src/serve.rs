//! The project's apps: what a person would otherwise keep running in a split terminal (a
//! backend and its frontend, TensorBoard, a notebook server), started by nib, each process in
//! a terminal of its own, and gone with the window.
//!
//! An app is the one thing registered, opened, stopped and removed; its servers are the
//! processes it runs. They are defined in `$NIBDIR/apps.json` on the daemon's machine:
//!
//! ```json
//! { "apps": {
//!     "demo": { "title": "Demo", "page": { "server": "web", "path": "/" },
//!               "servers": {
//!                 "api": { "cmd": "uv run app serve --port 8765", "port": 8765 },
//!                 "web": { "cmd": "uv run app frontend --port 8766", "port": 8766, "after": ["api"] } },
//!               "origin": { "by": "agent:claude-3", "at": 1795000000 }, "approved": "…" } } }
//! ```
//!
//! `cmd` is run by the user's shell in a PTY (`pty`), in `cwd` (inside the project), with `env`
//! added. A server is `ready` once its `port` accepts a connection on 127.0.0.1 or `[::1]` (one
//! with no port: as soon as it runs); one that comes `after` others of its app is started when
//! their ports answer, and given up (`exited`, with the reason in its log) if one of them ended
//! or was stopped before its port ever did. A port that already answers before the start means
//! something else has it (another project's server, an older run): the server is not started,
//! and says `port-taken`.
//!
//! An app is a command nib keeps and runs later, so it must not be a way in: an app
//! defined by anybody but the user (an agent, `nib ctl`) waits for the user's approval of its
//! exact definition (`approved` is its hash, set only by `AppOp::Approve` from the user), and
//! any change takes the approval away. The user's requests start only approved apps; an agent
//! may start the app it defined itself (its harness already let it run that command), nothing
//! else does. Agents' requests carry their session's token (`tokens`): a request without one
//! is refused. Every change is written to `$NIBDIR/apps.log.jsonl`.
//!
//! The window asks with `Request::App` and hears of every change as `ServerNotify::App`.
//! Agents ask with `nib-app …` (`nibd app …`, see `cli`): a short-lived process, so it leaves
//! its request as a file in the session's mailbox folder (`app/req/`), which the daemon reads
//! on its next turn and answers in `app/res/`, as the mailbox does for letters.

use crate::pty::Ptys;
use nib_proto::{AppDef, AppInfo, AppOp, AppOrigin, ErrorKind, LogLevel, PtyId, RpcError, ServerDef, ServerInfo, ServerMsg, ServerNotify, ServerState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often ports are asked and agents' requests read.
const TICK: Duration = Duration::from_millis(200);
/// A restarted server waits this long for its old port to be free.
const FREE_PORT: Duration = Duration::from_secs(10);
/// How many lines of a server's output are kept for `nib-app log`.
const LOG_LINES: usize = 5000;

/// Every app, by name, as the file keeps it.
pub type Apps = BTreeMap<String, Stored>;
/// Every server of every app, by key (`app/server`), with `after` as keys too: what runs.
type Defs = BTreeMap<String, ServerDef>;
pub type Shared = Arc<Mutex<Servers>>;

/// An app as `apps.json` keeps it: its definition, who made it, and the user's approval.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Stored {
    #[serde(flatten)]
    pub def: AppDef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<AppOrigin>,
    /// The hash of the definition the user approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<String>,
}

/// Who asks.
#[derive(Debug, Clone, PartialEq)]
pub enum Who {
    /// The person at the window.
    User,
    /// `nib ctl`, or anything else the window passes on.
    Ctl,
    /// An agent nib started, by the name its token was given to.
    Agent(String),
}

impl Who {
    fn by(&self) -> String {
        match self {
            Who::User => "user".into(),
            Who::Ctl => "nib ctl".into(),
            Who::Agent(n) => format!("agent:{n}"),
        }
    }
}

/// A server's key among all that run: `app/server`.
fn key(app: &str, server: &str) -> String {
    format!("{app}/{server}")
}

/// The app a key is of.
fn app_of(key: &str) -> &str {
    key.split_once('/').map_or(key, |(a, _)| a)
}

/// What names a definition, whatever the order its maps were written in.
pub fn hash(def: &AppDef) -> String {
    let text = serde_json::to_vec(def).unwrap_or_default();
    Sha256::digest(&text).iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A server that was started (or asked to) in this session.
struct Run {
    /// As it was when started, `after` as keys: the file may have changed since.
    def: ServerDef,
    /// The ports of the servers it comes after.
    after: Vec<u16>,
    state: ServerState,
    pty: Option<PtyId>,
    /// Restarting: not before its port is free, or this time has come.
    free_by: Option<Instant>,
    /// The end of what it printed.
    log: Arc<Mutex<Printed>>,
}

/// What a server printed, kept as its terminal shows it rather than as the bytes that came:
/// a terminal program moves the cursor and draws over what is there, and Windows' ConPTY
/// repaints the whole screen when the window resizes the terminal, so plain bytes with their
/// escape sequences taken out would say some things twice and run lines together.
pub(crate) struct Printed {
    term: vt100::Parser,
}

impl Default for Printed {
    fn default() -> Self {
        Self { term: vt100::Parser::new(NonZeroU16::new(24).unwrap(), NonZeroU16::new(80).unwrap(), LOG_LINES) }
    }
}

impl Printed {
    /// Output that came from a terminal of `size` (columns, rows) when known.
    fn feed(&mut self, data: &[u8], size: Option<(u16, u16)>) {
        if let Some((cols, rows)) = size.and_then(|(c, r)| Some((NonZeroU16::new(c)?, NonZeroU16::new(r)?))) {
            if self.term.screen().size() != (rows, cols) {
                self.term.set_size(rows, cols);
            }
        }
        self.term.process(data);
    }

    /// A line of nib's own, on a line of its own.
    fn note(&mut self, line: &str) {
        let at_start = self.term.screen().cursor_position().1 == 0;
        self.term.process(format!("{}{line}\r\n", if at_start { "" } else { "\r\n" }).as_bytes());
    }

    /// Every line kept, oldest first: what scrolled away, then the screen; a line the terminal
    /// wrapped is one line, and blank lines at the end are left out.
    fn lines(&mut self) -> Vec<String> {
        let s = self.term.screen_mut();
        let (rows, cols) = s.size();
        let rows = rows.get() as usize;
        s.set_scrollback(usize::MAX);
        let back = s.scrollback();
        let (mut out, mut wrapped, mut next, mut offset) = (Vec::<String>::new(), false, 0usize, back);
        // A screenful at a time, from the oldest down: row `r` at offset `k` is line `back - k + r`.
        loop {
            s.set_scrollback(offset);
            let texts: Vec<String> = s.rows(0, cols.get()).collect();
            for (r, text) in texts.into_iter().enumerate() {
                let i = back - offset + r;
                if i < next {
                    continue;
                }
                next = i + 1;
                match out.last_mut() {
                    Some(last) if wrapped => last.push_str(&text),
                    _ => out.push(text),
                }
                wrapped = s.row_wrapped(r as u16);
            }
            if offset == 0 {
                break;
            }
            offset = offset.saturating_sub(rows);
        }
        s.set_scrollback(0);
        for l in &mut out {
            l.truncate(l.trim_end().len());
        }
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        out
    }
}

pub struct Servers {
    root: PathBuf,
    /// `$NIBDIR/apps.json` (`None`: no NIBDIR on this machine).
    file: Option<PathBuf>,
    /// Where agents leave their requests (`<mailbox>/app`; made by the first one that asks).
    dir: PathBuf,
    /// What every terminal gets: `NIB_SCRATCH`, `NIBDIR`.
    env: Vec<(&'static str, PathBuf)>,
    ptys: Arc<Mutex<Ptys>>,
    tx: Sender<ServerMsg>,
    /// By key (`app/server`).
    runs: BTreeMap<String, Run>,
    me: Weak<Mutex<Servers>>,
}

/// The apps of the project at `root`, and the thread that looks after them for as long as
/// they are kept.
pub fn new(root: &Path, nibdir: Option<&Path>, scratch: Option<&Path>, mailbox: &Path, ptys: Arc<Mutex<Ptys>>, tx: Sender<ServerMsg>) -> Shared {
    let dir = mailbox.join("app");
    let mut env = Vec::new();
    env.extend(scratch.map(|p| ("NIB_SCRATCH", p.to_path_buf())));
    env.extend(nibdir.map(|p| ("NIBDIR", p.to_path_buf())));
    let shared = Arc::new_cyclic(|me| {
        Mutex::new(Servers { root: root.to_path_buf(), file: nibdir.map(|d| d.join("apps.json")), dir, env, ptys, tx, runs: BTreeMap::new(), me: me.clone() })
    });
    let weak = Arc::downgrade(&shared);
    thread::spawn(move || loop {
        thread::sleep(TICK);
        let Some(shared) = weak.upgrade() else { return };
        tick(&shared);
    });
    shared
}

/// Something listens on this machine's `port`: on 127.0.0.1, or on `[::1]` (a dev server told
/// to listen on `localhost` may take only that one).
fn listening(port: u16) -> bool {
    let wait = Duration::from_millis(300);
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), wait).is_ok()
        || TcpStream::connect_timeout(&SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)), wait).is_ok()
}

/// One turn: ask the ports that matter (without holding the lock), move every server on, and
/// answer what agents asked.
fn tick(shared: &Shared) {
    let ports: Vec<u16> = {
        let s = shared.lock().unwrap();
        s.runs.values().filter(|r| r.state.coming()).flat_map(|r| r.def.port.into_iter().chain(r.after.iter().copied())).collect()
    };
    let open: HashMap<u16, bool> = ports.into_iter().map(|p| (p, listening(p))).collect();
    let mut s = shared.lock().unwrap();
    s.advance(&open);
    s.agents();
}

/// `cmd` as the user's shell runs it: a login shell, so that it finds what a terminal finds.
fn shell(cmd: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()), "/C".into(), cmd.into()]
    } else {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()), "-lc".into(), cmd.into()]
    }
}

fn bad(msg: impl Into<String>) -> RpcError {
    RpcError::new(ErrorKind::BadRequest, msg)
}

/// An app's or a server's name: `[a-z0-9_-]{1,32}`.
fn name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Variables that make a program load or run something else than its command says (a
/// library, a startup script, another program of the same name): not settable by an app.
const DENIED_ENV: &[&str] = &[
    "PATH", "NODE_OPTIONS", "PYTHONSTARTUP", "PYTHONPATH", "PYTHONHOME", "PERL5OPT", "PERL5LIB", "RUBYOPT", "BASH_ENV", "ENV", "PROMPT_COMMAND", "ZDOTDIR", "SHELL", "COMSPEC",
    "PATHEXT",
];
const DENIED_ENV_PREFIX: &[&str] = &["LD_", "DYLD_"];

fn env_denied(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    DENIED_ENV.contains(&up.as_str()) || DENIED_ENV_PREFIX.iter().any(|p| up.starts_with(p))
}

/// App `name`'s definition makes sense, alone and with its own servers. The message says
/// what does not.
pub fn validate(name: &str, def: &AppDef) -> Result<(), String> {
    if !name_ok(name) {
        return Err(format!("app name {name:?}: use 1 to 32 of a-z, 0-9, _ and -"));
    }
    if def.servers.is_empty() {
        return Err(format!("{name}: an app runs at least one server"));
    }
    for (s, d) in &def.servers {
        let at = key(name, s);
        if !name_ok(s) {
            return Err(format!("{name}: server name {s:?}: use 1 to 32 of a-z, 0-9, _ and -"));
        }
        if d.cmd.trim().is_empty() {
            return Err(format!("{at}: cmd is empty"));
        }
        if d.port == Some(0) {
            return Err(format!("{at}: port must be 1 to 65535"));
        }
        if let Some(c) = &d.cwd {
            let p = Path::new(c);
            if p.is_absolute() || c.starts_with(['/', '\\', '~']) || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return Err(format!("{at}: cwd {c:?} must be a folder inside the project (relative, no ..)"));
            }
        }
        if let Some(k) = d.env.keys().find(|k| k.is_empty() || k.contains('=')) {
            return Err(format!("{at}: env name {k:?} is not a variable name"));
        }
        if let Some(k) = d.env.keys().find(|k| env_denied(k)) {
            return Err(format!("{at}: env {k} is not allowed: it changes what runs, not how (put what the command needs in the command)"));
        }
        for a in &d.after {
            match def.servers.get(a) {
                _ if a == s => return Err(format!("{at}: comes after itself")),
                None => return Err(format!("{at}: comes after {a}, which is not a server of {name}")),
                Some(o) if o.port.is_none() => return Err(format!("{at}: comes after {a}, which has no port to wait for")),
                Some(_) => {}
            }
        }
    }
    // No server waits, through the others, for itself.
    fn reaches(defs: &BTreeMap<String, ServerDef>, from: &str, to: &str, left: usize) -> bool {
        left > 0 && defs.get(from).is_some_and(|d| d.after.iter().any(|a| a == to || reaches(defs, a, to, left - 1)))
    }
    if let Some(s) = def.servers.keys().find(|n| reaches(&def.servers, n, n, def.servers.len())) {
        return Err(format!("{}: comes after itself (a cycle in after)", key(name, s)));
    }
    if let Some(p) = &def.page {
        match def.servers.get(&p.server) {
            None => return Err(format!("{name}: its page is on {:?}, which is not one of its servers", p.server)),
            Some(d) if d.port.is_none() => return Err(format!("{name}: its page is on {}, which has no port", p.server)),
            Some(_) => {}
        }
        if !p.path.starts_with('/') {
            return Err(format!("{name}: page path {:?} must start with /", p.path));
        }
    }
    Ok(())
}

/// What an app, a server and a page may say: a misspelt key would otherwise be dropped
/// without a word.
const APP_KEYS: &[&str] = &["title", "page", "servers"];
const STORED_KEYS: &[&str] = &["origin", "approved"];
const SERVER_KEYS: &[&str] = &["cmd", "port", "after", "cwd", "env"];
const PAGE_KEYS: &[&str] = &["server", "path"];

fn unknown_key(v: &Value, known: &[&[&str]]) -> Option<String> {
    v.as_object()?.keys().find(|k| !known.iter().any(|ks| ks.contains(&k.as_str()))).cloned()
}

/// App `name`'s definition from JSON (what `nib-app set` sends, an app in the file), strictly.
pub fn parse_def(name: &str, v: &Value) -> Result<AppDef, String> {
    if !v.is_object() {
        return Err(format!("{name}: an app is an object: {{ \"title\", \"page\", \"servers\" }}"));
    }
    if let Some(k) = unknown_key(v, &[APP_KEYS]) {
        return Err(format!("{name}: unknown key {k:?} (known: {})", APP_KEYS.join(", ")));
    }
    if let Some(p) = v.get("page").filter(|p| !p.is_null()) {
        if let Some(k) = unknown_key(p, &[PAGE_KEYS]) {
            return Err(format!("{name}: page: unknown key {k:?} (known: {})", PAGE_KEYS.join(", ")));
        }
    }
    match v.get("servers") {
        Some(Value::Object(m)) => {
            for (s, d) in m {
                if let Some(k) = unknown_key(d, &[SERVER_KEYS]) {
                    return Err(format!("{}: unknown key {k:?} (known: {})", key(name, s), SERVER_KEYS.join(", ")));
                }
            }
        }
        Some(_) => return Err(format!("{name}: \"servers\" must be an object of name: server")),
        None => return Err(format!("{name}: \"servers\" is needed (an object of name: server)")),
    }
    serde_json::from_value(v.clone()).map_err(|e| format!("{name}: {e}"))
}

/// The apps in `file` (none if there is no file), checked.
pub fn read(file: &Path) -> Result<Apps, String> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Apps::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let wrong = |e: String| format!("{}: {e}", file.display());
    let v: Value = serde_json::from_str(&text).map_err(|e| wrong(e.to_string()))?;
    let apps = match v.get("apps") {
        Some(Value::Object(m)) => m,
        Some(_) => return Err(wrong("\"apps\" must be an object of name: app".into())),
        None => return Ok(Apps::new()),
    };
    let mut out = Apps::new();
    for (name, a) in apps {
        if let Some(k) = unknown_key(a, &[APP_KEYS, STORED_KEYS]) {
            return Err(wrong(format!("{name}: unknown key {k:?} (known: {})", [APP_KEYS, STORED_KEYS].concat().join(", "))));
        }
        let mut bare = a.clone();
        if let Some(o) = bare.as_object_mut() {
            o.remove("origin");
            o.remove("approved");
        }
        let def = parse_def(name, &bare).map_err(wrong)?;
        validate(name, &def).map_err(wrong)?;
        let origin = a.get("origin").map(|o| serde_json::from_value(o.clone())).transpose().map_err(|e| wrong(format!("{name}: origin: {e}")))?;
        let approved = a.get("approved").and_then(Value::as_str).map(str::to_string);
        out.insert(name.clone(), Stored { def, origin, approved });
    }
    Ok(out)
}

/// Whole or not at all (written beside, then renamed).
pub fn write(file: &Path, apps: &Apps) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(&json!({ "apps": apps }))?;
    crate::fs_ops::write_whole(file, text + "\n")
}

/// Every server of every app as it runs: by key, `after` as keys.
fn flat(apps: &Apps) -> Defs {
    let mut out = Defs::new();
    for (app, st) in apps {
        for (s, d) in &st.def.servers {
            let after = d.after.iter().map(|a| key(app, a)).collect();
            out.insert(key(app, s), ServerDef { after, ..d.clone() });
        }
    }
    out
}

/// May `who` start `app` as it is defined now? The message says why not.
fn may_run(who: &Who, app: &str, st: &Stored) -> Result<(), String> {
    if st.approved.as_deref() == Some(hash(&st.def).as_str()) {
        return Ok(());
    }
    let by = st.origin.as_ref().map_or("somebody", |o| o.by.as_str());
    match who {
        Who::Agent(_) if st.origin.as_ref().is_some_and(|o| o.by == who.by()) => Ok(()),
        Who::User => Err(format!("{app} is not approved yet (defined by {by}): open it from the Apps panel to see its commands and approve them")),
        _ => Err(format!("{app} waits for the user's approval (defined by {by}): they see its commands and approve them the first time they open it from the Apps panel")),
    }
}

impl Servers {
    fn file(&self) -> Result<&Path, String> {
        self.file.as_deref().ok_or_else(|| "no NIBDIR on this machine to keep apps.json in (see the daemon log)".to_string())
    }

    fn apps(&self) -> Result<Apps, String> {
        read(self.file()?)
    }

    fn save(&self, apps: &Apps) -> Result<(), String> {
        write(self.file()?, apps).map_err(|e| format!("writing apps.json: {e}"))
    }

    /// One line in `$NIBDIR/apps.log.jsonl`: who did what to which app.
    fn audit(&self, who: &Who, did: &str, app: &str, def: Option<&AppDef>) {
        let Some(file) = self.file.as_ref().map(|f| f.with_file_name("apps.log.jsonl")) else { return };
        let line = json!({ "at": now(), "by": who.by(), "did": did, "app": app, "def": def });
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn info(&self, app: &str, st: Option<&Stored>) -> AppInfo {
        let def = st.map(|s| s.def.clone()).unwrap_or_default();
        let prefix = format!("{app}/");
        let mut names: BTreeSet<String> = def.servers.keys().cloned().collect();
        names.extend(self.runs.keys().filter_map(|k| k.strip_prefix(&prefix)).map(str::to_string));
        let servers = names
            .into_iter()
            .map(|s| {
                let run = self.runs.get(&key(app, &s));
                // A server the file no longer has, still running: as it was started.
                let def = def.servers.get(&s).cloned().or_else(|| {
                    run.map(|r| ServerDef { after: r.def.after.iter().map(|a| a.strip_prefix(&prefix).unwrap_or(a).to_string()).collect(), ..r.def.clone() })
                });
                ServerInfo { def: def.unwrap_or_default(), state: run.map_or(ServerState::Stopped, |r| r.state), pty: run.and_then(|r| r.pty), name: s }
            })
            .collect();
        let h = hash(&def);
        AppInfo {
            name: app.to_string(),
            approved: st.is_some_and(|s| s.approved.as_deref() == Some(h.as_str())),
            origin: st.and_then(|s| s.origin.clone()),
            hash: h,
            def,
            servers,
            removed: false,
        }
    }

    /// Every app defined, and any still running whose definition is gone from the file.
    fn list(&self, apps: &Apps) -> Vec<AppInfo> {
        let mut names: BTreeSet<&str> = apps.keys().map(String::as_str).collect();
        names.extend(self.runs.keys().map(|k| app_of(k)));
        names.into_iter().map(|n| self.info(n, apps.get(n))).collect()
    }

    /// Tell the window how `app` is now.
    fn tell(&self, app: &str) {
        let apps = self.apps().unwrap_or_default();
        let _ = self.tx.send(ServerMsg::Notify(ServerNotify::App(self.info(app, apps.get(app)))));
    }

    fn set_state(&mut self, key: &str, state: ServerState) {
        if let Some(r) = self.runs.get_mut(key) {
            r.state = state;
        }
        self.tell(app_of(key));
    }

    /// The window asks: `user`, the person at it; otherwise `nib ctl` or the like.
    pub fn request(&mut self, op: AppOp, user: bool) -> Result<Vec<AppInfo>, RpcError> {
        self.act(op, &if user { Who::User } else { Who::Ctl }).map_err(bad)
    }

    fn act(&mut self, op: AppOp, who: &Who) -> Result<Vec<AppInfo>, String> {
        let mut apps = self.apps()?;
        let known = |apps: &Apps, name: &str| match apps.contains_key(name) {
            true => Ok(()),
            false if apps.is_empty() => Err(format!("no app named {name:?}: none is registered yet (nib-app set APP --json FILE)")),
            false => Err(format!("no app named {name:?}; there are: {}", apps.keys().cloned().collect::<Vec<_>>().join(", "))),
        };
        match op {
            AppOp::List => {}
            AppOp::Set { name, def } => {
                validate(&name, &def)?;
                // The user's own definition is approved as they save it; anybody else's waits.
                let approved = (*who == Who::User).then(|| hash(&def));
                self.audit(who, "set", &name, Some(&def));
                apps.insert(name.clone(), Stored { def, origin: Some(AppOrigin { by: who.by(), at: now() }), approved });
                self.save(&apps)?;
                self.tell(&name);
            }
            AppOp::Remove { name } => {
                known(&apps, &name)?;
                apps.remove(&name);
                self.save(&apps)?;
                self.audit(who, "rm", &name, None);
                let keys: Vec<String> = self.runs.keys().filter(|k| app_of(k) == name).cloned().collect();
                for k in &keys {
                    self.stop(k);
                    self.runs.remove(k);
                }
                let gone = AppInfo { name, removed: true, ..Default::default() };
                let _ = self.tx.send(ServerMsg::Notify(ServerNotify::App(gone)));
            }
            AppOp::Up { names } => {
                if apps.is_empty() {
                    return Err("no apps are registered yet (nib-app set APP --json FILE)".into());
                }
                let names: Vec<String> = if names.is_empty() {
                    // All: those this one may start; the others are said, not started.
                    let (go, wait): (Vec<&String>, Vec<&String>) = apps.keys().partition(|n| may_run(who, n, &apps[*n]).is_ok());
                    if go.is_empty() {
                        return Err(format!("no app is approved yet ({}): open each from the Apps panel to approve it", wait.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
                    }
                    if !wait.is_empty() {
                        let msg = format!("not started, waiting for approval: {}", wait.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
                        let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg }));
                    }
                    go.into_iter().cloned().collect()
                } else {
                    for n in &names {
                        known(&apps, n)?;
                        may_run(who, n, &apps[n])?;
                    }
                    names
                };
                let defs = flat(&apps);
                for app in names {
                    for s in apps[&app].def.servers.keys() {
                        self.up(&key(&app, s), &defs, None);
                    }
                }
            }
            AppOp::Down { names } => {
                let names: Vec<String> = match names.is_empty() {
                    true => self.runs.keys().map(|k| app_of(k).to_string()).collect::<BTreeSet<_>>().into_iter().collect(),
                    false => names,
                };
                for name in names {
                    let keys: Vec<String> = self.runs.keys().filter(|k| app_of(k) == name).cloned().collect();
                    if keys.is_empty() {
                        known(&apps, &name)?;
                    }
                    for k in keys {
                        self.stop(&k);
                    }
                }
            }
            AppOp::Restart { name, server } => {
                known(&apps, &name)?;
                may_run(who, &name, &apps[&name])?;
                let servers: Vec<String> = match server {
                    Some(s) if apps[&name].def.servers.contains_key(&s) => vec![s],
                    Some(s) => return Err(format!("{name} has no server {s:?}; it has: {}", apps[&name].def.servers.keys().cloned().collect::<Vec<_>>().join(", "))),
                    None => apps[&name].def.servers.keys().cloned().collect(),
                };
                let defs = flat(&apps);
                // Not said to be stopped on the way: the window keeps their terminals.
                for s in &servers {
                    if let Some(r) = self.runs.get_mut(&key(&name, s)) {
                        if let Some(id) = r.pty.take() {
                            self.ptys.lock().unwrap().close(id);
                        }
                        r.state = ServerState::Stopped;
                    }
                }
                for s in &servers {
                    self.up(&key(&name, s), &defs, Some(Instant::now() + FREE_PORT));
                }
            }
            AppOp::Approve { name, hash: shown } => {
                if *who != Who::User {
                    return Err("only the user approves an app, from the Apps panel".into());
                }
                known(&apps, &name)?;
                let st = apps.get_mut(&name).expect("known");
                if hash(&st.def) != shown {
                    return Err(format!("{name} changed since it was shown: look at it again"));
                }
                st.approved = Some(shown);
                let def = st.def.clone();
                self.save(&apps)?;
                self.audit(who, "approve", &name, Some(&def));
                self.tell(&name);
            }
        }
        Ok(self.list(&apps))
    }

    /// Ask for server `key` to run: it waits its turn (`advance` starts it). One already
    /// running or on its way is left alone.
    fn up(&mut self, key: &str, defs: &Defs, free_by: Option<Instant>) {
        let Some(def) = defs.get(key) else { return };
        if self.runs.get(key).is_some_and(|r| r.state.coming() || r.state == ServerState::Ready) {
            return;
        }
        let after = def.after.iter().filter_map(|a| defs.get(a).and_then(|d| d.port)).collect();
        let log = self.runs.get(key).map(|r| r.log.clone()).unwrap_or_default();
        self.runs.insert(key.to_string(), Run { def: def.clone(), after, state: ServerState::Waiting, pty: None, free_by, log });
        self.tell(app_of(key));
    }

    /// Stop server `key` if it runs or is about to.
    fn stop(&mut self, key: &str) {
        let Some(r) = self.runs.get_mut(key) else { return };
        if let Some(id) = r.pty.take() {
            self.ptys.lock().unwrap().close(id);
        }
        if r.state != ServerState::Stopped {
            self.set_state(key, ServerState::Stopped);
        }
    }

    /// The window closed terminal `id`: if it was a server's, the server is stopped.
    pub fn closed(&mut self, id: PtyId) {
        if let Some(k) = self.runs.iter().find(|(_, r)| r.pty == Some(id)).map(|(n, _)| n.clone()) {
            if let Some(r) = self.runs.get_mut(&k) {
                r.pty = None;
            }
            self.set_state(&k, ServerState::Stopped);
        }
    }

    /// The process in terminal `id` ended by itself.
    fn exited(&mut self, id: PtyId, code: Option<i32>) {
        if let Some(k) = self.runs.iter().find(|(_, r)| r.pty == Some(id)).map(|(n, _)| n.clone()) {
            if let Some(r) = self.runs.get_mut(&k) {
                r.pty = None;
            }
            self.set_state(&k, ServerState::Exited(code));
        }
    }

    /// Move on every server that is on its way, given which ports answer now.
    fn advance(&mut self, open: &HashMap<u16, bool>) {
        let coming: Vec<String> = self.runs.iter().filter(|(_, r)| r.state.coming()).map(|(n, _)| n.clone()).collect();
        for k in coming {
            let Some(r) = self.runs.get_mut(&k) else { continue };
            // A port nobody asked about this turn (the server was asked for a moment ago) is
            // not known to be free: next turn.
            let own = match r.def.port {
                Some(p) => match open.get(&p) {
                    Some(o) => Some(*o),
                    None => continue,
                },
                None => None,
            };
            match r.state {
                ServerState::Starting if own == Some(true) => self.set_state(&k, ServerState::Ready),
                ServerState::Waiting => {
                    if let Some(by) = r.free_by {
                        if own == Some(true) && Instant::now() < by {
                            continue;
                        }
                        r.free_by = None;
                    }
                    let (answer, after) = (r.after.iter().all(|p| open.get(p).copied().unwrap_or(false)), r.def.after.clone());
                    if !answer {
                        // One it waits for will not come (it ended, or was stopped) and
                        // nothing answers on its port: waiting on would be for ever.
                        let lost = after.iter().find_map(|a| {
                            let dep = self.runs.get(a)?;
                            let silent = dep.def.port.is_none_or(|p| open.get(&p) == Some(&false));
                            (silent && matches!(dep.state, ServerState::Exited(_) | ServerState::Stopped)).then(|| (a.clone(), dep.state))
                        });
                        if let Some((dep, state)) = lost {
                            let dep = dep.split_once('/').map_or(dep.as_str(), |(_, s)| s).to_string();
                            self.give_up(&k, &format!("not started: {dep}, which it comes after, is {} and its port does not answer", state.name()));
                        }
                        continue;
                    }
                    match own {
                        Some(true) => self.set_state(&k, ServerState::PortTaken),
                        _ => self.start(&k),
                    }
                }
                _ => {}
            }
        }
    }

    /// Server `key` will not be started, and why: in its log, and said to the window.
    fn give_up(&mut self, key: &str, why: &str) {
        if let Some(r) = self.runs.get(key) {
            r.log.lock().unwrap().note(&format!("nib: {why}"));
        }
        let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg: format!("{key}: {why}") }));
        self.set_state(key, ServerState::Exited(None));
    }

    /// Where `cwd` (relative to the root) is, if it is a folder inside the project.
    fn dir_of(&self, cwd: Option<&str>) -> Result<PathBuf, String> {
        let Some(cwd) = cwd else { return Ok(self.root.clone()) };
        let dir = nib_proto::path::canonical(&self.root.join(cwd)).map_err(|e| format!("cwd {cwd:?}: {e}"))?;
        match dir.starts_with(&self.root) && dir.is_dir() {
            true => Ok(dir),
            false => Err(format!("cwd {cwd:?} is not a folder inside the project")),
        }
    }

    /// Run server `key`'s command in a terminal of its own.
    fn start(&mut self, key: &str) {
        let Some(r) = self.runs.get(key) else { return };
        let (def, log) = (r.def.clone(), r.log.clone());
        let (ftx, frx) = mpsc::channel::<ServerMsg>();
        let opened = self.dir_of(def.cwd.as_deref()).and_then(|cwd| {
            let mut env: Vec<(&str, &Path)> = self.env.iter().map(|(k, v)| (*k, v.as_path())).collect();
            env.extend(def.env.iter().filter(|(k, _)| !env_denied(k)).map(|(k, v)| (k.as_str(), Path::new(v))));
            self.ptys.lock().unwrap().open(&cwd, Some(shell(&def.cmd)), 80, 24, &env, ftx).map_err(|e| e.msg)
        });
        let id = match opened {
            Ok(id) => id,
            Err(e) => {
                let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg: format!("{key}: {e}") }));
                return self.set_state(key, ServerState::Exited(None));
            }
        };
        if let Some(r) = self.runs.get_mut(key) {
            r.pty = Some(id);
        }
        // The window hears of the terminal before anything it prints.
        self.set_state(key, if def.port.is_some() { ServerState::Starting } else { ServerState::Ready });
        let (tx, me, ptys) = (self.tx.clone(), self.me.clone(), self.ptys.clone());
        thread::spawn(move || {
            for msg in frx {
                match &msg {
                    ServerMsg::Notify(ServerNotify::PtyOutput { data, .. }) => {
                        // At the size the window has made the terminal (it repaints after a resize).
                        let size = ptys.lock().unwrap().size(id);
                        log.lock().unwrap().feed(data, size);
                    }
                    ServerMsg::Notify(ServerNotify::PtyExit { code, .. }) => {
                        if let Some(s) = me.upgrade() {
                            s.lock().unwrap().exited(id, *code);
                        }
                    }
                    _ => {}
                }
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
    }

    /// The last `lines` lines server `key` printed, as plain text.
    fn log(&self, key: &str, lines: usize) -> Option<String> {
        let all = self.runs.get(key)?.log.lock().unwrap().lines();
        Some(all[all.len().saturating_sub(lines)..].join("\n"))
    }

    /// Answer the requests agents left (`cli`), oldest first. One without the token of an
    /// agent nib started is refused.
    fn agents(&mut self) {
        let dir = self.dir.clone();
        let taken = crate::rpc::take(&dir);
        if taken.is_empty() {
            return;
        }
        for (name, req) in taken {
            let req = req.unwrap_or_default();
            let args: Vec<String> = serde_json::from_value(req["args"].clone()).unwrap_or_default();
            let (out, code) = match crate::tokens::who(req["token"].as_str().unwrap_or_default()) {
                Some(agent) => self.run(&args, &Who::Agent(agent)),
                None => {
                    let msg = format!("refused a request for the apps without an agent's token: {}", args.join(" "));
                    let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg }));
                    ("nib-app: refused: the request does not come from an agent nib started (no valid $NIB_TOKEN)".to_string(), 1)
                }
            };
            crate::rpc::answer(&dir, &name, &json!({ "out": out, "code": code }).to_string());
        }
        // What nobody came for (a command that gave up, or was stopped).
        crate::rpc::sweep(&dir);
    }

    /// The server of `app` that `log` means when none is named: its page's, else its first.
    fn main_server(&self, app: &str) -> Option<String> {
        let apps = self.apps().ok()?;
        let def = &apps.get(app)?.def;
        def.page.as_ref().map(|p| p.server.clone()).or_else(|| def.servers.keys().next().cloned())
    }

    /// What `nib-app ARGS` does, here in the daemon: what to print, and the exit code.
    fn run(&mut self, args: &[String], who: &Who) -> (String, i32) {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let op = match words.as_slice() {
            ["list"] => AppOp::List,
            ["set", name, text] => {
                let def = serde_json::from_str::<Value>(text).map_err(|e| format!("not JSON: {e}")).and_then(|v| parse_def(name, &v));
                match def {
                    Ok(def) => AppOp::Set { name: name.to_string(), def },
                    Err(e) => return (format!("nib-app set: {e}"), 2),
                }
            }
            ["rm", name] => AppOp::Remove { name: name.to_string() },
            ["up" | "open", n @ ..] => AppOp::Up { names: names(n) },
            ["down", n @ ..] => AppOp::Down { names: names(n) },
            ["restart", name] => AppOp::Restart { name: name.to_string(), server: None },
            ["restart", name, server] => AppOp::Restart { name: name.to_string(), server: Some(server.to_string()) },
            ["log", app, rest @ ..] => {
                let (server, lines) = match rest {
                    [] => (None, 40),
                    ["--lines", n] => (None, n.parse().unwrap_or(40)),
                    [s] => (Some(s.to_string()), 40),
                    [s, "--lines", n] => (Some(s.to_string()), n.parse().unwrap_or(40)),
                    _ => return (USAGE.to_string(), 2),
                };
                let Some(server) = server.or_else(|| self.main_server(app)) else { return (format!("nib-app: no app named {app:?}"), 1) };
                return match self.log(&key(app, &server), lines) {
                    Some(text) => (text, 0),
                    None => (format!("nib-app: {app} › {server} has not run in this session"), 1),
                };
            }
            // `cli` asks until the servers it started are no longer on their way: 3 and those
            // still coming, else 0 and those that did not get to `ready`.
            ["settled", n @ ..] => {
                let mine = |k: &str| n.is_empty() || n.contains(&app_of(k));
                let coming: Vec<&str> = self.runs.iter().filter(|(k, r)| r.state.coming() && mine(k)).map(|(k, _)| k.as_str()).collect();
                if !coming.is_empty() {
                    return (coming.join(" "), 3);
                }
                let failed: Vec<String> = self
                    .runs
                    .iter()
                    .filter(|(k, r)| mine(k) && matches!(r.state, ServerState::Exited(_) | ServerState::PortTaken))
                    .map(|(k, r)| format!("{k} ({})", r.state.name()))
                    .collect();
                return (failed.join(", "), 0);
            }
            _ => return (USAGE.to_string(), 2),
        };
        match self.act(op, who) {
            Ok(list) => (table(&list), 0),
            Err(e) => (format!("nib-app: {e}"), 1),
        }
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        let mut ptys = self.ptys.lock().unwrap();
        for r in self.runs.values_mut() {
            if let Some(id) = r.pty.take() {
                ptys.close(id);
            }
        }
    }
}

/// An app a few lines: its name, title, approval and page; then each server's name, state,
/// port, what it waits for, and its command.
fn table(list: &[AppInfo]) -> String {
    if list.is_empty() {
        return "no apps are registered (nib-app set APP --json FILE)".to_string();
    }
    let mut out = Vec::new();
    for a in list {
        let approval = match (&a.approved, &a.origin) {
            (true, _) => "approved".to_string(),
            (false, Some(o)) => format!("waits for the user's approval (defined by {})", o.by),
            (false, None) => "waits for the user's approval".to_string(),
        };
        let page = a.def.page.as_ref().map(|p| format!("  page {}{}", p.server, p.path)).unwrap_or_default();
        out.push(format!("{}  \"{}\"  {}  {approval}{page}", a.name, a.title(), a.state().name()));
        let w = a.servers.iter().map(|s| s.name.len()).max().unwrap_or(0);
        for s in &a.servers {
            let port = s.def.port.map(|p| format!(":{p}")).unwrap_or_default();
            let after = if s.def.after.is_empty() { String::new() } else { format!("after {}  ", s.def.after.join(",")) };
            out.push(format!("  {:w$}  {:11} {port:6}  {after}{}", s.name, s.state.name(), s.def.cmd));
        }
    }
    out.join("\n")
}

const USAGE: &str = "usage:
  nib-app list                        the project's apps: their servers, states, ports, commands
  nib-app set APP --json FILE|-       register (or replace) an app, whole, from JSON:
        { \"title\": \"Demo\", \"page\": { \"server\": \"web\", \"path\": \"/\" },
          \"servers\": { \"api\": { \"cmd\": \"…\", \"port\": 8765 },
                       \"web\": { \"cmd\": \"…\", \"port\": 8766, \"after\": [\"api\"], \"cwd\": \"web\", \"env\": { \"K\": \"V\" } } } }
        kept in $NIBDIR/apps.json; the user approves it the first time they open it
  nib-app rm APP                      stop it and forget it
  nib-app open APP…                   start them and wait until each server is ready or has failed
  nib-app up [APP…]                   the same (none named: every app you may start)
  nib-app down [APP…]                 stop them (none named: all)
  nib-app restart APP [SERVER]        stop its servers (or one), wait for the ports to be free, start them
  nib-app log APP [SERVER] [--lines N]  the end of what it printed (default: its page's server, 40 lines)
open, up and restart wait at most 60 s (--timeout SECONDS; 0: don't wait).
states: stopped, waiting (for the servers it comes after), starting, ready, exited(CODE),
port-taken (something else already listens on its port: it was not started).";

/// Leave `args` for the daemon, with this agent's token, and wait for its answer: what to
/// print, and the exit code.
fn ask(dir: &Path, token: &str, args: &[String]) -> (String, i32) {
    let body = json!({ "token": token, "args": args }).to_string();
    match crate::rpc::ask(dir, &body, Duration::from_secs(15)) {
        Ok(Some(v)) => (v["out"].as_str().unwrap_or_default().to_string(), v["code"].as_i64().unwrap_or(1) as i32),
        Ok(None) => ("nib-app: nib did not answer (is its window still open?)".to_string(), 1),
        Err(e) => (format!("nib-app: {e}"), 1),
    }
}

/// What to add to the list after `open` / `up` / `restart` waited, given the last `settled`
/// answer: servers still on their way when the time ran out (exit 3), or that ended or found
/// their port taken (exit 1). None: all are ready (or no wait was asked for).
fn after_wait(last: &(String, i32), timeout: u64) -> Option<(String, i32)> {
    match last {
        (coming, 3) if timeout > 0 => Some((format!("nib-app: not ready after {timeout} s: {coming}; `nib-app list` to see when, `nib-app log APP SERVER` for what it printed, --timeout SECONDS to wait longer"), 3)),
        (failed, 0) if !failed.is_empty() => Some((format!("nib-app: not ready: {failed}; `nib-app log APP SERVER` says why"), 1)),
        _ => None,
    }
}

/// `set APP --json FILE|-` as the daemon takes it: `set APP TEXT`, the file (or stdin) read
/// here, where the agent is.
fn json_arg(args: &mut Vec<String>, stdin: impl FnOnce() -> std::io::Result<String>) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("set") {
        return Ok(());
    }
    let (Some(name), Some("--json"), Some(from), 4) = (args.get(1).cloned(), args.get(2).map(String::as_str), args.get(3).cloned(), args.len()) else {
        return Err("write it as: nib-app set APP --json FILE (or - for stdin)".into());
    };
    let text = match from.as_str() {
        "-" => stdin().map_err(|e| format!("reading stdin: {e}"))?,
        file => fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?,
    };
    *args = vec!["set".into(), name, text];
    Ok(())
}

/// `nibd app …`, run by an agent as `nib-app`. Returns the exit code; prints to stdout / stderr.
pub fn cli(args: &[String]) -> i32 {
    if let Some(code) = crate::cli::help(args, USAGE, true) {
        return code;
    }
    let Some(dir) = crate::cli::rpc_dir("app") else {
        eprintln!("nib-app: only works inside an agent that nib started (NIB_MAILBOX is not set, or holds no outbox/)");
        return 2;
    };
    crate::tokens::private(&dir);
    let token = std::env::var(crate::tokens::VAR).unwrap_or_default();
    // `--timeout S` is this side's: how long `open` / `up` / `restart` wait for the servers.
    let mut args = args.to_vec();
    let mut timeout = 60;
    if let Some(i) = args.iter().position(|a| a == "--timeout") {
        match args.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
            Some(s) => timeout = s,
            None => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
        args.drain(i..i + 2);
    }
    if let Err(e) = json_arg(&mut args, || {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map(|_| s)
    }) {
        eprintln!("nib-app set: {e}");
        return 2;
    }
    let (mut out, mut code) = ask(&dir, &token, &args);
    let started = match args.first().map(String::as_str) {
        Some("up" | "open") => Some(args[1..].to_vec()),
        Some("restart") => Some(args.iter().skip(1).take(1).cloned().collect()),
        _ => None,
    };
    if let (0, Some(names)) = (code, started) {
        let t = Instant::now();
        let settled: Vec<String> = std::iter::once("settled".to_string()).chain(names).collect();
        let mut last = ask(&dir, &token, &settled);
        while timeout > 0 && t.elapsed() < Duration::from_secs(timeout) && last.1 == 3 {
            thread::sleep(Duration::from_millis(300));
            last = ask(&dir, &token, &settled);
        }
        (out, code) = ask(&dir, &token, &["list".to_string()]);
        // Not done waiting is not done: say so, and with an exit code a script can see.
        if code == 0 {
            if let Some(note) = after_wait(&last, timeout) {
                out = format!("{out}\n{}", note.0);
                code = note.1;
            }
        }
    }
    crate::cli::print((out, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib_proto::AppPage;

    fn def(cmd: &str, port: Option<u16>, after: &[&str]) -> ServerDef {
        ServerDef { cmd: cmd.into(), port, after: after.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn app(servers: &[(&str, ServerDef)], page: Option<(&str, &str)>) -> AppDef {
        AppDef {
            title: None,
            page: page.map(|(server, path)| AppPage { server: server.into(), path: path.into() }),
            servers: servers.iter().map(|(n, d)| (n.to_string(), d.clone())).collect(),
        }
    }

    /// A command that runs for a while, on this system.
    fn sleeper() -> &'static str {
        if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nibd-app-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        nib_proto::path::canonical(&d).unwrap()
    }

    #[test]
    fn apps_are_checked() {
        let ok = app(&[("api", def("serve", Some(8765), &[])), ("web", def("front", Some(8766), &["api"]))], Some(("web", "/")));
        assert_eq!(validate("demo", &ok), Ok(()));
        let err = |name: &str, d: AppDef| validate(name, &d).unwrap_err();
        let one = |d: ServerDef| app(&[("a", d)], None);
        assert!(err("Demo", ok.clone()).contains("a-z"));
        assert!(err(&"a".repeat(33), ok.clone()).contains("1 to 32"));
        assert!(err("demo", app(&[], None)).contains("at least one server"));
        assert!(err("demo", app(&[("Bad", def("x", None, &[]))], None)).contains("server name"));
        assert!(err("demo", one(def(" ", None, &[]))).contains("cmd is empty"));
        assert!(err("demo", one(def("x", Some(0), &[]))).contains("1 to 65535"));
        assert!(err("demo", one(def("x", None, &["b"]))).contains("not a server of demo"));
        assert!(err("demo", app(&[("a", def("x", None, &["b"])), ("b", def("x", None, &[]))], None)).contains("no port"));
        assert!(err("demo", one(def("x", Some(1), &["a"]))).contains("after itself"));
        assert!(err("demo", app(&[("a", def("x", Some(1), &["b"])), ("b", def("x", Some(2), &["c"])), ("c", def("x", Some(3), &["a"]))], None)).contains("cycle"));
        for cwd in ["../other", "/etc", "a/../../b", "~/x"] {
            assert!(err("demo", one(ServerDef { cwd: Some(cwd.into()), ..def("x", None, &[]) })).contains("inside the project"), "{cwd}");
        }
        assert!(err("demo", one(ServerDef { env: [("A=B".to_string(), "1".to_string())].into(), ..def("x", None, &[]) })).contains("variable name"));
        // What would make the command run something else is refused, whatever its case.
        for k in ["LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "NODE_OPTIONS", "PYTHONSTARTUP", "Path", "bash_env", "PROMPT_COMMAND"] {
            assert!(err("demo", one(ServerDef { env: [(k.to_string(), "x".to_string())].into(), ..def("x", None, &[]) })).contains("not allowed"), "{k}");
        }
        assert_eq!(validate("demo", &one(ServerDef { env: [("API_URL".to_string(), "x".to_string())].into(), ..def("x", None, &[]) })), Ok(()));
        // The page is on one of its servers, one with a port, at a path.
        assert!(err("demo", app(&[("a", def("x", Some(1), &[]))], Some(("b", "/")))).contains("not one of its servers"));
        assert!(err("demo", app(&[("a", def("x", None, &[]))], Some(("a", "/")))).contains("no port"));
        assert!(err("demo", app(&[("a", def("x", Some(1), &[]))], Some(("a", "index.html")))).contains("start with /"));
    }

    #[test]
    fn the_file_is_read_strictly_and_written_back_the_same() {
        let d = temp("file");
        let file = d.join("apps.json");
        assert_eq!(read(&file), Ok(Apps::new()), "no file: no apps");
        let text = r#"{ "apps": { "clarity": {
            "title": "Clarity", "page": { "server": "frontend", "path": "/" },
            "servers": {
              "backend":  { "cmd": "uv run clarity-desktop serve --no-open --port 8765", "port": 8765 },
              "frontend": { "cmd": "uv run clarity-desktop frontend --port 8766", "port": 8766, "after": ["backend"],
                            "env": { "API": "http://127.0.0.1:8765" }, "cwd": "web" } },
            "origin": { "by": "agent:claude-3", "at": 1795000000 } } } }"#;
        fs::write(&file, text).unwrap();
        let got = read(&file).unwrap();
        let c = &got["clarity"];
        assert_eq!(c.def.servers["backend"], def("uv run clarity-desktop serve --no-open --port 8765", Some(8765), &[]));
        assert_eq!((c.def.title.as_deref(), c.def.page.as_ref().map(|p| p.server.as_str()), c.def.servers["frontend"].cwd.as_deref()), (Some("Clarity"), Some("frontend"), Some("web")));
        assert_eq!(c.def.servers["frontend"].env["API"], "http://127.0.0.1:8765");
        assert_eq!((c.origin.as_ref().unwrap().by.as_str(), c.approved.as_deref()), ("agent:claude-3", None));
        write(&file, &got).unwrap();
        assert_eq!(read(&file).unwrap(), got);
        assert!(!fs::read_to_string(&file).unwrap().contains("\"env\": {}"), "what is not set is not written");
        for (wrong, says) in [
            (r#"{ "apps": { "a": { "servers": { "s": { "cmd": "x", "prot": 1 } } } } }"#, "unknown key \"prot\""),
            (r#"{ "apps": { "a": { "titel": "A", "servers": {} } } }"#, "unknown key \"titel\""),
            (r#"{ "apps": { "a": { "servers": { "s": { "cmd": "x", "port": 70000 } } } } }"#, "a: "),
            (r#"{ "apps": { "a": { "servers": { "s": { "port": 1 } } } } }"#, "cmd"),
            (r#"{ "apps": { "a": { "title": "A" } } }"#, "\"servers\" is needed"),
            (r#"{ "apps": [] }"#, "must be an object"),
            ("{ not json", "apps.json"),
        ] {
            fs::write(&file, wrong).unwrap();
            let e = read(&file).unwrap_err();
            assert!(e.contains(says), "{wrong}: {e}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_hash_names_the_definition_and_nothing_else() {
        let a = app(&[("api", def("serve", Some(8765), &[]))], None);
        let mut b = a.clone();
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(hash(&a).len(), 64);
        b.servers.get_mut("api").unwrap().cmd = "serve; curl evil | sh".into();
        assert_ne!(hash(&a), hash(&b));
        // The SHA-256 of the definition as compact JSON, fields in their order, maps by key and
        // nothing that is empty: what scripts/e2e.sh computes for the apps it writes approved.
        let demo = AppDef {
            title: Some("Demo".into()),
            page: Some(AppPage { server: "web".into(), path: "/".into() }),
            servers: [("web".to_string(), def("npm run dev", Some(8766), &["api"])), ("api".to_string(), def("uv run api --port 8765", Some(8765), &[]))].into(),
        };
        assert_eq!(hash(&demo), "0ab1a960c97e75501a7d87cf6975a7c425e1a224a34eaf658b19bec0e6547046");
    }

    #[test]
    fn set_reads_its_json_where_the_agent_is() {
        let d = temp("json");
        let f = d.join("app.json");
        fs::write(&f, r#"{ "servers": { "s": { "cmd": "x" } } }"#).unwrap();
        let s = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut args = s(&["set", "demo", "--json", &f.display().to_string()]);
        json_arg(&mut args, || unreachable!()).unwrap();
        assert_eq!(args, s(&["set", "demo", r#"{ "servers": { "s": { "cmd": "x" } } }"#]));
        let mut args = s(&["set", "demo", "--json", "-"]);
        json_arg(&mut args, || Ok("{}".into())).unwrap();
        assert_eq!(args, s(&["set", "demo", "{}"]));
        assert!(json_arg(&mut s(&["set", "demo", "--cmd", "x"]), || unreachable!()).unwrap_err().contains("--json"));
        assert!(json_arg(&mut s(&["set", "demo", "--json", "/no/such/file.json"]), || unreachable!()).is_err());
        let mut list = s(&["list"]);
        json_arg(&mut list, || unreachable!()).unwrap();
        assert_eq!(list, s(&["list"]));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn output_is_kept_as_the_terminal_shows_it() {
        type Chunk<'a> = (&'a [u8], Option<(u16, u16)>);
        let lines = |chunks: &[Chunk]| {
            let mut p = Printed::default();
            chunks.iter().for_each(|(d, s)| p.feed(d, *s));
            p.lines()
        };
        // Colours, titles and links go; lines stay lines.
        assert_eq!(lines(&[(b"\x1b[1;32mINFO\x1b[0m: ready\r\n\x1b]0;title\x07next\x1b]8;;x\x1b\\.", None)]), ["INFO: ready", "next."]);
        // ConPTY moves to the next row instead of a newline (Node's error, as captured).
        assert_eq!(lines(&[(b"Unterminated string constant\x1b[6;1HSyntaxError: Invalid", None)]), ["Unterminated string constant", "", "", "", "", "SyntaxError: Invalid"]);
        // A repaint after a resize draws over what is there, rather than saying it twice.
        let first: &[u8] = b"Serving notebooks from local directory: C:\\Users\\me\\src\\explorer_v2\\reports\r\nrunning at http://127.0.0.1:8899/lab\r\n";
        let repaint: &[u8] = b"\x1b[?25l\x1b[H\x1b[2JServing notebooks from local directory: C:\\Users\\me\\src\\explorer_v2\\reports\x1b[2;1Hrunning at http://127.0.0.1:8899/lab\x1b[3;1H\x1b[?25h";
        let got = lines(&[(first, Some((60, 24))), (repaint, Some((100, 24)))]);
        assert_eq!(got, ["Serving notebooks from local directory: C:\\Users\\me\\src\\explorer_v2\\reports", "running at http://127.0.0.1:8899/lab"]);
        // A line the terminal wrapped is one line again.
        assert_eq!(lines(&[(b"0123456789abcdef\r\nnext\r\n", Some((10, 5)))]), ["0123456789abcdef", "next"]);
        // What scrolled off the screen is kept, in order.
        let many: String = (0..60).map(|i| format!("line {i}\r\n")).collect();
        let got = lines(&[(many.as_bytes(), Some((80, 24)))]);
        assert_eq!((got.len(), got[0].as_str(), got[59].as_str()), (60, "line 0", "line 59"));
        // nib's own word starts a line of its own.
        let mut p = Printed::default();
        p.feed(b"half a line", None);
        p.note("nib: not started");
        assert_eq!(p.lines(), ["half a line", "nib: not started"]);
    }
    /// Windows' ConPTY repaints the screen when the window resizes the terminal: the log says
    /// each line once, whole.
    #[cfg(windows)]
    #[test]
    fn a_resized_terminal_does_not_double_the_log() {
        let (s, _rx, d) = servers("resize");
        let line = r"Serving notebooks from local directory: C:\Users\me\src\explorer_v2\reports\notebooks\and\on";
        let cmd = format!("echo {line} & echo second line & ping -n 8 127.0.0.1 >NUL");
        s.lock().unwrap().request(AppOp::Set { name: "p".into(), def: app(&[("p", def(&cmd, None, &[]))], None) }, true).unwrap();
        s.lock().unwrap().request(AppOp::Up { names: vec![] }, true).unwrap();
        until("its terminal", || s.lock().unwrap().runs.get("p/p").is_some_and(|r| r.pty.is_some()));
        let ptys = s.lock().unwrap().ptys.clone();
        let id = s.lock().unwrap().runs["p/p"].pty.unwrap();
        // ConPTY asks where the cursor is before it shows anything; the window's terminal answers.
        thread::sleep(Duration::from_millis(300));
        ptys.lock().unwrap().input(id, b"\x1b[1;1R");
        until("its output", || s.lock().unwrap().log("p/p", 10).is_some_and(|l| l.contains("second line")));
        for (cols, rows) in [(50, 20), (120, 30), (70, 24)] {
            ptys.lock().unwrap().resize(id, cols, rows);
            thread::sleep(Duration::from_millis(400));
        }
        let log = s.lock().unwrap().log("p/p", 50).unwrap();
        assert_eq!(log.matches("Serving notebooks").count(), 1, "{log}");
        assert_eq!(log.matches("second line").count(), 1, "{log}");
        assert!(log.lines().any(|l| l.trim() == line), "{log}");
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn up_says_when_it_stopped_waiting_and_why() {
        assert_eq!(after_wait(&(String::new(), 0), 60), None);
        let (note, code) = after_wait(&("demo/web".into(), 3), 60).unwrap();
        assert!(note.contains("not ready after 60 s: demo/web") && code == 3, "{note}");
        // Asked not to wait: nothing to say about it.
        assert_eq!(after_wait(&("demo/web".into(), 3), 0), None);
        let (note, code) = after_wait(&("demo/api (port-taken)".into(), 0), 60).unwrap();
        assert!(note.contains("demo/api (port-taken)") && code == 1, "{note}");
    }

    /// Apps in a throwaway project, and what the window would be told.
    fn servers(tag: &str) -> (Shared, mpsc::Receiver<ServerMsg>, PathBuf) {
        let d = temp(tag);
        for sub in ["root", "nibdir", "mailbox"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        let (tx, rx) = mpsc::channel();
        (new(&d.join("root"), Some(&d.join("nibdir")), None, &d.join("mailbox"), Default::default(), tx), rx, d)
    }

    fn info(s: &Shared, name: &str) -> AppInfo {
        s.lock().unwrap().request(AppOp::List, true).unwrap().into_iter().find(|i| i.name == name).unwrap()
    }

    fn state_of(s: &Shared, app: &str, server: &str) -> ServerState {
        info(s, app).servers.into_iter().find(|i| i.name == server).map(|i| i.state).unwrap()
    }

    fn until(what: &str, f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn only_the_user_approves_and_the_user_starts_only_what_they_approved() {
        let (s, _rx, d) = servers("approve");
        let agent = Who::Agent("claude-7".into());
        let other = Who::Agent("codex-8".into());
        let act = |op: AppOp, who: &Who| s.lock().unwrap().act(op, who);
        let job = app(&[("job", def(sleeper(), None, &[]))], None);
        // An agent's app waits; the user is told so, and so is everyone else but that agent.
        act(AppOp::Set { name: "demo".into(), def: job.clone() }, &agent).unwrap();
        let a = info(&s, "demo");
        assert!(!a.approved);
        assert_eq!(a.origin.as_ref().unwrap().by, "agent:claude-7");
        assert!(act(AppOp::Up { names: vec!["demo".into()] }, &Who::User).unwrap_err().contains("not approved yet"));
        assert!(act(AppOp::Up { names: vec!["demo".into()] }, &Who::Ctl).unwrap_err().contains("waits for the user's approval"));
        assert!(act(AppOp::Up { names: vec!["demo".into()] }, &other).unwrap_err().contains("waits for the user's approval"));
        assert!(act(AppOp::Restart { name: "demo".into(), server: None }, &Who::User).is_err());
        assert!(act(AppOp::Up { names: vec![] }, &Who::User).unwrap_err().contains("no app is approved"));
        // Only the user approves, and only what they were shown.
        assert!(act(AppOp::Approve { name: "demo".into(), hash: a.hash.clone() }, &agent).unwrap_err().contains("only the user"));
        assert!(act(AppOp::Approve { name: "demo".into(), hash: a.hash.clone() }, &Who::Ctl).unwrap_err().contains("only the user"));
        assert!(act(AppOp::Approve { name: "demo".into(), hash: "0".repeat(64) }, &Who::User).unwrap_err().contains("changed since"));
        act(AppOp::Approve { name: "demo".into(), hash: a.hash.clone() }, &Who::User).unwrap();
        assert!(info(&s, "demo").approved);
        // A change by anybody but the user takes the approval away.
        let mut changed = job.clone();
        changed.servers.get_mut("job").unwrap().cmd = format!("echo hi & {}", sleeper());
        act(AppOp::Set { name: "demo".into(), def: changed.clone() }, &other).unwrap();
        assert!(!info(&s, "demo").approved);
        assert!(act(AppOp::Up { names: vec!["demo".into()] }, &agent).unwrap_err().contains("defined by agent:codex-8"), "now the other agent's");
        // The user's own definition needs no second look.
        act(AppOp::Set { name: "mine".into(), def: job.clone() }, &Who::User).unwrap();
        assert!(info(&s, "mine").approved);
        // Every change is in the log.
        let log = fs::read_to_string(d.join("nibdir/apps.log.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 4, "{log}");
        assert!(log.contains("\"did\":\"approve\"") && log.contains("\"by\":\"agent:codex-8\""));
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_agent_starts_the_app_it_defined_and_up_all_skips_what_waits() {
        let (s, rx, d) = servers("own");
        let agent = Who::Agent("claude-7".into());
        let act = |op: AppOp, who: &Who| s.lock().unwrap().act(op, who);
        act(AppOp::Set { name: "mine".into(), def: app(&[("job", def(sleeper(), None, &[]))], None) }, &agent).unwrap();
        act(AppOp::Set { name: "yours".into(), def: app(&[("job", def(sleeper(), None, &[]))], None) }, &Who::User).unwrap();
        act(AppOp::Up { names: vec!["mine".into()] }, &agent).unwrap();
        until("mine to run", || state_of(&s, "mine", "job") == ServerState::Ready);
        act(AppOp::Down { names: vec![] }, &Who::User).unwrap();
        assert_eq!(state_of(&s, "mine", "job"), ServerState::Stopped);
        // The user's "start everything" starts what they approved, and says what it left.
        act(AppOp::Up { names: vec![] }, &Who::User).unwrap();
        until("yours to run", || state_of(&s, "yours", "job") == ServerState::Ready);
        assert_eq!(state_of(&s, "mine", "job"), ServerState::Stopped);
        assert!(rx.try_iter().any(|m| matches!(m, ServerMsg::Notify(ServerNotify::Log { msg, .. }) if msg.contains("waiting for approval: mine"))));
        act(AppOp::Remove { name: "yours".into() }, &Who::User).unwrap();
        assert!(s.lock().unwrap().runs.keys().all(|k| app_of(k) != "yours"), "removed: its servers are gone");
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_agent_asks_through_the_mailbox_with_its_token() {
        let (s, _rx, d) = servers("agent");
        let dir = d.join("mailbox/app");
        fs::create_dir_all(dir.join("req")).unwrap();
        fs::create_dir_all(dir.join("res")).unwrap();
        let token = crate::tokens::issue("claude-test-mailbox");
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let json = format!(r#"{{ "title": "Job", "servers": {{ "job": {{ "cmd": "echo agent says hi & {}" }} }} }}"#, sleeper());
        let (out, code) = ask(&dir, &token, &args(&["set", "job", &json]));
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("waits for the user's approval (defined by agent:claude-test-mailbox)"), "{out}");
        assert_eq!(ask(&dir, &token, &args(&["set", "job", "{ \"servers\": 1 }"])).1, 2);
        // Without the token, or with another, nothing is done.
        for t in ["", "0123456789abcdef0123456789abcdef"] {
            let (out, code) = ask(&dir, t, &args(&["up", "job"]));
            assert!(code == 1 && out.contains("refused"), "{out}");
        }
        assert_eq!(state_of(&s, "job", "job"), ServerState::Stopped);
        // With it, the agent starts what it defined.
        let (out, code) = ask(&dir, &token, &args(&["open", "job"]));
        assert!(code == 0 && out.starts_with("job  \"Job\""), "{out}");
        until("settled", || ask(&dir, &token, &args(&["settled", "job"])).1 == 0);
        assert!(ask(&dir, &token, &args(&["list"])).0.contains("ready"));
        // ConPTY asks where the cursor is before it shows anything; the window's terminal answers.
        if cfg!(windows) {
            let (ptys, id) = {
                let g = s.lock().unwrap();
                (g.ptys.clone(), g.runs["job/job"].pty.unwrap())
            };
            thread::sleep(Duration::from_millis(300));
            ptys.lock().unwrap().input(id, b"\x1b[1;1R");
        }
        until("its output", || ask(&dir, &token, &args(&["log", "job", "--lines", "5"])).0.contains("agent says hi"));
        assert_eq!(ask(&dir, &token, &args(&["down"])).1, 0);
        assert!(ask(&dir, &token, &args(&["list"])).0.contains("stopped"));
        assert_eq!(ask(&dir, &token, &args(&["restart", "nobody"])).1, 1);
        assert_eq!(ask(&dir, &token, &args(&["dance"])).1, 2);
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_server_runs_in_a_terminal_until_it_is_stopped() {
        let (s, rx, d) = servers("run");
        let set = |name: &str, def: AppDef| s.lock().unwrap().request(AppOp::Set { name: name.into(), def }, true);
        assert!(s.lock().unwrap().request(AppOp::Up { names: vec![] }, true).unwrap_err().msg.contains("no apps"));
        assert!(set("Bad", app(&[("x", def("x", None, &[]))], None)).is_err());
        set("sleeper", app(&[("main", def("echo hello from the server; sleep 30", None, &[]))], None)).unwrap();
        set("short", app(&[("main", def("exit 3", None, &[]))], None)).unwrap();
        assert!(d.join("nibdir/apps.json").is_file());
        assert_eq!(state_of(&s, "sleeper", "main"), ServerState::Stopped);
        assert!(s.lock().unwrap().request(AppOp::Up { names: vec!["nobody".into()] }, true).unwrap_err().msg.contains("sleeper"));
        s.lock().unwrap().request(AppOp::Up { names: vec![] }, true).unwrap();
        // No port: ready as soon as it runs.
        until("sleeper to run", || state_of(&s, "sleeper", "main") == ServerState::Ready);
        until("short to end", || state_of(&s, "short", "main") == ServerState::Exited(Some(3)));
        until("its output", || s.lock().unwrap().log("sleeper/main", 5).is_some_and(|l| l.contains("hello from the server")));
        // The window heard of the terminal before its first output.
        let msgs: Vec<ServerMsg> = rx.try_iter().collect();
        let pty = s.lock().unwrap().runs["sleeper/main"].pty.unwrap();
        let told = msgs.iter().position(|m| matches!(m, ServerMsg::Notify(ServerNotify::App(i)) if i.name == "sleeper" && i.servers.iter().any(|s| s.pty == Some(pty)))).unwrap();
        let printed = msgs.iter().position(|m| matches!(m, ServerMsg::Notify(ServerNotify::PtyOutput { id, .. }) if *id == pty)).unwrap();
        assert!(told < printed);
        // Closing its terminal stops it; so does `down`; a removed one is forgotten.
        s.lock().unwrap().closed(pty);
        assert_eq!(state_of(&s, "sleeper", "main"), ServerState::Stopped);
        s.lock().unwrap().request(AppOp::Restart { name: "sleeper".into(), server: None }, true).unwrap();
        until("sleeper to run again", || state_of(&s, "sleeper", "main") == ServerState::Ready);
        s.lock().unwrap().request(AppOp::Down { names: vec![] }, true).unwrap();
        assert_eq!((state_of(&s, "sleeper", "main"), state_of(&s, "short", "main")), (ServerState::Stopped, ServerState::Stopped));
        let left = s.lock().unwrap().request(AppOp::Remove { name: "short".into() }, true).unwrap();
        assert_eq!(left.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["sleeper"]);
        assert!(rx.try_iter().any(|m| matches!(m, ServerMsg::Notify(ServerNotify::App(i)) if i.name == "short" && i.removed)));
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_port_decides_ready_taken_and_who_waits() {
        let (s, _rx, d) = servers("port");
        // Something else has this port.
        let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = other.local_addr().unwrap().port();
        // And this one is free until the test itself listens on it, as the server would.
        let free = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let set = |name: &str, def: AppDef| s.lock().unwrap().request(AppOp::Set { name: name.into(), def }, true).unwrap();
        set("taken", app(&[("main", def("sleep 30", Some(taken), &[]))], None));
        set("pair", app(&[("slow", def("sleep 30", Some(free), &[])), ("next", def("sleep 30", None, &["slow"]))], None));
        s.lock().unwrap().request(AppOp::Up { names: vec!["pair".into(), "taken".into()] }, true).unwrap();
        until("port-taken", || state_of(&s, "taken", "main") == ServerState::PortTaken);
        assert!(s.lock().unwrap().runs["taken/main"].pty.is_none(), "not started");
        until("slow to start", || state_of(&s, "pair", "slow") == ServerState::Starting);
        thread::sleep(TICK * 3);
        assert_eq!((state_of(&s, "pair", "slow"), state_of(&s, "pair", "next")), (ServerState::Starting, ServerState::Waiting));
        let _listening = std::net::TcpListener::bind(("127.0.0.1", free)).unwrap();
        until("slow to be ready", || state_of(&s, "pair", "slow") == ServerState::Ready);
        until("next to run", || state_of(&s, "pair", "next") == ServerState::Ready);
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_server_whose_dependency_ended_does_not_wait_for_ever() {
        let (s, rx, d) = servers("lost");
        let free = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let set = |name: &str, def: AppDef| s.lock().unwrap().request(AppOp::Set { name: name.into(), def }, true).unwrap();
        // The backend ends at once, without ever listening.
        set("demo", app(&[("backend", def("exit 7", Some(free), &[])), ("frontend", def("sleep 30", None, &["backend"]))], None));
        s.lock().unwrap().request(AppOp::Up { names: vec!["demo".into()] }, true).unwrap();
        until("backend to end", || state_of(&s, "demo", "backend") == ServerState::Exited(Some(7)));
        until("frontend to give up", || state_of(&s, "demo", "frontend") == ServerState::Exited(None));
        assert!(s.lock().unwrap().runs["demo/frontend"].pty.is_none(), "never started");
        assert!(s.lock().unwrap().log("demo/frontend", 5).unwrap().contains("backend, which it comes after, is exited(7)"));
        assert!(rx.try_iter().any(|m| matches!(m, ServerMsg::Notify(ServerNotify::Log { msg, .. }) if msg.contains("frontend") && msg.contains("backend"))));
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_server_on_ipv6_localhost_only_is_ready() {
        let Ok(l) = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)) else { return };
        let port = l.local_addr().unwrap().port();
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return;
        }
        assert!(listening(port), "[::1]:{port} answers");
    }
}
