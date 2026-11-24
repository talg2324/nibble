//! Control socket: scripts and agents drive a running `nib` without screen access.
//!
//! Each window gets a private run directory, `<runtime dir>/nib-<uid>/<pid>/`:
//!
//!   ctl.sock     the socket (`$NIB_CONTROL` picks another path; `off` disables all of this)
//!   meta.json    pid, label, root, version, socket, log paths, start / end time
//!   nib.log      flash messages, disconnects, and every control request received
//!   snapshots/   where `snapshot` saves when no path is given
//!
//! The directory outlives the window (post-mortem after a crash); directories whose socket is
//! dead and whose log is older than a day are pruned. The protocol is JSON lines: one request
//! object per line (`{"cmd": "state"}`), one reply per line (`{"ok": true, ...}` or
//! `{"ok": false, "error": "..."}`). `nib ctl` is the client; `App::on_control` answers.
//! Everything is private to the user (0700 directories, 0600 socket). On Windows the socket is
//! an AF_UNIX one too (Windows 10 1803+), in the per-user temp directory, whose ACL keeps it
//! private.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

/// How long a connection waits for the app to answer one request. A person typing a password,
/// and ssh connecting (and uploading nibd), take longer than the rest.
fn reply_timeout(req: &Value) -> std::time::Duration {
    // Starting the browser; waiting for a page as long as was asked.
    if let Some(secs) = nib_proto::web::reply_secs(req) {
        return std::time::Duration::from_secs_f64(secs);
    }
    let secs = match req.get("cmd").and_then(Value::as_str).unwrap_or("") {
        "askpass" => 600,
        // These wait for ssh: a handshake, a probe, a new daemon's Hello.
        "ssh.connect" | "ssh.test" | "ssh.root" => 180,
        "ssh.add" if req.get("test").and_then(Value::as_bool) == Some(true) => 180,
        "forward.add" => 90,
        _ => 30,
    };
    std::time::Duration::from_secs(secs)
}

/// One request from a control client, carried to the app as a message. The app answers once
/// with `reply`, possibly later (a screenshot arrives after a frame).
#[derive(Clone)]
pub struct Call {
    pub req: Value,
    /// Where the answer goes (a socket's connection, another request's answer); taken once.
    answer: Arc<Mutex<Option<Answer>>>,
}

type Answer = Box<dyn FnOnce(Value) + Send>;

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Call({})", self.req)
    }
}

impl Call {
    /// A request as if it came from a client, and where its answer goes.
    #[cfg(test)]
    pub fn test(req: Value) -> (Self, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel();
        (Self::then(req, move |v| drop(tx.send(v))), rx)
    }
    /// A request the window makes of itself (a server's page, once the server is ready):
    /// nobody waits for the answer.
    pub fn internal(req: Value) -> Self {
        Self { req, answer: Arc::new(Mutex::new(None)) }
    }
    /// A request whose answer (`{"ok": …}`) is given to `answer`, on whichever thread answers:
    /// one that came another way than the socket (an agent's, through the daemon), or one made
    /// on the way to answering another.
    pub fn then(req: Value, answer: impl FnOnce(Value) + Send + 'static) -> Self {
        Self { req, answer: Arc::new(Mutex::new(Some(Box::new(answer)))) }
    }
    pub fn cmd(&self) -> &str {
        self.req.get("cmd").and_then(Value::as_str).unwrap_or("")
    }
    pub fn str_arg(&self, k: &str) -> Option<&str> {
        self.req.get(k).and_then(Value::as_str)
    }
    /// Answer with `{"ok": true}` merged with `v` (an object).
    pub fn ok(&self, v: Value) {
        let mut out = json!({ "ok": true });
        if let (Some(o), Value::Object(m)) = (out.as_object_mut(), v) {
            o.extend(m);
        }
        self.send(out);
    }
    pub fn err(&self, e: impl std::fmt::Display) {
        self.send(json!({ "ok": false, "error": e.to_string() }));
    }
    /// Answer with `v` as it is (another request's answer, passed on).
    pub fn reply(&self, v: Value) {
        self.send(v);
    }
    fn send(&self, v: Value) {
        let answer = self.answer.lock().unwrap().take();
        if let Some(answer) = answer {
            answer(v);
        }
    }
}

/// This window's run directory and log, set once by `serve`.
struct Run {
    dir: PathBuf,
    /// `meta.json` as written at start (what `state` reports as `session`).
    meta: Value,
    log: Mutex<std::fs::File>,
    shots: std::sync::atomic::AtomicU32,
}

static RUN: std::sync::OnceLock<Run> = std::sync::OnceLock::new();

/// Where this window keeps its socket, logs and snapshots (None: control is off).
pub fn session() -> Option<&'static Value> {
    RUN.get().map(|r| &r.meta)
}

/// Append a line to `nib.log` (nothing when control is off).
pub fn log(kind: &str, text: &str) {
    let Some(r) = RUN.get() else { return };
    let mut text: String = text.chars().take(400).collect();
    text = text.replace('\n', "\\n");
    use std::io::Write;
    let _ = writeln!(r.log.lock().unwrap(), "{} {kind:<5} {text}", nib_proto::log_clock());
}

/// A request as the log keeps it. What is typed into a page (`web.fill`, `web.type`: often a
/// password or a token) is its length only, and an address loses what follows `?` or `#`, and
/// a name and password before `@`: enough to follow what was done, nothing to sign in with.
pub fn loggable(req: &Value) -> String {
    let mut v = req.clone();
    let web = v.get("cmd").and_then(Value::as_str).is_some_and(|c| c.starts_with("web."));
    if let Some(o) = v.as_object_mut().filter(|_| web) {
        if let Some(n) = o.get("text").and_then(Value::as_str).map(|t| t.chars().count()) {
            o.insert("text".into(), Value::String(format!("<{n} chars>")));
        }
        if let Some(url) = o.get("url").and_then(Value::as_str) {
            let mut shown = url[..url.find(['?', '#']).unwrap_or(url.len())].to_string();
            if shown.len() < url.len() {
                shown.push('…');
            }
            let start = shown.find("://").map_or(0, |i| i + 3);
            let host_end = shown[start..].find('/').map_or(shown.len(), |i| start + i);
            if let Some(at) = shown[start..host_end].rfind('@') {
                shown.replace_range(start..start + at, "…");
            }
            o.insert("url".into(), Value::String(shown));
        }
    }
    v.to_string()
}

/// Next default snapshot file: `<run dir>/snapshots/001.png`, `002.png`, …
pub fn next_snapshot_path() -> Option<PathBuf> {
    let r = RUN.get()?;
    let n = r.shots.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let d = r.dir.join("snapshots");
    std::fs::create_dir_all(&d).ok()?;
    Some(d.join(format!("{n:03}.png")))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// On normal exit: removes the socket, stamps `ended` into `meta.json`, keeps the rest.
pub struct Guard {
    sock: PathBuf,
}

impl Guard {
    pub fn socket(&self) -> &Path {
        &self.sock
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        crate::web::shutdown();
        crate::forward::clear_now();
        let _ = std::fs::remove_file(&self.sock);
        if let Some(r) = RUN.get() {
            log("exit", "");
            let mut meta = r.meta.clone();
            meta["ended"] = json!(unix_now());
            let _ = std::fs::write(r.dir.join("meta.json"), serde_json::to_vec_pretty(&meta).unwrap_or_default());
        }
    }
}

mod sys {
    use super::*;
    use anyhow::{bail, Context, Result};
    use std::io::{BufRead, BufReader, Write};
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    #[cfg(unix)]
    use std::os::unix::net::{UnixListener, UnixStream};
    #[cfg(windows)]
    use uds_windows::{UnixListener, UnixStream};

    /// Run directories older than this, with a dead socket, are deleted.
    const KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

    /// Directory holding every window's run directory: `%TEMP%\nib`, which is per-user
    /// (`%LOCALAPPDATA%\Temp`, readable by this user only).
    #[cfg(windows)]
    pub fn base() -> Result<PathBuf> {
        let d = std::env::temp_dir().join("nib");
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        Ok(d)
    }

    /// A new private directory (0700 on Unix; Windows: inherits the per-user temp ACL).
    fn private_dir(d: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        return std::fs::DirBuilder::new().mode(0o700).create(d);
        #[cfg(not(unix))]
        std::fs::create_dir(d)
    }

    /// Directory holding every window's run directory, created private to this user.
    #[cfg(unix)]
    pub fn base() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let d = base.join(format!("nib-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", d.display())),
        }
        let m = std::fs::metadata(&d)?;
        if m.uid() != uid || m.permissions().mode() & 0o077 != 0 {
            bail!("{} is not private to this user; refusing to use it", d.display());
        }
        Ok(d)
    }

    /// The socket a run directory's window listens on.
    pub fn socket_of(run_dir: &Path) -> PathBuf {
        std::fs::read(run_dir.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|m| m.get("socket").and_then(Value::as_str).map(PathBuf::from))
            .unwrap_or_else(|| run_dir.join("ctl.sock"))
    }

    /// Set up this window's run directory, bind its socket and serve it on background threads.
    /// Each request becomes `wrap(call)` sent on `out`. `meta` is merged into `meta.json`.
    pub fn serve<M: Send + 'static>(
        out: iced::futures::channel::mpsc::UnboundedSender<M>,
        wrap: fn(Call) -> M,
        meta: Value,
    ) -> Result<Option<Guard>> {
        let custom = match std::env::var("NIB_CONTROL") {
            Ok(v) if matches!(v.as_str(), "off" | "0" | "") => return Ok(None),
            Ok(v) => Some(PathBuf::from(v)),
            Err(_) => None,
        };
        let base = base()?;
        prune(&base);
        let dir = base.join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir); // a previous process with the same pid
        private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let sock = custom.unwrap_or_else(|| dir.join("ctl.sock"));
        // sun_path is 104 bytes on macOS, 108 on Linux and Windows, including the NUL.
        if sock.as_os_str().len() > 103 {
            bail!("socket path {} is too long for a unix socket (max 103 bytes); pick a shorter NIB_CONTROL", sock.display());
        }
        if UnixStream::connect(&sock).is_ok() {
            bail!("{} is in use by another nib", sock.display());
        }
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

        let log_path = dir.join("nib.log");
        let mut m = json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "started": unix_now(),
            "run_dir": dir,
            "socket": sock,
            "snapshots": dir.join("snapshots"),
        });
        if let (Some(o), Value::Object(extra)) = (m.as_object_mut(), meta) {
            o.extend(extra);
        }
        m["logs"]["nib"] = json!(log_path);
        std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&m)?)?;
        let log_file = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let _ = RUN.set(Run { dir, meta: m.clone(), log: Mutex::new(log_file), shots: Default::default() });
        log("start", &m.to_string());

        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let out = out.clone();
                std::thread::spawn(move || {
                    let _ = handle_conn(conn, out, wrap);
                });
            }
        });
        Ok(Some(Guard { sock }))
    }

    /// Delete run directories whose window is gone and whose log hasn't moved for `KEEP`.
    fn prune(base: &Path) {
        let Ok(entries) = std::fs::read_dir(base) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if UnixStream::connect(socket_of(&p)).is_ok() {
                    continue;
                }
                let last = std::fs::metadata(p.join("nib.log")).or_else(|_| e.metadata()).and_then(|m| m.modified());
                if last.is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEEP)) {
                    let _ = std::fs::remove_dir_all(&p);
                }
            } else if p.extension().is_some_and(|x| x == "sock") && UnixStream::connect(&p).is_err() {
                let _ = std::fs::remove_file(&p); // left by an older nib
            }
        }
    }

    fn handle_conn<M>(
        conn: UnixStream,
        out: iced::futures::channel::mpsc::UnboundedSender<M>,
        wrap: fn(Call) -> M,
    ) -> std::io::Result<()> {
        let mut w = conn.try_clone()?;
        for line in BufReader::new(conn).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // `wait` polls `pending` many times a second; keep it out of the trace.
            // An askpass request carries the connection's secret: never write it down.
            if line.contains(r#""cmd":"askpass""#) {
                log("ctl", r#"{"cmd":"askpass", …}"#);
            } else if !line.contains(r#""cmd":"pending""#) {
                match serde_json::from_str::<Value>(&line) {
                    Ok(req) => log("ctl", &loggable(&req)),
                    Err(_) => log("ctl", line.trim()),
                }
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(req) if req.is_object() => {
                    let (tx, rx) = mpsc::channel();
                    let timeout = reply_timeout(&req);
                    let call = Call::then(req, move |v| drop(tx.send(v)));
                    if out.unbounded_send(wrap(call)).is_err() {
                        return Ok(()); // the window is gone
                    }
                    rx.recv_timeout(timeout)
                        .unwrap_or_else(|_| json!({ "ok": false, "error": "no answer from the window" }))
                }
                Ok(_) => json!({ "ok": false, "error": "a request is a JSON object" }),
                Err(e) => json!({ "ok": false, "error": format!("bad JSON: {e}") }),
            };
            if let Some(e) = reply.get("error").and_then(Value::as_str) {
                log("ctl!", e);
            }
            writeln!(w, "{reply}")?;
        }
        Ok(())
    }

    /// Send one request and read its reply.
    pub fn call(sock: &Path, req: &Value) -> Result<Value> {
        let mut s = UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
        s.set_read_timeout(Some(reply_timeout(req) + std::time::Duration::from_secs(5)))?;
        writeln!(s, "{req}")?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line)?;
        serde_json::from_str(&line).with_context(|| format!("bad reply {line:?}"))
    }

    /// Every live window's socket, newest first. Also prunes old dead run directories.
    pub fn live() -> Result<Vec<PathBuf>> {
        let base = base()?;
        prune(&base);
        let mut socks: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for e in std::fs::read_dir(&base)?.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let sock = socket_of(&e.path());
            if UnixStream::connect(&sock).is_err() {
                continue;
            }
            let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            socks.push((t, sock));
        }
        socks.sort_by_key(|s| std::cmp::Reverse(s.0));
        Ok(socks.into_iter().map(|(_, p)| p).collect())
    }
}

pub use sys::serve;
pub use sys::base as run_base;

/// Send one request to a window's socket and read its reply.
pub fn request(sock: &Path, req: &Value) -> anyhow::Result<Value> {
    sys::call(sock, req)
}

/// This window's socket, if it has one (the askpass helper reports back to it).
pub fn socket() -> Option<PathBuf> {
    session()?.get("socket")?.as_str().map(PathBuf::from)
}

pub const CTL_USAGE: &str = "\
usage: nib ctl [-p PID | -s SOCKET] COMMAND [ARGS]
Talk to a running nib window. Without -p/-s: $NIB_CONTROL if set, else the only window
(with several open, pick one from `nib ctl list`). Replies are JSON on stdout.

  list                 open windows: pid, socket, title, root
  state                tabs, cursor, selections, visible lines, terminals, panels, messages,
                       and `session`: run dir, socket, log paths (nib.log, the daemon's log)
  snapshot [FILE]      save a PNG of the window (default: <run dir>/snapshots/NNN.png)
  commands             names usable with `run` (snake_case, e.g. move_doc_end)
  run COMMAND          run a named command, e.g. run save, run new_terminal
  key CHORD            press a key chord, e.g. key mod+s, key ctrl+end, key enter
  type TEXT            type TEXT into whatever has focus (\\n = Enter, \\t = Tab)
  open PATH            open a file (relative to the window's root, or absolute)
  search [--project] TEXT
                       find TEXT (Cmd+F): in the file or chat with focus, or with --project
                       across the project on the daemon (file names fuzzily, then lines; hidden
                       folders, gitignored and binary files skipped); answers with the matches
  search --next|--prev|--close
                       next / previous match in the file or chat, or close the box
  search --open [N]    open project result N (0-based: file names first, then lines)
  drop tree DIR PATH…  copy files / folders into the tree's folder DIR ('' or . is its root), as a drag-and-drop
  drop chat PATH…      drop files on the agents chat: pictures go with the message, text files as @ mentions
  ai use NAME [--model ID]
                       start a new AI chat with that assistant (Claude Code, Codex, Grok, OpenCode)
                       on one of its models (`state` lists them under ai.agents)
  ai pick [NAME]       open the AI panel's assistant / model dropdown (on NAME's models)
  ai warm|cold MODEL   load a local model into memory / unload it (OpenCode's id, e.g. ollama/qwen3:8b)
  ai update NAME       update that assistant to its latest version, in a terminal
  ai install NAME      install that assistant in a terminal, as its Install button does (OpenCode:
                       with Ollama)
  ai pull MODEL        download an Ollama model (qwen3:8b); state.ai.pulling shows how far
  ai rm MODEL          delete a downloaded Ollama model (no confirmation)
  diff open [SCOPE]    show the Review tab (state.diff): head, unstaged, staged, everyone, agent:N or an
                       agent's name (state.diff.scopes lists them), since:N (what agent N changed
                       since its work was last reviewed)
  diff comment [--on PATH[:LINE[-END]] | --on plan:LINE[-END]] TEXT
                       review the agent's work the Review tab shows: a comment on lines of a file as
                       it is now, on a file, on lines of the agent's plan (state.diff.plan.text;
                       a file called plan is ./plan), or (no --on) on the work as a whole
  diff reply ID TEXT   answer comment ID; diff resolve|reopen|remove ID
  diff send            send the open comments to the agent (the next round; state.diff.review)
  ai accept|keep       the shown agent said its job is done (state.ai.chats[].finished): accept it
                       and end the agent (on its own branch: land the branch first, as one
                       commit; `wait` until that is done), or put the question away and keep it
  ai diff              show the shown agent's work in the Review tab (state.diff), like its pull request
  ai effort LEVEL      the shown agent's thinking level: default or one of its assistant's
                       (state.ai.efforts); taken when its session starts
  ai system-prompt TEXT  replace what the shown agent adds to its system prompt (empty: none)
  ai system            show the system prompt pane beside the steps
  ai wpf [on|off]      worktree-per-feature: each agent's change on a branch of its own, landed
                       as one commit (state.ai.worktree_per_feature; no argument: flip it)
  wait [SECS]          until nothing is waiting on the daemon and no server is on its way to
                       ready (default 10 s)
  raw JSON             send one request as-is, e.g. raw '{\"cmd\":\"state\"}'

Local apps (a page served by this machine, shown in a web tab; read and operated from here, so no
browser automation is needed). Runs in the system's browser, unseen (Firefox, Chrome, Edge, Brave
or Chromium). Only this machine's pages: anything else is refused, and never opened for you.
Every request takes --tab T: a tab's number in `web list`, its port, or the start of its name.
Without it: the only tab; with several open, it is refused (the user switches tabs too).
  web open PORT|URL            show an app (http://127.0.0.1:PORT/ for a port); the same app
                               again is the same tab. Its tab comes forward (--background: not).
                               In a window on another machine (SSH) the port is that machine's:
                               it is forwarded here first (`forwarded` in the answer; below)
  web open PROFILE:PORT[/PATH] a port of a saved SSH connection, forwarded here and shown
                               (TensorBoard on the GPU machine: web open gpu:6006)
  web list                     the web tabs: name, address, loading
  web read [--full]            the page as text with what can be acted on marked [N name], those
                               elements (ref, role, name, value, min, max, checked, options,
                               disabled, offscreen: not to be seen where it is, outside the page
                               or a panel around it; frame), zoom when not 100%, and since the
                               last read: console errors and
                               warnings, failed requests, downloads; `blocked`: an address
                               outside this machine the page tried to show; `dialog`: one waiting
  web click N                  click element N of the last read
  web fill N TEXT              empty field N and type TEXT (a <select>: choose that option);
                               then waits 0.4 s, as pages take a typed value after a pause
                               (--no-wait: don't)
  web select N OPTION          choose an option of <select> N by its text or value
  web type TEXT | web key CHORD
                               type into, or press a chord in, whatever has the keyboard in
                               the page (key shift+enter, key mod+s, key escape, key right)
  web scroll N | +PX | -PX     bring element N into view, or scroll down / up; `visible: false`
                               when N is scrolled to but a box around it leaves it no room
  web zoom [PERCENT|in|out|reset]
                               the page laid out smaller or bigger, as a browser zooms (25-300;
                               the user's Cmd/Ctrl + wheel or +/- over the tab does the same):
                               zoom 75 gives a crowded layout room. Answers zoom and laid_out
  web wait [TEXT | N] [--gone] [--secs S]
                               until the page shows TEXT, or element N (--gone: no longer),
                               or with neither: has loaded. Default 10 s; an error if not
  web shot [FILE] [--full] [--ref N]
                               a PNG of the page as it shows, of the whole document, or of
                               one element (default: <run dir>/snapshots/NNN.png)
  web dialog accept|dismiss [TEXT]
                               answer an alert / confirm / prompt (TEXT: what to type in it)
  web upload N FILE…           give file input N its files
  web back | web reload
  web pointer X Y [move|click|down|up|wheel PX]
                               the mouse in the web tab in front, at X Y of the page (what a
                               person does there, the tab's own lists and dialogs included;
                               agents use click N instead). state.web.window says what shows
  web eval JS                  the value of an expression in the page
  web close                    close the tab (the last one: the browser too)

Apps (the project's apps: each runs servers — a backend, a frontend, TensorBoard… — and may
have a page. Defined in apps.json in the project's NIBDIR on the daemon's machine; each server
runs in a terminal of the window, stopped when it closes. The Apps panel shows them. What is
defined here waits for the user's approval: they see its commands the first time they open it):
  app list                     every app and its servers: state, port, page, terminal, cmd,
                               approved. States: stopped, waiting (for the servers it comes
                               after), starting, ready (its port answers), exited(CODE),
                               port-taken (something else already listens there: not started)
  app set NAME --json FILE|-   define or redefine an app, whole, from JSON:
                               {\"title\": \"Demo\", \"page\": {\"server\": \"web\", \"path\": \"/\"},
                                \"servers\": {\"api\": {\"cmd\": \"…\", \"port\": 8765},
                                            \"web\": {\"cmd\": \"…\", \"port\": 8766, \"after\": [\"api\"],
                                                    \"cwd\": \"web\", \"env\": {\"K\": \"V\"}}}}
  app rm NAME                  stop it and forget it
  app up [NAME…]               start them (none named: every approved one); answers at once:
                               `wait`, or poll `app list`, for ready
  app down [NAME…]             stop them (none named: all); their terminals close
  app restart NAME [SERVER]    stop its servers (or one), wait for the ports to be free, start
  app log NAME [SERVER] [--lines N]  what its terminal shows (the page's server by default)

Ports of another machine (the web tab shows this machine's pages only, so a remote port is
brought here over ssh: to the same port number when it is free, else the next free one, with a
warning). `web open` and a ready server do this by themselves; a link clicked in a terminal too.
  forward list                 state.forwards: local (the port here), remote, host
  forward add [PROFILE:]PORT   bring a port here and keep it: the window's machine's, or a saved
                               SSH connection's. Keys only: ssh asks nothing
  forward rm LOCALPORT         take one down (also: when its web tab closes or its server
                               stops, when the window changes machine or root, when it quits)

SSH (saved connections are [profiles] in config.toml; the window switches in place):
  ssh list                         saved connections, where the window is, ~/.ssh/config hosts
  ssh add NAME HOST [OPTIONS]      save one: --root DIR (default ~) --user U --port N
                                   --identity FILE --jump HOST --nibd PATH; --test also tests it
  ssh update NAME [OPTIONS]        change one: --host H, the options above, --rename NEW,
                                   --unset FIELD (user, port, identity, jump, nibd)
  ssh remove NAME
  ssh test NAME | ssh test --host H [OPTIONS]
                                   reach it, check the host key, log in with keys, check the
                                   platform and folder; doesn't connect the window
  ssh connect NAME [--root DIR] [--force] | ssh connect --home [--force]
                                   switch the window there (default: the root used last;
                                   --home: where the window started); waits
                                   until connected. Keys only: nothing is asked, unknown host
                                   keys are refused (trust them in the SSH panel). --force
                                   closes unsaved files and terminals of the current machine
  ssh root PATH                    make another folder on this machine the root (absolute,
                                   ~/…, or relative to the current root); tabs and terminals stay
  ssh cancel                       stop connecting";

/// `text` and a newline on stdout. A reader that stopped reading (`| head`, `| grep -q`, a pager
/// that quit) is not an error: what it wanted it has, so the answer's exit code stands. (Rust's
/// `println!` panics there instead.)
fn say(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if let Err(e) = writeln!(out, "{text}").and_then(|()| out.flush()) {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("nib ctl: writing the answer: {e}");
        }
    }
}

/// `nib ctl ...`: returns the process exit code.
pub fn ctl_main(args: &[String]) -> i32 {
    match ctl(args) {
        Ok(ok) => i32::from(!ok),
        Err(e) => {
            eprintln!("nib ctl: {e:#}");
            2
        }
    }
}

fn ctl(args: &[String]) -> anyhow::Result<bool> {
    use anyhow::{bail, Context};
    use std::path::PathBuf;

    let mut a: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut sock: Option<PathBuf> = None;
    loop {
        match a.as_slice() {
            ["-p", pid, ..] => {
                sock = Some(sys::socket_of(&sys::base()?.join(pid)));
                a.drain(..2);
            }
            ["-s", path, ..] => {
                sock = Some(PathBuf::from(path));
                a.drain(..2);
            }
            _ => break,
        }
    }
    let print = |v: &Value| -> bool {
        say(&serde_json::to_string_pretty(v).unwrap_or_default());
        v.get("ok").and_then(Value::as_bool).unwrap_or(false)
    };
    if a.as_slice() == ["list"] {
        let mut rows = Vec::new();
        for p in sys::live()? {
            let mut v = sys::call(&p, &json!({ "cmd": "hello" })).unwrap_or_else(|e| json!({ "error": e.to_string() }));
            if let Some(o) = v.as_object_mut() {
                o.remove("ok");
                o.insert("socket".into(), json!(p));
            }
            rows.push(v);
        }
        return Ok(print(&json!({ "ok": true, "windows": rows })));
    }
    let req = match a.as_slice() {
        [] | ["-h" | "--help" | "help"] => {
            say(CTL_USAGE);
            return Ok(true);
        }
        ["state"] => json!({ "cmd": "state" }),
        ["commands"] => json!({ "cmd": "commands" }),
        ["snapshot"] => json!({ "cmd": "snapshot" }),
        ["snapshot", file] => {
            json!({ "cmd": "snapshot", "path": std::path::absolute(file)? })
        }
        ["run", name] => json!({ "cmd": "run", "command": name }),
        ["key", chord] => json!({ "cmd": "key", "chord": chord }),
        ["type", text] => json!({ "cmd": "type", "text": nib_proto::web::unescape(text) }),
        ["open", path] => json!({ "cmd": "open", "path": path }),
        ["search", "--next"] => json!({ "cmd": "search", "do": "next" }),
        ["search", "--prev"] => json!({ "cmd": "search", "do": "prev" }),
        ["search", "--close"] => json!({ "cmd": "search", "do": "close" }),
        ["search", "--open"] => json!({ "cmd": "search", "do": "open" }),
        ["search", "--open", n] => json!({ "cmd": "search", "do": "open", "index": n.parse::<usize>().context("search --open: a result number")? }),
        ["search", "--project" | "--repo", rest @ ..] if !rest.is_empty() => json!({ "cmd": "search", "scope": "project", "query": rest.join(" ") }),
        ["search", rest @ ..] if !rest.is_empty() => json!({ "cmd": "search", "scope": "local", "query": rest.join(" ") }),
        ["ai", "warm", model] => json!({ "cmd": "ai.warm", "model": model, "warm": true }),
        ["ai", "cold", model] => json!({ "cmd": "ai.warm", "model": model, "warm": false }),
        ["ai", "update", rest @ ..] if !rest.is_empty() => json!({ "cmd": "ai.update", "agent": rest.join(" ") }),
        ["ai", "install", rest @ ..] if !rest.is_empty() => json!({ "cmd": "ai.install", "agent": rest.join(" ") }),
        ["ai", "pull", model] => json!({ "cmd": "ai.pull", "model": model }),
        ["ai", "rm", model] => json!({ "cmd": "ai.rm", "model": model }),
        ["ai", "pick", rest @ ..] => json!({ "cmd": "ai.pick", "agent": rest.join(" ") }),
        ["ai", "reopen", n] => json!({ "cmd": "ai.past", "do": "reopen", "index": n.parse::<usize>().context("ai reopen: which one (0 is the first listed)")? }),
        ["ai", "replay", n] => json!({ "cmd": "ai.past", "do": "replay", "index": n.parse::<usize>().context("ai replay: which one (0 is the first listed)")? }),
        ["ai", "forget", n] => json!({ "cmd": "ai.past", "do": "forget", "index": n.parse::<usize>().context("ai forget: which one (0 is the first listed)")? }),
        ["ai", "sub"] => json!({ "cmd": "ai.sub" }),
        ["ai", "sub", id] => json!({ "cmd": "ai.sub", "id": id }),
        ["ai", "follow"] => json!({ "cmd": "ai.follow", "on": true }),
        ["ai", "unfollow"] => json!({ "cmd": "ai.follow", "on": false }),
        ["open", path, "--line", line] => json!({ "cmd": "open", "path": path, "line": line.parse::<u32>().context("open --line: a line number")? }),
        ["ai", "wpf"] => json!({ "cmd": "ai.wpf" }),
        ["ai", "wpf", on @ ("on" | "off")] => json!({ "cmd": "ai.wpf", "on": *on == "on" }),
        ["ai", "skills"] => json!({ "cmd": "ai.side", "skills": true }),
        ["ai", "terminal"] => json!({ "cmd": "ai.side", "skills": false }),
        ["ai", "noside"] => json!({ "cmd": "ai.side", "off": true }),
        ["ai", "system" | "prompt"] => json!({ "cmd": "ai.side", "side": "system" }),
        ["ai", "effort", level] => json!({ "cmd": "ai.effort", "level": level }),
        ["ai", "system-prompt", rest @ ..] => json!({ "cmd": "ai.prompt", "text": rest.join(" ") }),
        ["ai", "mode", mode] => json!({ "cmd": "ai.mode", "mode": mode }),
        ["ai", "allow"] => json!({ "cmd": "ai.answer", "allow": true }),
        ["ai", "answer", answers @ ..] if !answers.is_empty() => json!({ "cmd": "ai.answer", "allow": true, "answers": answers }),
        ["ai", "deny", rest @ ..] => json!({ "cmd": "ai.answer", "allow": false, "message": rest.join(" ") }),
        ["diff", "open"] => json!({ "cmd": "diff.open" }),
        ["diff", "open", scope] => json!({ "cmd": "diff.open", "scope": scope }),
        ["diff", "send"] => json!({ "cmd": "diff.review", "what": "send" }),
        ["diff", what @ ("resolve" | "reopen" | "remove"), id] => json!({ "cmd": "diff.review", "what": what, "target": id }),
        ["diff", "reply", id, text @ ..] if !text.is_empty() => json!({ "cmd": "diff.review", "what": "reply", "target": id, "body": text.join(" ") }),
        ["diff", "comment", "--on", target, text @ ..] if !text.is_empty() => json!({ "cmd": "diff.review", "what": "comment", "target": target, "body": text.join(" ") }),
        ["diff", "comment", text @ ..] if !text.is_empty() => json!({ "cmd": "diff.review", "what": "comment", "body": text.join(" ") }),
        ["ai", "accept"] => json!({ "cmd": "ai.accept", "yes": true }),
        ["ai", "keep"] => json!({ "cmd": "ai.accept", "yes": false }),
        ["ai", "diff"] => json!({ "cmd": "ai.diff" }),
        ["ai", "image", path] => json!({ "cmd": "ai.image", "path": path }),
        ["drop", "chat", paths @ ..] if !paths.is_empty() => json!({ "cmd": "drop", "to": "chat", "paths": abs_paths(paths)? }),
        ["drop", "tree", dir, paths @ ..] if !paths.is_empty() => json!({ "cmd": "drop", "to": "tree", "dir": dir.trim_start_matches('.'), "paths": abs_paths(paths)? }),
        ["ai", "use", rest @ ..] if !rest.is_empty() => {
            // `--model ID` anywhere after the name; the rest is the name ("Claude Code").
            let at = rest.iter().position(|w| *w == "--model");
            let model = at.and_then(|i| rest.get(i + 1));
            let name: Vec<&str> = rest.iter().enumerate().filter(|(i, _)| at.is_none_or(|a| *i != a && *i != a + 1)).map(|(_, w)| *w).collect();
            json!({ "cmd": "ai.use", "agent": name.join(" "), "model": model })
        }
        ["web", rest @ ..] => web_request(rest)?,
        ["app", rest @ ..] => app_request(rest, || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map(|_| s)
        })?,
        ["forward", rest @ ..] => forward_request(rest)?,
        ["raw", j] => serde_json::from_str(j).context("raw: bad JSON")?,
        ["ssh", rest @ ..] => ssh_request(rest)?,
        ["wait"] | ["wait", _] => Value::Null,
        _ => bail!("unknown command {:?}\n{CTL_USAGE}", a.join(" ")),
    };
    let sock = match sock {
        Some(s) => s,
        None => match std::env::var_os("NIB_CONTROL") {
            Some(s) if !s.is_empty() && s != "off" && s != "0" => PathBuf::from(s),
            _ => {
                let live = sys::live()?;
                match live.as_slice() {
                    [one] => one.clone(),
                    [] => bail!("no nib window is running"),
                    _ => bail!("{} nib windows are open; pick one with -p PID (see `nib ctl list`)", live.len()),
                }
            }
        },
    };
    if let ["wait", rest @ ..] = a.as_slice() {
        let secs: f64 = rest.first().map(|s| s.parse()).transpose().context("wait: SECS")?.unwrap_or(10.0);
        let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        loop {
            let v = sys::call(&sock, &json!({ "cmd": "pending" }))?;
            let busy = v.get("pending").and_then(Value::as_u64).unwrap_or(0);
            if busy == 0 {
                return Ok(print(&json!({ "ok": true })));
            }
            if std::time::Instant::now() >= until {
                return Ok(print(&json!({ "ok": false, "error": format!("{busy} request(s) still pending") })));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    Ok(print(&sys::call(&sock, &req)?))
}

/// `nib ctl web …` as a request: the same words an agent's `nib-web` takes (`nib_proto::web`).
fn web_request(a: &[&str]) -> anyhow::Result<Value> {
    nib_proto::web::request(a).map_err(|e| match e.starts_with("web: unknown request") {
        true => anyhow::anyhow!("{e}\n{CTL_USAGE}"),
        false => anyhow::anyhow!(e),
    })
}

/// `nib ctl forward …` as a request.
fn forward_request(a: &[&str]) -> anyhow::Result<Value> {
    use anyhow::{bail, Context};
    let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0).with_context(|| format!("forward: {p:?} is not a port (1 to 65535)"));
    Ok(match a {
        ["list"] => json!({ "cmd": "forward.list" }),
        ["rm", local] => json!({ "cmd": "forward.rm", "local": port(local)? }),
        ["add", what] => match what.rsplit_once(':') {
            Some((profile, p)) if !profile.is_empty() => json!({ "cmd": "forward.add", "profile": profile, "port": port(p)? }),
            _ => json!({ "cmd": "forward.add", "port": port(what)? }),
        },
        _ => bail!("forward: list, add [PROFILE:]PORT or rm LOCALPORT\n{CTL_USAGE}"),
    })
}

/// `nib ctl app …` as a request. `stdin`: what `set NAME --json -` reads.
fn app_request(a: &[&str], stdin: impl FnOnce() -> std::io::Result<String>) -> anyhow::Result<Value> {
    use anyhow::{bail, Context};
    let lines = |n: &str| n.parse::<u64>().context("app log: --lines takes a number");
    Ok(match a {
        ["list"] => json!({ "cmd": "app.list" }),
        ["rm", name] => json!({ "cmd": "app.rm", "name": name }),
        ["restart", name] => json!({ "cmd": "app.restart", "name": name }),
        ["restart", name, server] => json!({ "cmd": "app.restart", "name": name, "server": server }),
        [c @ ("up" | "down"), names @ ..] => json!({ "cmd": format!("app.{c}"), "names": names }),
        ["log", name] => json!({ "cmd": "app.log", "name": name }),
        ["log", name, "--lines", n] => json!({ "cmd": "app.log", "name": name, "lines": lines(n)? }),
        ["log", name, server] => json!({ "cmd": "app.log", "name": name, "server": server }),
        ["log", name, server, "--lines", n] => json!({ "cmd": "app.log", "name": name, "server": server, "lines": lines(n)? }),
        ["set", name, "--json", from] => {
            let text = match *from {
                "-" => stdin().context("app set: reading stdin")?,
                file => std::fs::read_to_string(file).with_context(|| format!("app set: {file}"))?,
            };
            let def: Value = serde_json::from_str(&text).context("app set: not JSON")?;
            json!({ "cmd": "app.set", "name": name, "def": def })
        }
        ["set", ..] => bail!("app set NAME --json FILE (or - for stdin)\n{CTL_USAGE}"),
        _ => bail!("app: unknown request {:?}\n{CTL_USAGE}", a.join(" ")),
    })
}

/// `nib ctl ssh …` as a request.
fn ssh_request(a: &[&str]) -> anyhow::Result<Value> {
    use anyhow::{bail, Context};
    let (verb, rest) = a.split_first().context("ssh: list, add, update, remove, test, connect or cancel")?;
    let mut req = json!({ "cmd": format!("ssh.{verb}") });
    let mut positional = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            positional.push(*arg);
            continue;
        };
        match flag {
            "force" | "home" => req[flag] = json!(true),
            "test" => req["test"] = json!(true),
            "unset" => {
                let f = it.next().context("--unset FIELD")?;
                req[*f] = Value::Null;
            }
            "host" | "root" | "user" | "identity" | "jump" | "nibd" | "rename" => {
                req[flag] = json!(it.next().with_context(|| format!("--{flag} needs a value"))?);
            }
            "port" => req["port"] = json!(it.next().context("--port N")?.parse::<u16>().context("--port N")?),
            _ => bail!("ssh {verb}: unknown option --{flag}"),
        }
    }
    match (*verb, positional.as_slice()) {
        ("list" | "cancel", []) => {}
        ("add", [name, host]) => {
            req["name"] = json!(name);
            req["host"] = json!(host);
        }
        ("update" | "remove", [name]) => req["name"] = json!(name),
        ("root", [path]) => req["path"] = json!(path),
        ("test", [name]) | ("connect", [name]) => req["name"] = json!(name),
        ("test", []) if req.get("host").is_some() => {}
        ("connect", []) if req.get("home").is_some() => {}
        _ => bail!("ssh {verb}: wrong arguments (nib ctl help)"),
    }
    Ok(req)
}

/// Paths on this machine, absolute (the window may have another working directory).
fn abs_paths(paths: &[&str]) -> anyhow::Result<Vec<String>> {
    let cwd = std::env::current_dir()?;
    Ok(paths.iter().map(|p| cwd.join(p).to_string_lossy().into_owned()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_no_password_and_no_signed_address() {
        let line = loggable(&json!({ "cmd": "web.fill", "ref": "3", "text": "hunter2 is my password" }));
        assert!(!line.contains("hunter2") && line.contains("<22 chars>") && line.contains("\"ref\":\"3\""), "{line}");
        assert!(!loggable(&json!({ "cmd": "web.type", "text": "s3cret\n" })).contains("s3cret"));
        let line = loggable(&json!({ "cmd": "web.open", "url": "http://ada:pw@127.0.0.1:8888/lab?token=abc123#frag" }));
        assert!(line.contains("http://…@127.0.0.1:8888/lab…") && !line.contains("abc123") && !line.contains("pw@"), "{line}");
        assert_eq!(loggable(&json!({ "cmd": "web.open", "url": "8765" })), json!({ "cmd": "web.open", "url": "8765" }).to_string());
        // What is typed into the editor is the user's own text, and stays readable.
        assert!(loggable(&json!({ "cmd": "type", "text": "x = 1" })).contains("x = 1"));
    }

    #[test]
    fn forward_requests_from_the_command_line() {
        assert_eq!(forward_request(&["list"]).unwrap(), json!({ "cmd": "forward.list" }));
        assert_eq!(forward_request(&["add", "8765"]).unwrap(), json!({ "cmd": "forward.add", "port": 8765 }));
        assert_eq!(forward_request(&["add", "gpu:6006"]).unwrap(), json!({ "cmd": "forward.add", "profile": "gpu", "port": 6006 }));
        assert_eq!(forward_request(&["rm", "6007"]).unwrap(), json!({ "cmd": "forward.rm", "local": 6007 }));
        for wrong in [&["add"][..], &["add", "gpu"], &["add", "gpu:"], &["add", "0"], &["add", "70000"], &["rm"], &["rm", "x"], &["dance"]] {
            assert!(forward_request(wrong).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn an_answer_goes_where_the_request_came_from_once() {
        let (tx, rx) = mpsc::channel();
        let call = Call::then(json!({ "cmd": "web.read" }), move |v| drop(tx.send(v)));
        call.clone().ok(json!({ "text": "hi" }));
        call.err("too late");
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [json!({ "ok": true, "text": "hi" })]);
        Call::internal(json!({})).ok(json!({}));
    }

    #[test]
    fn app_requests_from_the_command_line() {
        let none = || -> std::io::Result<String> { unreachable!("stdin not read") };
        assert_eq!(app_request(&["list"], none).unwrap(), json!({ "cmd": "app.list" }));
        assert_eq!(app_request(&["up"], none).unwrap(), json!({ "cmd": "app.up", "names": [] }));
        assert_eq!(app_request(&["down", "a", "b"], none).unwrap(), json!({ "cmd": "app.down", "names": ["a", "b"] }));
        assert_eq!(app_request(&["restart", "a"], none).unwrap(), json!({ "cmd": "app.restart", "name": "a" }));
        assert_eq!(app_request(&["restart", "a", "api"], none).unwrap(), json!({ "cmd": "app.restart", "name": "a", "server": "api" }));
        assert_eq!(app_request(&["log", "a", "--lines", "5"], none).unwrap(), json!({ "cmd": "app.log", "name": "a", "lines": 5 }));
        assert_eq!(app_request(&["log", "a", "web", "--lines", "5"], none).unwrap(), json!({ "cmd": "app.log", "name": "a", "server": "web", "lines": 5 }));
        let json = r#"{ "title": "Demo", "page": { "server": "web", "path": "/" }, "servers": { "web": { "cmd": "npm run dev", "port": 5173, "env": { "A": "1" } } } }"#;
        let set = app_request(&["set", "demo", "--json", "-"], || Ok(json.to_string())).unwrap();
        assert_eq!(set["cmd"], "app.set");
        // What the window turns into the daemon's definition.
        let def = serde_json::from_value::<nib_proto::AppDef>(set["def"].clone()).unwrap();
        assert_eq!((def.title.as_deref(), def.servers["web"].port), (Some("Demo"), Some(5173)));
        for wrong in [&["set", "web"][..], &["set", "web", "--cmd", "x"], &["log"], &["restart"], &["dance"]] {
            assert!(app_request(wrong, none).is_err(), "{wrong:?}");
        }
        assert!(app_request(&["set", "web", "--json", "-"], || Ok("{ not json".into())).is_err());
    }
}
