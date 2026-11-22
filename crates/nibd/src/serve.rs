//! The project's servers: the backend, the frontend, TensorBoard, a notebook server. What a
//! person would otherwise keep running in a split terminal, started by nib, each in a terminal
//! of its own, and gone with the window.
//!
//! They are defined in `$NIBDIR/serve.json` on the daemon's machine:
//!
//! ```json
//! { "servers": {
//!     "backend":  { "cmd": "uv run app serve --port 8765", "port": 8765 },
//!     "frontend": { "cmd": "uv run app frontend --port 8766", "port": 8766,
//!                   "after": ["backend"], "view": "/" } } }
//! ```
//!
//! `cmd` is run by the user's shell in a PTY (`pty`), in `cwd` (inside the project), with `env`
//! added. A server is `ready` once its `port` accepts a connection on 127.0.0.1 or `[::1]` (one
//! with no port: as soon as it runs); one that comes `after` others is started when their ports
//! answer, and given up (`exited`, with the reason in its log) if one of them ended or was
//! stopped before its port ever did.
//! A port that already answers before the start means something else has it (another project's
//! server, an older run): the server is not started, and says `port-taken`.
//!
//! The window asks with `Request::Serve` and hears of every change as `ServerNotify::Server`.
//! Agents ask with `nib-serve …` (`nibd serve …`, see `cli`): a short-lived process, so it leaves
//! its request as a file in the session's mailbox folder (`serve/req/`), which the daemon reads
//! on its next turn and answers in `serve/res/`, as the mailbox does for letters.

use crate::pty::Ptys;
use nib_proto::{ErrorKind, LogLevel, PtyId, RpcError, ServeOp, ServerDef, ServerInfo, ServerMsg, ServerNotify, ServerState};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
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
/// How many lines of a server's output are kept for `nib-serve log`.
const LOG_LINES: usize = 5000;

pub type Defs = BTreeMap<String, ServerDef>;
pub type Shared = Arc<Mutex<Servers>>;

/// A server that was started (or asked to) in this session.
struct Run {
    /// As it was when started: the file may have changed since.
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
    /// `$NIBDIR/serve.json` (`None`: no NIBDIR on this machine).
    file: Option<PathBuf>,
    /// Where agents leave their requests (`<mailbox>/serve`; made by the first one that asks).
    dir: PathBuf,
    /// What every terminal gets: `NIB_SCRATCH`, `NIBDIR`.
    env: Vec<(&'static str, PathBuf)>,
    ptys: Arc<Mutex<Ptys>>,
    tx: Sender<ServerMsg>,
    runs: BTreeMap<String, Run>,
    me: Weak<Mutex<Servers>>,
}

/// The servers of the project at `root`, and the thread that looks after them for as long as
/// they are kept.
pub fn new(root: &Path, nibdir: Option<&Path>, scratch: Option<&Path>, mailbox: &Path, ptys: Arc<Mutex<Ptys>>, tx: Sender<ServerMsg>) -> Shared {
    let dir = mailbox.join("serve");
    let mut env = Vec::new();
    env.extend(scratch.map(|p| ("NIB_SCRATCH", p.to_path_buf())));
    env.extend(nibdir.map(|p| ("NIBDIR", p.to_path_buf())));
    let shared = Arc::new_cyclic(|me| {
        Mutex::new(Servers { root: root.to_path_buf(), file: nibdir.map(|d| d.join("serve.json")), dir, env, ptys, tx, runs: BTreeMap::new(), me: me.clone() })
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

/// A server's name: `[a-z0-9_-]{1,32}`.
fn name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Every definition makes sense, alone and with the others. The message says which does not.
pub fn validate(defs: &Defs) -> Result<(), String> {
    for (name, d) in defs {
        if !name_ok(name) {
            return Err(format!("server name {name:?}: use 1 to 32 of a-z, 0-9, _ and -"));
        }
        if d.cmd.trim().is_empty() {
            return Err(format!("{name}: cmd is empty"));
        }
        if d.port == Some(0) {
            return Err(format!("{name}: port must be 1 to 65535"));
        }
        if let Some(v) = &d.view {
            if !v.starts_with('/') {
                return Err(format!("{name}: view {v:?} must be a path starting with /"));
            }
            if d.port.is_none() {
                return Err(format!("{name}: view needs a port"));
            }
        }
        if let Some(c) = &d.cwd {
            let p = Path::new(c);
            if p.is_absolute() || c.starts_with(['/', '\\', '~']) || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return Err(format!("{name}: cwd {c:?} must be a folder inside the project (relative, no ..)"));
            }
        }
        if let Some(k) = d.env.keys().find(|k| k.is_empty() || k.contains('=')) {
            return Err(format!("{name}: env name {k:?} is not a variable name"));
        }
        for a in &d.after {
            match defs.get(a) {
                _ if a == name => return Err(format!("{name}: comes after itself")),
                None => return Err(format!("{name}: comes after {a}, which is not defined")),
                Some(o) if o.port.is_none() => return Err(format!("{name}: comes after {a}, which has no port to wait for")),
                Some(_) => {}
            }
        }
    }
    // No server waits, through the others, for itself.
    fn reaches(defs: &Defs, from: &str, to: &str, left: usize) -> bool {
        left > 0 && defs.get(from).is_some_and(|d| d.after.iter().any(|a| a == to || reaches(defs, a, to, left - 1)))
    }
    match defs.keys().find(|n| reaches(defs, n, n, defs.len())) {
        Some(n) => Err(format!("{n}: comes after itself (a cycle in after)")),
        None => Ok(()),
    }
}

/// What a definition may say.
const KEYS: &[&str] = &["cmd", "port", "after", "view", "cwd", "env", "title"];

/// The definitions in `file` (none if there is no file), checked.
pub fn read(file: &Path) -> Result<Defs, String> {
    let text = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Defs::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let wrong = |e: String| format!("{}: {e}", file.display());
    let v: Value = serde_json::from_str(&text).map_err(|e| wrong(e.to_string()))?;
    let servers = match v.get("servers") {
        Some(Value::Object(m)) => m,
        Some(_) => return Err(wrong("\"servers\" must be an object of name: definition".into())),
        None => return Ok(Defs::new()),
    };
    let mut defs = Defs::new();
    for (name, d) in servers {
        // A misspelt key would otherwise be dropped without a word.
        if let Some(k) = d.as_object().and_then(|o| o.keys().find(|k| !KEYS.contains(&k.as_str()))) {
            return Err(wrong(format!("{name}: unknown key {k:?} (known: {})", KEYS.join(", "))));
        }
        let def: ServerDef = serde_json::from_value(d.clone()).map_err(|e| wrong(format!("{name}: {e}")))?;
        defs.insert(name.clone(), def);
    }
    validate(&defs).map_err(wrong)?;
    Ok(defs)
}

/// Whole or not at all (written beside, then renamed).
pub fn write(file: &Path, defs: &Defs) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(&serde_json::json!({ "servers": defs }))?;
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, text + "\n")?;
    fs::rename(tmp, file)
}

impl Servers {
    fn file(&self) -> Result<&Path, String> {
        self.file.as_deref().ok_or_else(|| "no NIBDIR on this machine to keep serve.json in (see the daemon log)".to_string())
    }

    fn defs(&self) -> Result<Defs, String> {
        read(self.file()?)
    }

    fn info(&self, name: &str, def: Option<&ServerDef>) -> ServerInfo {
        let run = self.runs.get(name);
        ServerInfo {
            name: name.to_string(),
            def: def.or(run.map(|r| &r.def)).cloned().unwrap_or_default(),
            state: run.map_or(ServerState::Stopped, |r| r.state),
            pty: run.and_then(|r| r.pty),
            removed: false,
        }
    }

    /// Every server defined, and any still running whose definition is gone from the file.
    fn list(&self, defs: &Defs) -> Vec<ServerInfo> {
        let mut names: Vec<&String> = defs.keys().chain(self.runs.keys().filter(|n| !defs.contains_key(*n))).collect();
        names.sort();
        names.into_iter().map(|n| self.info(n, defs.get(n))).collect()
    }

    /// Tell the window how `name` is now.
    fn tell(&self, name: &str) {
        let def = self.defs().ok().and_then(|d| d.get(name).cloned());
        let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Server(self.info(name, def.as_ref()))));
    }

    fn set_state(&mut self, name: &str, state: ServerState) {
        if let Some(r) = self.runs.get_mut(name) {
            r.state = state;
        }
        self.tell(name);
    }

    pub fn request(&mut self, op: ServeOp) -> Result<Vec<ServerInfo>, RpcError> {
        self.act(op).map_err(bad)
    }

    fn act(&mut self, op: ServeOp) -> Result<Vec<ServerInfo>, String> {
        let mut defs = self.defs()?;
        let known = |defs: &Defs, name: &str| match defs.contains_key(name) {
            true => Ok(()),
            false if defs.is_empty() => Err(format!("no server named {name:?}: none is defined yet (serve set NAME --cmd …)")),
            false => Err(format!("no server named {name:?}; there are: {}", defs.keys().cloned().collect::<Vec<_>>().join(", "))),
        };
        match op {
            ServeOp::List => {}
            ServeOp::Set { name, def } => {
                defs.insert(name.clone(), def);
                validate(&defs)?;
                write(self.file()?, &defs).map_err(|e| format!("writing serve.json: {e}"))?;
                self.tell(&name);
            }
            ServeOp::Remove { name } => {
                known(&defs, &name)?;
                defs.remove(&name);
                validate(&defs)?;
                write(self.file()?, &defs).map_err(|e| format!("writing serve.json: {e}"))?;
                self.stop(&name);
                self.runs.remove(&name);
                let gone = ServerInfo { name, removed: true, ..Default::default() };
                let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Server(gone)));
            }
            ServeOp::Up { names } => {
                names.iter().try_for_each(|n| known(&defs, n))?;
                if defs.is_empty() {
                    return Err("no servers are defined yet (serve set NAME --cmd …)".into());
                }
                // With the servers each one comes after, and theirs.
                let mut want: Vec<String> = if names.is_empty() { defs.keys().cloned().collect() } else { names };
                let mut i = 0;
                while i < want.len() {
                    let after = defs.get(&want[i]).map(|d| d.after.clone()).unwrap_or_default();
                    want.extend(after.into_iter().filter(|a| !want.contains(a)).collect::<Vec<_>>());
                    i += 1;
                }
                for name in want {
                    self.up(&name, &defs, None);
                }
            }
            ServeOp::Down { names } => {
                let names: Vec<String> = if names.is_empty() { self.runs.keys().cloned().collect() } else { names };
                for name in names {
                    if !self.runs.contains_key(&name) {
                        known(&defs, &name)?;
                    }
                    self.stop(&name);
                }
            }
            ServeOp::Restart { name } => {
                known(&defs, &name)?;
                // Not said to be stopped on the way: the window keeps its terminal.
                if let Some(r) = self.runs.get_mut(&name) {
                    if let Some(id) = r.pty.take() {
                        self.ptys.lock().unwrap().close(id);
                    }
                    r.state = ServerState::Stopped;
                }
                self.up(&name, &defs, Some(Instant::now() + FREE_PORT));
            }
        }
        Ok(self.list(&defs))
    }

    /// Ask for `name` to run: it waits its turn (`advance` starts it). One already running or
    /// on its way is left alone.
    fn up(&mut self, name: &str, defs: &Defs, free_by: Option<Instant>) {
        let Some(def) = defs.get(name) else { return };
        if self.runs.get(name).is_some_and(|r| r.state.coming() || r.state == ServerState::Ready) {
            return;
        }
        let after = def.after.iter().filter_map(|a| defs.get(a).and_then(|d| d.port)).collect();
        let log = self.runs.get(name).map(|r| r.log.clone()).unwrap_or_default();
        self.runs.insert(name.to_string(), Run { def: def.clone(), after, state: ServerState::Waiting, pty: None, free_by, log });
        self.tell(name);
    }

    /// Stop `name` if it runs or is about to.
    fn stop(&mut self, name: &str) {
        let Some(r) = self.runs.get_mut(name) else { return };
        if let Some(id) = r.pty.take() {
            self.ptys.lock().unwrap().close(id);
        }
        if r.state != ServerState::Stopped {
            self.set_state(name, ServerState::Stopped);
        }
    }

    /// The window closed terminal `id`: if it was a server's, the server is stopped.
    pub fn closed(&mut self, id: PtyId) {
        if let Some(name) = self.runs.iter().find(|(_, r)| r.pty == Some(id)).map(|(n, _)| n.clone()) {
            if let Some(r) = self.runs.get_mut(&name) {
                r.pty = None;
            }
            self.set_state(&name, ServerState::Stopped);
        }
    }

    /// The process in terminal `id` ended by itself.
    fn exited(&mut self, id: PtyId, code: Option<i32>) {
        if let Some(name) = self.runs.iter().find(|(_, r)| r.pty == Some(id)).map(|(n, _)| n.clone()) {
            if let Some(r) = self.runs.get_mut(&name) {
                r.pty = None;
            }
            self.set_state(&name, ServerState::Exited(code));
        }
    }

    /// Move on every server that is on its way, given which ports answer now.
    fn advance(&mut self, open: &HashMap<u16, bool>) {
        let coming: Vec<String> = self.runs.iter().filter(|(_, r)| r.state.coming()).map(|(n, _)| n.clone()).collect();
        for name in coming {
            let Some(r) = self.runs.get_mut(&name) else { continue };
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
                ServerState::Starting if own == Some(true) => self.set_state(&name, ServerState::Ready),
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
                            self.give_up(&name, &format!("not started: {dep}, which it comes after, is {} and its port does not answer", state.name()));
                        }
                        continue;
                    }
                    match own {
                        Some(true) => self.set_state(&name, ServerState::PortTaken),
                        _ => self.start(&name),
                    }
                }
                _ => {}
            }
        }
    }

    /// `name` will not be started, and why: in its log, and said to the window.
    fn give_up(&mut self, name: &str, why: &str) {
        if let Some(r) = self.runs.get(name) {
            r.log.lock().unwrap().note(&format!("nib: {why}"));
        }
        let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg: format!("server {name}: {why}") }));
        self.set_state(name, ServerState::Exited(None));
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

    /// Run `name`'s command in a terminal of its own.
    fn start(&mut self, name: &str) {
        let Some(r) = self.runs.get(name) else { return };
        let (def, log) = (r.def.clone(), r.log.clone());
        let (ftx, frx) = mpsc::channel::<ServerMsg>();
        let opened = self.dir_of(def.cwd.as_deref()).and_then(|cwd| {
            let mut env: Vec<(&str, &Path)> = self.env.iter().map(|(k, v)| (*k, v.as_path())).collect();
            env.extend(def.env.iter().map(|(k, v)| (k.as_str(), Path::new(v))));
            self.ptys.lock().unwrap().open(&cwd, Some(shell(&def.cmd)), 80, 24, &env, ftx).map_err(|e| e.msg)
        });
        let id = match opened {
            Ok(id) => id,
            Err(e) => {
                let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg: format!("server {name}: {e}") }));
                return self.set_state(name, ServerState::Exited(None));
            }
        };
        if let Some(r) = self.runs.get_mut(name) {
            r.pty = Some(id);
        }
        // The window hears of the terminal before anything it prints.
        self.set_state(name, if def.port.is_some() { ServerState::Starting } else { ServerState::Ready });
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

    /// The last `lines` lines `name` printed, as plain text.
    fn log(&self, name: &str, lines: usize) -> Option<String> {
        let all = self.runs.get(name)?.log.lock().unwrap().lines();
        Some(all[all.len().saturating_sub(lines)..].join("\n"))
    }

    /// Answer the requests agents left (`cli`), oldest first.
    fn agents(&mut self) {
        let dir = self.dir.clone();
        let Ok(read) = fs::read_dir(dir.join("req")) else { return };
        let mut files: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        files.sort();
        for file in files {
            let args: Vec<String> = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let _ = fs::remove_file(&file);
            let (out, code) = self.run(&args);
            let Some(name) = file.file_name() else { continue };
            let tmp = dir.join("res").join(format!(".{}.tmp", name.to_string_lossy()));
            if fs::write(&tmp, serde_json::json!({ "out": out, "code": code }).to_string()).is_ok() {
                let _ = fs::rename(tmp, dir.join("res").join(name));
            }
        }
    }

    /// What `nib-serve ARGS` does, here in the daemon: what to print, and the exit code.
    fn run(&mut self, args: &[String]) -> (String, i32) {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let op = match words.as_slice() {
            ["list"] => ServeOp::List,
            ["set", name, opts @ ..] => match parse_def(opts) {
                Ok(def) => ServeOp::Set { name: name.to_string(), def },
                Err(e) => return (format!("nib-serve set: {e}"), 2),
            },
            ["rm", name] => ServeOp::Remove { name: name.to_string() },
            ["up", n @ ..] => ServeOp::Up { names: names(n) },
            ["down", n @ ..] => ServeOp::Down { names: names(n) },
            ["restart", name] => ServeOp::Restart { name: name.to_string() },
            ["log", name] | ["log", name, "--lines", _] => {
                let lines = words.get(3).and_then(|n| n.parse().ok()).unwrap_or(40);
                return match self.log(name, lines) {
                    Some(text) => (text, 0),
                    None => (format!("nib-serve: {name} has not run in this session"), 1),
                };
            }
            // `cli` asks until the servers it started are no longer on their way: 3 and those
            // still coming, else 0 and those that did not get to `ready`.
            ["settled", n @ ..] => {
                let mine = |name: &str| n.is_empty() || n.contains(&name);
                let coming: Vec<&str> = self.runs.iter().filter(|(name, r)| r.state.coming() && mine(name)).map(|(name, _)| name.as_str()).collect();
                if !coming.is_empty() {
                    return (coming.join(" "), 3);
                }
                let failed: Vec<String> = self
                    .runs
                    .iter()
                    .filter(|(name, r)| mine(name) && matches!(r.state, ServerState::Exited(_) | ServerState::PortTaken))
                    .map(|(name, r)| format!("{name} ({})", r.state.name()))
                    .collect();
                return (failed.join(", "), 0);
            }
            _ => return (USAGE.to_string(), 2),
        };
        match self.act(op) {
            Ok(list) => (table(&list), 0),
            Err(e) => (format!("nib-serve: {e}"), 1),
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

/// A definition from `--cmd "…" [--port N] [--after A,B] [--view /path] [--cwd DIR] [--title T]
/// [--env K=V]…`.
fn parse_def(opts: &[&str]) -> Result<ServerDef, String> {
    let mut def = ServerDef::default();
    let mut it = opts.iter();
    while let Some(o) = it.next() {
        let Some(v) = it.next() else { return Err(format!("{o} needs a value")) };
        match *o {
            "--cmd" => def.cmd = v.to_string(),
            "--port" => def.port = Some(v.parse().map_err(|_| format!("--port {v}: a number from 1 to 65535"))?),
            "--after" => def.after = v.split(',').filter(|a| !a.is_empty()).map(str::to_string).collect(),
            "--view" => def.view = Some(v.to_string()),
            "--cwd" => def.cwd = Some(v.to_string()),
            "--title" => def.title = Some(v.to_string()),
            "--env" => {
                let (k, val) = v.split_once('=').ok_or_else(|| format!("--env {v}: write NAME=VALUE"))?;
                def.env.insert(k.to_string(), val.to_string());
            }
            _ => return Err(format!("unknown option {o}")),
        }
    }
    if def.cmd.is_empty() {
        return Err("--cmd \"…\" is needed".into());
    }
    Ok(def)
}

/// One line a server: name, state, port, what it waits for, its command.
fn table(list: &[ServerInfo]) -> String {
    if list.is_empty() {
        return "no servers are defined (nib-serve set NAME --cmd \"…\" --port N)".to_string();
    }
    let w = list.iter().map(|s| s.name.len()).max().unwrap_or(0);
    list.iter()
        .map(|s| {
            let port = s.def.port.map(|p| format!(":{p}")).unwrap_or_default();
            let after = if s.def.after.is_empty() { String::new() } else { format!("after {}  ", s.def.after.join(",")) };
            format!("{:w$}  {:11} {port:6}  {after}{}", s.name, s.state.name(), s.def.cmd)
        })
        .collect::<Vec<_>>()
        .join("\n")
}


const USAGE: &str = "usage:
  nib-serve list                      the project's servers: name, state, port, command
  nib-serve set NAME --cmd \"…\" [--port N] [--after A,B] [--view /path] [--cwd DIR] [--title T] [--env K=V]…
                                      define (or redefine) a server; kept in $NIBDIR/serve.json
  nib-serve rm NAME                   stop it and forget its definition
  nib-serve up [NAME…]                start them (none named: all) and wait until each is ready or has failed
  nib-serve down [NAME…]              stop them (none named: all)
  nib-serve restart NAME              stop it, wait for its port to be free, start it, wait until it is ready
  nib-serve log NAME [--lines N]      the end of what it printed (default 40 lines)
up and restart wait at most 60 s (--timeout SECONDS; 0: don't wait).
states: stopped, waiting (for the servers it comes after), starting, ready, exited(CODE),
port-taken (something else already listens on its port: it was not started).";

/// Leave `args` for the daemon and wait for its answer: what to print, and the exit code.
fn ask(dir: &Path, args: &[String]) -> (String, i32) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("{nanos:032}-{}-{}.json", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let tmp = dir.join("req").join(format!(".{name}.tmp"));
    let sent = fs::write(&tmp, serde_json::to_vec(args).unwrap_or_default()).and_then(|_| fs::rename(&tmp, dir.join("req").join(&name)));
    if let Err(e) = sent {
        return (format!("nib-serve: {e}"), 1);
    }
    let res = dir.join("res").join(&name);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) {
        if let Some(v) = fs::read(&res).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            let _ = fs::remove_file(&res);
            return (v["out"].as_str().unwrap_or_default().to_string(), v["code"].as_i64().unwrap_or(1) as i32);
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = fs::remove_file(dir.join("req").join(&name));
    ("nib-serve: nib did not answer (is its window still open?)".to_string(), 1)
}

/// What to add to the list after `up` / `restart` waited, given the last `settled` answer:
/// servers still on their way when the time ran out (exit 3), or that ended or found their
/// port taken (exit 1). None: all are ready (or no wait was asked for).
fn after_wait(last: &(String, i32), timeout: u64) -> Option<(String, i32)> {
    match last {
        (coming, 3) if timeout > 0 => Some((format!("nib-serve: not ready after {timeout} s: {coming}; `nib-serve list` to see when, `nib-serve log NAME` for what it printed, --timeout SECONDS to wait longer"), 3)),
        (failed, 0) if !failed.is_empty() => Some((format!("nib-serve: not ready: {failed}; `nib-serve log NAME` says why"), 1)),
        _ => None,
    }
}

/// `--view` as Git Bash left it. Git Bash (MSYS) rewrites an argument that looks like an
/// absolute path — `/src/pages/x.html` — into a Windows one under its own folder
/// (`C:/Program Files/Git/src/pages/x.html`) before nib-serve sees it. Undone when the path
/// starts with that folder (`msys_root`, from `cygpath -m /`); any other drive path is refused
/// with how to avoid it.
fn unmangle_view(view: &str, msys_root: impl FnOnce() -> Option<String>) -> Result<String, String> {
    // `//path`, which Git Bash leaves alone, is `/path`.
    if let Some(rest) = view.strip_prefix("//") {
        return Ok(format!("/{}", rest.trim_start_matches('/')));
    }
    let drive = view.len() >= 3 && view.as_bytes()[0].is_ascii_alphabetic() && view[1..].starts_with([':']) && view[2..].starts_with(['/', '\\']);
    if !drive {
        return Ok(view.to_string());
    }
    let norm = view.replace('\\', "/");
    if let Some(root) = msys_root().map(|r| r.replace('\\', "/").trim_end_matches('/').to_string()).filter(|r| !r.is_empty()) {
        if let (Some(head), Some(rest)) = (norm.get(..root.len()), norm.get(root.len()..)) {
            if head.eq_ignore_ascii_case(&root) && rest.starts_with('/') {
                return Ok(rest.to_string());
            }
        }
    }
    Err(format!("--view {view:?} is a file path, not a page path: Git Bash rewrites arguments that start with /. Run it as MSYS_NO_PATHCONV=1 nib-serve set …, or write the path as //src/page.html"))
}

/// Git Bash's folder as Windows names it (what it puts in front of a path it rewrites), when
/// nib-serve runs under it.
fn msys_root() -> Option<String> {
    std::env::var_os("MSYSTEM")?;
    let out = std::process::Command::new("cygpath").args(["-m", "/"]).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// `nibd serve …`, run by an agent as `nib-serve`. Returns the exit code; prints to stdout / stderr.
pub fn cli(args: &[String]) -> i32 {
    let dir = std::env::var_os("NIB_MAILBOX").map(PathBuf::from).filter(|d| d.join("outbox").is_dir()).map(|d| d.join("serve"));
    let Some(dir) = dir.filter(|d| fs::create_dir_all(d.join("req")).is_ok() && fs::create_dir_all(d.join("res")).is_ok()) else {
        eprintln!("nib-serve: only works inside an agent that nib started (NIB_MAILBOX is not set)");
        return 2;
    };
    // `--timeout S` is this side's: how long `up` / `restart` wait for the servers to settle.
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
    if args.first().map(String::as_str) == Some("set") {
        if let Some(i) = args.iter().position(|a| a == "--view").map(|i| i + 1).filter(|i| *i < args.len()) {
            match unmangle_view(&args[i], msys_root) {
                Ok(v) => args[i] = v,
                Err(e) => {
                    eprintln!("nib-serve set: {e}");
                    return 2;
                }
            }
        }
    }
    let (mut out, mut code) = ask(&dir, &args);
    let started = match args.first().map(String::as_str) {
        Some("up") => Some(args[1..].to_vec()),
        Some("restart") => Some(args[1..].to_vec()),
        _ => None,
    };
    if let (0, Some(names)) = (code, started) {
        let t = Instant::now();
        let settled: Vec<String> = std::iter::once("settled".to_string()).chain(names).collect();
        let mut last = ask(&dir, &settled);
        while timeout > 0 && t.elapsed() < Duration::from_secs(timeout) && last.1 == 3 {
            thread::sleep(Duration::from_millis(300));
            last = ask(&dir, &settled);
        }
        (out, code) = ask(&dir, &["list".to_string()]);
        // Not done waiting is not done: say so, and with an exit code a script can see.
        if code == 0 {
            if let Some(note) = after_wait(&last, timeout) {
                out = format!("{out}\n{}", note.0);
                code = note.1;
            }
        }
    }
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(cmd: &str, port: Option<u16>, after: &[&str]) -> ServerDef {
        ServerDef { cmd: cmd.into(), port, after: after.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn defs(list: &[(&str, ServerDef)]) -> Defs {
        list.iter().map(|(n, d)| (n.to_string(), d.clone())).collect()
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nibd-serve-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        nib_proto::path::canonical(&d).unwrap()
    }

    #[test]
    fn definitions_are_checked() {
        let ok = defs(&[("backend", def("serve", Some(8765), &[])), ("frontend", ServerDef { view: Some("/".into()), ..def("front", Some(8766), &["backend"]) })]);
        assert_eq!(validate(&ok), Ok(()));
        let err = |d: &[(&str, ServerDef)]| validate(&defs(d)).unwrap_err();
        assert!(err(&[("Back end", def("x", None, &[]))]).contains("a-z"));
        assert!(err(&[(&"a".repeat(33), def("x", None, &[]))]).contains("1 to 32"));
        assert!(err(&[("a", def(" ", None, &[]))]).contains("cmd is empty"));
        assert!(err(&[("a", def("x", Some(0), &[]))]).contains("1 to 65535"));
        assert!(err(&[("a", def("x", None, &["b"]))]).contains("not defined"));
        assert!(err(&[("a", def("x", None, &["b"])), ("b", def("x", None, &[]))]).contains("no port"));
        assert!(err(&[("a", def("x", Some(1), &["a"]))]).contains("after itself"));
        assert!(err(&[("a", def("x", Some(1), &["b"])), ("b", def("x", Some(2), &["c"])), ("c", def("x", Some(3), &["a"]))]).contains("cycle"));
        assert!(err(&[("a", ServerDef { view: Some("index.html".into()), ..def("x", Some(1), &[]) })]).contains("starting with /"));
        assert!(err(&[("a", ServerDef { view: Some("/".into()), ..def("x", None, &[]) })]).contains("needs a port"));
        for cwd in ["../other", "/etc", "a/../../b", "~/x"] {
            assert!(err(&[("a", ServerDef { cwd: Some(cwd.into()), ..def("x", None, &[]) })]).contains("inside the project"), "{cwd}");
        }
        assert!(err(&[("a", ServerDef { env: [("A=B".to_string(), "1".to_string())].into(), ..def("x", None, &[]) })]).contains("variable name"));
    }

    #[test]
    fn the_file_is_read_strictly_and_written_back_the_same() {
        let d = temp("file");
        let file = d.join("serve.json");
        assert_eq!(read(&file), Ok(Defs::new()), "no file: no servers");
        let text = r#"{ "servers": {
            "backend":  { "cmd": "uv run clarity-desktop serve --no-open --port 8765", "port": 8765 },
            "frontend": { "cmd": "uv run clarity-desktop frontend --port 8766", "port": 8766, "after": ["backend"], "view": "/",
                          "env": { "API": "http://127.0.0.1:8765" }, "title": "Clarity", "cwd": "web" } } }"#;
        fs::write(&file, text).unwrap();
        let got = read(&file).unwrap();
        assert_eq!(got["backend"], def("uv run clarity-desktop serve --no-open --port 8765", Some(8765), &[]));
        assert_eq!((got["frontend"].view.as_deref(), got["frontend"].title.as_deref(), got["frontend"].cwd.as_deref()), (Some("/"), Some("Clarity"), Some("web")));
        assert_eq!(got["frontend"].env["API"], "http://127.0.0.1:8765");
        write(&file, &got).unwrap();
        assert_eq!(read(&file).unwrap(), got);
        assert!(!fs::read_to_string(&file).unwrap().contains("\"env\": {}"), "what is not set is not written");
        for (wrong, says) in [
            (r#"{ "servers": { "a": { "cmd": "x", "prot": 1 } } }"#, "unknown key \"prot\""),
            (r#"{ "servers": { "a": { "cmd": "x", "port": 70000 } } }"#, "a: "),
            (r#"{ "servers": { "a": { "port": 1 } } }"#, "cmd"),
            (r#"{ "servers": [] }"#, "must be an object"),
            ("{ not json", "serve.json"),
        ] {
            fs::write(&file, wrong).unwrap();
            let e = read(&file).unwrap_err();
            assert!(e.contains(says), "{wrong}: {e}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_definition_from_options() {
        let d = parse_def(&["--cmd", "npm run dev", "--port", "5173", "--after", "api,db", "--view", "/app", "--env", "A=1", "--env", "B=x=y", "--cwd", "web", "--title", "App"]).unwrap();
        assert_eq!((d.cmd.as_str(), d.port, d.after.join(" ").as_str(), d.view.as_deref()), ("npm run dev", Some(5173), "api db", Some("/app")));
        assert_eq!((d.env["A"].as_str(), d.env["B"].as_str(), d.cwd.as_deref(), d.title.as_deref()), ("1", "x=y", Some("web"), Some("App")));
        assert!(parse_def(&["--port", "1"]).unwrap_err().contains("--cmd"));
        assert!(parse_def(&["--cmd", "x", "--port", "99999"]).unwrap_err().contains("--port"));
        assert!(parse_def(&["--cmd", "x", "--env", "A"]).unwrap_err().contains("NAME=VALUE"));
        assert!(parse_def(&["--cmd"]).unwrap_err().contains("needs a value"));
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
        s.lock().unwrap().request(ServeOp::Set { name: "p".into(), def: def(&cmd, None, &[]) }).unwrap();
        s.lock().unwrap().request(ServeOp::Up { names: vec![] }).unwrap();
        until("its terminal", || s.lock().unwrap().runs.get("p").is_some_and(|r| r.pty.is_some()));
        let ptys = s.lock().unwrap().ptys.clone();
        let id = s.lock().unwrap().runs["p"].pty.unwrap();
        // ConPTY asks where the cursor is before it shows anything; the window's terminal answers.
        thread::sleep(Duration::from_millis(300));
        ptys.lock().unwrap().input(id, b"\x1b[1;1R");
        until("its output", || s.lock().unwrap().log("p", 10).is_some_and(|l| l.contains("second line")));
        for (cols, rows) in [(50, 20), (120, 30), (70, 24)] {
            ptys.lock().unwrap().resize(id, cols, rows);
            thread::sleep(Duration::from_millis(400));
        }
        let log = s.lock().unwrap().log("p", 50).unwrap();
        assert_eq!(log.matches("Serving notebooks").count(), 1, "{log}");
        assert_eq!(log.matches("second line").count(), 1, "{log}");
        assert!(log.lines().any(|l| l.trim() == line), "{log}");
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_view_git_bash_rewrote_is_given_back() {
        let git = || Some("C:/Users/me/AppData/Local/Programs/Git/".to_string());
        assert_eq!(unmangle_view("C:/Users/me/AppData/Local/Programs/Git/src/pages/x.html", git), Ok("/src/pages/x.html".into()));
        assert_eq!(unmangle_view("c:\\users\\me\\appdata\\local\\programs\\git\\lab", git), Ok("/lab".into()));
        assert_eq!(unmangle_view("/src/x.html", || panic!("not asked")), Ok("/src/x.html".into()));
        assert_eq!(unmangle_view("//src/x.html", || None), Ok("/src/x.html".into()));
        // A drive path that is not Git Bash's doing, or with no Git Bash to ask: refused, with the way round.
        assert!(unmangle_view("D:/work/x.html", git).unwrap_err().contains("MSYS_NO_PATHCONV=1"));
        assert!(unmangle_view("C:/Users/me/AppData/Local/Programs/Git/src", || None).is_err());
        assert!(unmangle_view("C:/Users/me/AppData/Local/Programs/Gitx/a", git).is_err());
    }

    #[test]
    fn up_says_when_it_stopped_waiting_and_why() {
        assert_eq!(after_wait(&(String::new(), 0), 60), None);
        let (note, code) = after_wait(&("frontend".into(), 3), 60).unwrap();
        assert!(note.contains("not ready after 60 s: frontend") && code == 3, "{note}");
        // Asked not to wait: nothing to say about it.
        assert_eq!(after_wait(&("frontend".into(), 3), 0), None);
        let (note, code) = after_wait(&("api (port-taken)".into(), 0), 60).unwrap();
        assert!(note.contains("api (port-taken)") && code == 1, "{note}");
    }

    /// Servers in a throwaway project, and what the window would be told.
    fn servers(tag: &str) -> (Shared, mpsc::Receiver<ServerMsg>, PathBuf) {
        let d = temp(tag);
        for sub in ["root", "nibdir", "mailbox"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        let (tx, rx) = mpsc::channel();
        (new(&d.join("root"), Some(&d.join("nibdir")), None, &d.join("mailbox"), Default::default(), tx), rx, d)
    }

    fn state_of(s: &Shared, name: &str) -> ServerState {
        s.lock().unwrap().request(ServeOp::List).unwrap().into_iter().find(|i| i.name == name).map(|i| i.state).unwrap()
    }

    fn until(what: &str, f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_server_runs_in_a_terminal_until_it_is_stopped() {
        let (s, rx, d) = servers("run");
        let set = |name: &str, def: ServerDef| s.lock().unwrap().request(ServeOp::Set { name: name.into(), def });
        assert!(s.lock().unwrap().request(ServeOp::Up { names: vec![] }).unwrap_err().msg.contains("no servers"));
        assert!(set("Bad", def("x", None, &[])).is_err());
        set("sleeper", def("echo hello from the server; sleep 30", None, &[])).unwrap();
        set("short", def("exit 3", None, &[])).unwrap();
        assert!(d.join("nibdir/serve.json").is_file());
        assert_eq!(state_of(&s, "sleeper"), ServerState::Stopped);
        assert!(s.lock().unwrap().request(ServeOp::Up { names: vec!["nobody".into()] }).unwrap_err().msg.contains("sleeper"));
        s.lock().unwrap().request(ServeOp::Up { names: vec![] }).unwrap();
        // No port: ready as soon as it runs.
        until("sleeper to run", || state_of(&s, "sleeper") == ServerState::Ready);
        until("short to end", || state_of(&s, "short") == ServerState::Exited(Some(3)));
        until("its output", || s.lock().unwrap().log("sleeper", 5).is_some_and(|l| l.contains("hello from the server")));
        // The window heard of the terminal before its first output.
        let msgs: Vec<ServerMsg> = rx.try_iter().collect();
        let pty = s.lock().unwrap().runs["sleeper"].pty.unwrap();
        let told = msgs.iter().position(|m| matches!(m, ServerMsg::Notify(ServerNotify::Server(i)) if i.name == "sleeper" && i.pty == Some(pty))).unwrap();
        let printed = msgs.iter().position(|m| matches!(m, ServerMsg::Notify(ServerNotify::PtyOutput { id, .. }) if *id == pty)).unwrap();
        assert!(told < printed);
        // Closing its terminal stops it; so does `down`; a removed one is forgotten.
        s.lock().unwrap().closed(pty);
        assert_eq!(state_of(&s, "sleeper"), ServerState::Stopped);
        s.lock().unwrap().request(ServeOp::Restart { name: "sleeper".into() }).unwrap();
        until("sleeper to run again", || state_of(&s, "sleeper") == ServerState::Ready);
        s.lock().unwrap().request(ServeOp::Down { names: vec![] }).unwrap();
        assert_eq!((state_of(&s, "sleeper"), state_of(&s, "short")), (ServerState::Stopped, ServerState::Stopped));
        let left = s.lock().unwrap().request(ServeOp::Remove { name: "short".into() }).unwrap();
        assert_eq!(left.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["sleeper"]);
        assert!(rx.try_iter().any(|m| matches!(m, ServerMsg::Notify(ServerNotify::Server(i)) if i.name == "short" && i.removed)));
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
        let set = |name: &str, def: ServerDef| s.lock().unwrap().request(ServeOp::Set { name: name.into(), def }).unwrap();
        set("taken", def("sleep 30", Some(taken), &[]));
        set("slow", def("sleep 30", Some(free), &[]));
        set("next", def("sleep 30", None, &["slow"]));
        // Asking for the last one brings up the one it comes after.
        s.lock().unwrap().request(ServeOp::Up { names: vec!["next".into(), "taken".into()] }).unwrap();
        until("port-taken", || state_of(&s, "taken") == ServerState::PortTaken);
        assert!(s.lock().unwrap().runs["taken"].pty.is_none(), "not started");
        until("slow to start", || state_of(&s, "slow") == ServerState::Starting);
        thread::sleep(TICK * 3);
        assert_eq!((state_of(&s, "slow"), state_of(&s, "next")), (ServerState::Starting, ServerState::Waiting));
        let _listening = std::net::TcpListener::bind(("127.0.0.1", free)).unwrap();
        until("slow to be ready", || state_of(&s, "slow") == ServerState::Ready);
        until("next to run", || state_of(&s, "next") == ServerState::Ready);
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
        let set = |name: &str, def: ServerDef| s.lock().unwrap().request(ServeOp::Set { name: name.into(), def }).unwrap();
        // The backend ends at once, without ever listening.
        set("backend", def("exit 7", Some(free), &[]));
        set("frontend", def("sleep 30", None, &["backend"]));
        s.lock().unwrap().request(ServeOp::Up { names: vec!["frontend".into()] }).unwrap();
        until("backend to end", || state_of(&s, "backend") == ServerState::Exited(Some(7)));
        until("frontend to give up", || state_of(&s, "frontend") == ServerState::Exited(None));
        assert!(s.lock().unwrap().runs["frontend"].pty.is_none(), "never started");
        assert!(s.lock().unwrap().log("frontend", 5).unwrap().contains("backend, which it comes after, is exited(7)"));
        assert!(rx.try_iter().any(|m| matches!(m, ServerMsg::Notify(ServerNotify::Log { msg, .. }) if msg.contains("frontend") && msg.contains("backend"))));
        // Stopped by the user while the other waits: the same.
        set("slow", def("sleep 30", Some(free), &[]));
        set("next", def("sleep 30", None, &["slow"]));
        s.lock().unwrap().request(ServeOp::Up { names: vec!["next".into()] }).unwrap();
        until("slow to start", || state_of(&s, "slow") == ServerState::Starting);
        assert_eq!(state_of(&s, "next"), ServerState::Waiting);
        s.lock().unwrap().request(ServeOp::Down { names: vec!["slow".into()] }).unwrap();
        until("next to give up", || state_of(&s, "next") == ServerState::Exited(None));
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

    #[cfg(unix)]
    #[test]
    fn an_agent_asks_through_the_mailbox_folder() {
        let (s, _rx, d) = servers("agent");
        let dir = d.join("mailbox/serve");
        fs::create_dir_all(dir.join("req")).unwrap();
        fs::create_dir_all(dir.join("res")).unwrap();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(ask(&dir, &args(&["set", "job", "--cmd", "echo agent says hi; sleep 30"])).1, 0);
        assert_eq!(ask(&dir, &args(&["set", "job"])), ("nib-serve set: --cmd \"…\" is needed".to_string(), 2));
        let (out, code) = ask(&dir, &args(&["up", "job"]));
        assert!(code == 0 && out.starts_with("job  "), "{out}");
        until("settled", || ask(&dir, &args(&["settled", "job"])).1 == 0);
        assert!(ask(&dir, &args(&["list"])).0.contains("ready"));
        until("its output", || ask(&dir, &args(&["log", "job", "--lines", "3"])).0.contains("agent says hi"));
        assert_eq!(ask(&dir, &args(&["down"])).1, 0);
        assert!(ask(&dir, &args(&["list"])).0.contains("stopped"));
        assert_eq!(ask(&dir, &args(&["restart", "nobody"])).1, 1);
        assert_eq!(ask(&dir, &args(&["dance"])).1, 2);
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }
}
