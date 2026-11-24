//! Language servers. nibd owns the server processes, so they run where the code and its
//! toolchain / virtualenv live. Which server serves which file is data: built-in specs
//! ([`builtin`]: ty, rust-analyzer, gopls, typescript-language-server, clangd) with the client
//! config's `[lsp.<name>]` entries applied over them. One server runs per (spec, root), started
//! when the first matching file is synced. The client sends buffer contents (`DocSync`) and typed `Request::Lsp` queries;
//! answers are sent later through the connection's writer channel, so a slow server never blocks
//! file I/O. Positions on the wire are (line, char column); `ty` agrees to UTF-32 positions, which
//! are exactly that, and other servers' UTF-16 / UTF-8 columns are converted here.

use nib_proto::{
    ErrorKind, FileEdit, HoverInfo, Location, LogLevel, LspOp, LspServer, Pos, ReqId, Response,
    RpcError, ServerMsg, ServerNotify, TextEdit,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use crate::exe::find_exe;

/// Give up restarting a server that keeps dying after this many starts.
const MAX_STARTS: u32 = 3;
const INIT_ID: i64 = 0;

/// Which server serves which files.
#[derive(Clone, Debug, PartialEq)]
struct Spec {
    name: String,
    /// `None`: python's `ty` lookup ([`find_ty`]). Empty: turned off.
    command: Option<Vec<String>>,
    extensions: Vec<String>,
    /// Root marker files; the topmost directory holding one (within the project) is the root.
    roots: Vec<String>,
}

fn spec(name: &str, command: &[&str], extensions: &[&str], roots: &[&str]) -> Spec {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Spec {
        name: name.into(),
        command: (!command.is_empty()).then(|| v(command)),
        extensions: v(extensions),
        roots: v(roots),
    }
}

/// The servers nibd knows without any config. Each is only started when a file it serves is
/// opened, and only reported missing then.
fn builtin() -> Vec<Spec> {
    vec![
        spec("python", &[], &["py", "pyi"], &[]),
        spec("rust", &["rust-analyzer"], &["rs"], &["Cargo.toml"]),
        spec("go", &["gopls"], &["go"], &["go.work", "go.mod"]),
        spec(
            "typescript",
            &["typescript-language-server", "--stdio"],
            &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
            &["tsconfig.json", "jsconfig.json", "package.json"],
        ),
        spec("typst", &["tinymist"], &["typ"], &[]),
        spec("latex", &["texlab"], &["tex", "bib"], &[]),
        spec(
            "c",
            &["clangd"],
            &["c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"],
            &["compile_commands.json", "compile_flags.txt", "CMakeLists.txt"],
        ),
    ]
}

/// Built-ins with the client's entries applied: an entry changes the fields it sets, a new name
/// needs a command and extensions, and an extension a later entry claims leaves the others.
fn merge(mut specs: Vec<Spec>, user: &[LspServer]) -> (Vec<Spec>, Vec<String>) {
    let mut warnings = Vec::new();
    for u in user {
        let i = match specs.iter().position(|s| s.name == u.name) {
            Some(i) => i,
            None if u.command.is_some() && !u.extensions.is_empty() => {
                specs.push(Spec { name: u.name.clone(), command: None, extensions: vec![], roots: vec![] });
                specs.len() - 1
            }
            None => {
                warnings.push(format!("[lsp.{}]: a new language server needs `command` and `extensions`", u.name));
                continue;
            }
        };
        if let Some(c) = &u.command {
            specs[i].command = Some(c.clone());
        }
        if !u.roots.is_empty() {
            specs[i].roots = u.roots.clone();
        }
        if !u.extensions.is_empty() {
            let exts: Vec<String> = u.extensions.iter().map(|e| e.trim_start_matches('.').to_string()).collect();
            for s in specs.iter_mut() {
                s.extensions.retain(|e| !exts.contains(e));
            }
            specs[i].extensions = exts;
        }
    }
    specs.retain(|s| s.command.as_ref().is_none_or(|c| !c.is_empty()));
    (specs, warnings)
}

/// The LSP `languageId` of a file a server named `server` serves.
fn language_id<'a>(server: &'a str, path: &Path) -> &'a str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" => "c",
        "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        _ => server,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Encoding {
    Utf32,
    Utf16,
    Utf8,
}

/// Buffers the client has synced: path -> (version, text).
type Docs = Arc<Mutex<HashMap<PathBuf, (u64, String)>>>;

/// Every language server of one connection, by (index into `specs`, root).
pub struct Lsp {
    root: PathBuf,
    specs: Vec<Spec>,
    tx: Sender<ServerMsg>,
    clients: HashMap<(usize, PathBuf), Client>,
}

/// One server process (restarted when it dies) and the buffers it was given.
struct Client {
    spec: Spec,
    root: PathBuf,
    tx: Sender<ServerMsg>,
    docs: Docs,
    server: Option<Server>,
    starts: u32,
    /// Why the last start failed.
    failed: Option<String>,
    /// A start failure was already reported to the client.
    reported: bool,
}

struct Server {
    child: Child,
    io: Arc<Mutex<Io>>,
}

/// Shared by the request side and the server's reader thread.
struct Io {
    /// `None` once the server is being shut down.
    stdin: Option<ChildStdin>,
    next_id: i64,
    /// `Some` until `initialize` is answered: messages wait here.
    queue: Option<Vec<Value>>,
    pending: HashMap<i64, Query>,
    opened: HashSet<PathBuf>,
    enc: Encoding,
    dead: bool,
    /// The spec's name, the `languageId` of most of its files.
    name: String,
}

struct Query {
    client: ReqId,
    op: LspOp,
    path: PathBuf,
    pos: Pos,
}

impl Io {
    fn write_now(&mut self, msg: &Value) {
        let Some(stdin) = &mut self.stdin else { return };
        let body = msg.to_string();
        let ok = write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).and_then(|_| stdin.flush());
        if ok.is_err() {
            self.dead = true;
        }
    }

    fn send(&mut self, msg: Value) {
        match &mut self.queue {
            Some(q) => q.push(msg),
            None => self.write_now(&msg),
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn open(&mut self, path: &Path, version: u64, text: &str) {
        let uri = path_to_uri(path);
        if self.opened.insert(path.to_path_buf()) {
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": language_id(&self.name, path), "version": version, "text": text } }),
            );
        } else {
            // A change event without a range replaces the whole text (always allowed).
            self.notify(
                "textDocument/didChange",
                json!({ "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }] }),
            );
        }
    }
}

impl Lsp {
    /// `user`: the client config's `[lsp.<name>]` entries; mistakes in them go to the client log.
    pub fn new(root: PathBuf, user: &[LspServer], tx: Sender<ServerMsg>) -> Self {
        let (specs, warnings) = merge(builtin(), user);
        for msg in warnings {
            let _ = tx.send(ServerMsg::Notify(ServerNotify::Log { level: LogLevel::Warn, msg }));
        }
        Self { root, specs, tx, clients: HashMap::new() }
    }

    /// File extensions some server serves: the client syncs and asks about only these files.
    pub fn extensions(&self) -> Vec<String> {
        self.specs.iter().flat_map(|s| s.extensions.iter().cloned()).collect()
    }

    /// The server for `path`: its spec and root.
    fn key(&self, path: &Path) -> Option<(usize, PathBuf)> {
        let ext = path.extension()?.to_str()?;
        let i = self.specs.iter().position(|s| s.extensions.iter().any(|e| e == ext))?;
        if !path.starts_with(&self.root) {
            // A library file reached through go-to-definition: ask a server already running.
            if let Some(k) = self.clients.keys().find(|(j, _)| *j == i) {
                return Some(k.clone());
            }
            return Some((i, self.root.clone()));
        }
        Some((i, project_root(&self.root, &self.specs[i].roots, path)))
    }

    fn client(&mut self, key: (usize, PathBuf)) -> &mut Client {
        let (specs, tx) = (&self.specs, &self.tx);
        self.clients
            .entry(key)
            .or_insert_with_key(|(i, root)| Client::new(specs[*i].clone(), root.clone(), tx.clone()))
    }

    /// The client's buffer for `path` now has this text.
    pub fn sync(&mut self, path: PathBuf, version: u64, text: String) {
        if let Some(key) = self.key(&path) {
            self.client(key).sync(path, version, text);
        }
    }

    pub fn close(&mut self, path: PathBuf) {
        if let Some(c) = self.key(&path).and_then(|k| self.clients.get_mut(&k)) {
            c.close(path);
        }
    }

    /// Files nibd wrote: the servers re-read the ones that aren't open buffers.
    pub fn files_changed(&mut self, paths: &[PathBuf]) {
        let mut by: HashMap<(usize, PathBuf), Vec<&PathBuf>> = HashMap::new();
        for p in paths {
            if let Some(k) = self.key(p).filter(|k| self.clients.contains_key(k)) {
                by.entry(k).or_default().push(p);
            }
        }
        for (k, paths) in by {
            self.clients.get_mut(&k).unwrap().files_changed(&paths);
        }
    }

    /// Send a query; the answer goes to the client as the response to `client`.
    pub fn request(&mut self, client: ReqId, path: PathBuf, pos: Pos, op: LspOp) -> Result<(), RpcError> {
        let Some(key) = self.key(&path) else {
            return Err(RpcError::new(ErrorKind::BadRequest, "no language server for this file type"));
        };
        self.client(key).request(client, path, pos, op)
    }
}

/// The topmost directory from `path` up to `root` holding one of `markers`; else `root`.
fn project_root(root: &Path, markers: &[String], path: &Path) -> PathBuf {
    let mut best = root;
    for dir in path.ancestors().skip(1).take_while(|d| d.starts_with(root)) {
        if markers.iter().any(|m| dir.join(m).exists()) {
            best = dir;
        }
    }
    best.to_path_buf()
}

impl Client {
    fn new(spec: Spec, root: PathBuf, tx: Sender<ServerMsg>) -> Self {
        Self {
            spec,
            root,
            tx,
            docs: Default::default(),
            server: None,
            starts: 0,
            failed: None,
            reported: false,
        }
    }

    /// The running server, (re)started if needed.
    fn server(&mut self) -> Result<&Arc<Mutex<Io>>, RpcError> {
        let dead = self.server.as_ref().is_some_and(|s| s.io.lock().unwrap().dead);
        if dead {
            self.server = None; // Drop kills it
        }
        if self.server.is_none() {
            if self.starts >= MAX_STARTS {
                let msg = self.failed.clone().unwrap_or_else(|| {
                    format!("the {} language server keeps exiting; see the nibd log", self.spec.name)
                });
                return Err(RpcError::new(ErrorKind::Io, msg));
            }
            self.starts += 1;
            let server = self.spawn().map_err(|msg| {
                self.failed = Some(msg.clone());
                if !self.reported {
                    self.reported = true;
                    let _ = self.tx.send(ServerMsg::Notify(ServerNotify::Log {
                        level: LogLevel::Warn,
                        msg: msg.clone(),
                    }));
                }
                RpcError::new(ErrorKind::NotFound, msg)
            })?;
            self.server = Some(server);
        }
        Ok(&self.server.as_ref().unwrap().io)
    }

    fn spawn(&self) -> Result<Server, String> {
        let spec_name = &self.spec.name;
        let argv = match &self.spec.command {
            Some(c) => {
                let mut argv = c.clone();
                if let Some(exe) = find_exe(&argv[0]) {
                    argv[0] = exe.to_string_lossy().into_owned();
                }
                argv
            }
            None => find_ty().ok_or_else(|| {
                "ty not found: install it with `uv tool install ty`, or set `command` under \
                 [lsp.python] in the config"
                    .to_string()
            })?,
        };
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // nibd's stderr is the client's per-target log
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => crate::exe::not_found(
                    &argv[0],
                    &format!("{spec_name} files"),
                    &format!("set `command` under [lsp.{spec_name}] in the config"),
                ),
                _ => format!("starting {:?}: {e}", argv.join(" ")),
            })?;
        log!("started language server {:?}", argv.join(" "));
        let stdout = child.stdout.take().unwrap();
        let mut io = Io {
            stdin: child.stdin.take(),
            next_id: INIT_ID + 1,
            queue: Some(Vec::new()),
            pending: HashMap::new(),
            opened: HashSet::new(),
            enc: Encoding::Utf16,
            dead: false,
            name: spec_name.clone(),
        };
        let root_uri = path_to_uri(&self.root);
        let name = self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        io.write_now(&json!({
            "jsonrpc": "2.0", "id": INIT_ID, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "nibd", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": name }],
                "capabilities": {
                    "general": { "positionEncodings": ["utf-32", "utf-16"] },
                    "workspace": {
                        "workspaceFolders": true,
                        "didChangeWatchedFiles": { "dynamicRegistration": true },
                        "workspaceEdit": { "documentChanges": true },
                    },
                    "textDocument": {
                        "synchronization": { "didSave": false },
                        "definition": { "linkSupport": true },
                        "references": {},
                        "documentHighlight": {},
                        "rename": { "prepareSupport": true },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                    },
                },
            },
        }));
        // Buffers synced before a restart are opened again.
        for (path, (version, text)) in self.docs.lock().unwrap().iter() {
            io.open(path, *version, text);
        }
        let io = Arc::new(Mutex::new(io));
        let (io2, docs, tx) = (io.clone(), self.docs.clone(), self.tx.clone());
        std::thread::spawn(move || read_loop(stdout, io2, docs, tx));
        Ok(Server { child, io })
    }

    fn sync(&mut self, path: PathBuf, version: u64, text: String) {
        {
            let mut docs = self.docs.lock().unwrap();
            if docs.get(&path).is_some_and(|(v, _)| *v >= version) {
                return;
            }
            docs.insert(path.clone(), (version, text.clone()));
        }
        if let Ok(io) = self.server() {
            io.lock().unwrap().open(&path, version, &text);
        }
    }

    fn close(&mut self, path: PathBuf) {
        self.docs.lock().unwrap().remove(&path);
        if let Some(s) = &self.server {
            let mut io = s.io.lock().unwrap();
            if io.opened.remove(&path) {
                io.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(&path) } }));
            }
        }
    }

    fn files_changed(&mut self, paths: &[&PathBuf]) {
        let Some(s) = &self.server else { return };
        let changes: Vec<Value> = paths.iter().map(|p| json!({ "uri": path_to_uri(p), "type": 2 })).collect();
        if !changes.is_empty() {
            s.io.lock().unwrap().notify("workspace/didChangeWatchedFiles", json!({ "changes": changes }));
        }
    }

    fn request(&mut self, client: ReqId, path: PathBuf, pos: Pos, op: LspOp) -> Result<(), RpcError> {
        let docs = self.docs.clone();
        let io = self.server()?.clone();
        let enc = io.lock().unwrap().enc;
        let character = {
            let docs = docs.lock().unwrap();
            let mut lines = Lines::new(&docs);
            to_units(lines.line(&path, pos.line).unwrap_or_default(), pos.col, enc)
        };
        let mut params = json!({
            "textDocument": { "uri": path_to_uri(&path) },
            "position": { "line": pos.line, "character": character },
        });
        let method = match &op {
            LspOp::Definition => "textDocument/definition",
            LspOp::References => {
                params["context"] = json!({ "includeDeclaration": true });
                "textDocument/references"
            }
            LspOp::Highlights => "textDocument/documentHighlight",
            LspOp::PrepareRename => "textDocument/prepareRename",
            LspOp::Rename { new_name } => {
                params["newName"] = json!(new_name);
                "textDocument/rename"
            }
            LspOp::Hover => "textDocument/hover",
        };
        let mut io = io.lock().unwrap();
        let id = io.next_id;
        io.next_id += 1;
        io.pending.insert(id, Query { client, op, path, pos });
        io.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        Ok(())
    }
}

impl Drop for Server {
    /// Ask the server to exit and close its stdin. Killing alone isn't enough: launched through
    /// `uvx`, the real server is a grandchild that would keep its stdout open, and with it our
    /// reader thread and the connection's writer.
    fn drop(&mut self) {
        {
            let mut io = self.io.lock().unwrap();
            io.queue = None;
            io.write_now(&json!({ "jsonrpc": "2.0", "id": -1, "method": "shutdown" }));
            io.write_now(&json!({ "jsonrpc": "2.0", "method": "exit" }));
            io.stdin = None;
        }
        for _ in 0..100 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// finding the server
// ---------------------------------------------------------------------------------------------

/// `ty server` found like any command; else through uv.
fn find_ty() -> Option<Vec<String>> {
    let find = find_exe;
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    if let Some(ty) = find("ty") {
        return Some(vec![s(ty), "server".into()]);
    }
    if let Some(uvx) = find("uvx") {
        return Some(vec![s(uvx), "ty".into(), "server".into()]);
    }
    find("uv").map(|uv| vec![s(uv), "tool".into(), "run".into(), "ty".into(), "server".into()])
}

// ---------------------------------------------------------------------------------------------
// reading the server
// ---------------------------------------------------------------------------------------------

/// A header line longer than this is not a language server's.
const MAX_HEADER: u64 = 8 * 1024;
/// The largest message taken (a whole project's diagnostics or symbols is a few megabytes).
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// The next message of a language server. `None`: its output ended, or is not the protocol
/// (and then it is not read further: the server is taken for gone). A server is a program the
/// project chose, so what it sends is bounded before any of it is kept: a header line that
/// never ends, or a length of gigabytes, would otherwise be memory nibd asks for at its word.
fn read_msg(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if std::io::Read::take(&mut *r, MAX_HEADER).read_line(&mut line).ok()? == 0 {
            return None;
        }
        if !line.ends_with('\n') {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse::<usize>().ok();
            }
        }
    }
    let mut buf = vec![0; len.filter(|n| *n <= MAX_MESSAGE)?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn read_loop(stdout: ChildStdout, io: Arc<Mutex<Io>>, docs: Docs, tx: Sender<ServerMsg>) {
    let mut r = BufReader::new(stdout);
    while let Some(msg) = read_msg(&mut r) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();
        match (id, method) {
            // A request from the server: answer so it doesn't wait.
            (Some(id), Some(method)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let n = msg["params"]["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    }
                    _ => Value::Null,
                };
                io.lock().unwrap().send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            (None, Some("window/showMessage")) if msg["params"]["type"].as_i64() == Some(1) => {
                let text = msg["params"]["message"].as_str().unwrap_or_default();
                let _ = tx.send(ServerMsg::Notify(ServerNotify::Log {
                    level: LogLevel::Warn,
                    msg: format!("language server: {text}"),
                }));
            }
            (None, Some(_)) => {} // diagnostics, logs, progress: not used yet
            (Some(id), None) => {
                let Some(id) = id.as_i64() else { continue };
                if id == INIT_ID {
                    on_initialized(&io, &msg);
                    continue;
                }
                let (query, enc) = {
                    let mut io = io.lock().unwrap();
                    (io.pending.remove(&id), io.enc)
                };
                let Some(q) = query else { continue };
                let result = match msg.get("error") {
                    Some(e) => Err(RpcError::new(
                        ErrorKind::Io,
                        e["message"].as_str().unwrap_or("language server error").to_string(),
                    )),
                    None => {
                        let docs = docs.lock().unwrap();
                        Ok(convert(&q, &msg["result"], enc, &mut Lines::new(&docs)))
                    }
                };
                if tx.send(ServerMsg::Response { id: q.client, result }).is_err() {
                    return;
                }
            }
            (None, None) => {}
        }
    }
    // The server exited: fail whatever is still waiting.
    let pending = {
        let mut io = io.lock().unwrap();
        io.dead = true;
        std::mem::take(&mut io.pending)
    };
    for q in pending.into_values() {
        let err = RpcError::new(ErrorKind::Io, "the language server exited; see the nibd log");
        let _ = tx.send(ServerMsg::Response { id: q.client, result: Err(err) });
    }
}

fn on_initialized(io: &Mutex<Io>, msg: &Value) {
    let mut io = io.lock().unwrap();
    if msg.get("error").is_some() {
        log!("language server failed to initialize: {}", msg["error"]);
        io.dead = true;
        return;
    }
    io.enc = match msg["result"]["capabilities"]["positionEncoding"].as_str() {
        Some("utf-32") => Encoding::Utf32,
        Some("utf-8") => Encoding::Utf8,
        _ => Encoding::Utf16,
    };
    io.write_now(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    for m in io.queue.take().unwrap_or_default() {
        io.write_now(&m);
    }
}

// ---------------------------------------------------------------------------------------------
// converting answers
// ---------------------------------------------------------------------------------------------

/// Line text by path: synced buffers first, else the file on disk (read once per answer).
struct Lines<'a> {
    docs: &'a HashMap<PathBuf, (u64, String)>,
    files: HashMap<PathBuf, Option<String>>,
}

impl<'a> Lines<'a> {
    fn new(docs: &'a HashMap<PathBuf, (u64, String)>) -> Self {
        Self { docs, files: HashMap::new() }
    }

    fn line(&mut self, path: &Path, line: u32) -> Option<&str> {
        let text = match self.docs.get(path) {
            Some((_, t)) => t.as_str(),
            None => self
                .files
                .entry(path.to_path_buf())
                .or_insert_with(|| std::fs::read_to_string(path).ok())
                .as_deref()?,
        };
        text.split('\n').nth(line as usize).map(|l| l.strip_suffix('\r').unwrap_or(l))
    }

    fn pos(&mut self, path: &Path, v: &Value, enc: Encoding) -> Pos {
        let line = v["line"].as_u64().unwrap_or(0) as u32;
        let units = v["character"].as_u64().unwrap_or(0) as u32;
        let col = match enc {
            Encoding::Utf32 => units,
            _ => from_units(self.line(path, line).unwrap_or_default(), units, enc),
        };
        Pos { line, col }
    }

    fn location(&mut self, path: PathBuf, range: &Value, enc: Encoding) -> Location {
        let start = self.pos(&path, &range["start"], enc);
        let end = self.pos(&path, &range["end"], enc);
        let line_text = self.line(&path, start.line).unwrap_or_default().to_string();
        Location { path: nib_proto::path::to_wire(&path), start, end, line_text }
    }
}

fn convert(q: &Query, result: &Value, enc: Encoding, lines: &mut Lines) -> Response {
    match &q.op {
        LspOp::Definition | LspOp::References => {
            // Location | Location[] | LocationLink[] | null
            let items: Vec<&Value> = match result {
                Value::Array(a) => a.iter().collect(),
                Value::Object(_) => vec![result],
                _ => vec![],
            };
            let mut locs: Vec<Location> = items
                .into_iter()
                .filter_map(|it| {
                    let (uri, range) = match it.get("targetUri") {
                        Some(u) => (u, &it["targetSelectionRange"]),
                        None => (it.get("uri")?, &it["range"]),
                    };
                    Some(lines.location(uri_to_path(uri.as_str()?), range, enc))
                })
                .collect();
            locs.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
            locs.dedup_by(|a, b| a.path == b.path && a.start == b.start);
            Response::Locations(locs)
        }
        LspOp::Highlights => {
            let mut locs: Vec<Location> = result
                .as_array()
                .into_iter()
                .flatten()
                .map(|h| lines.location(q.path.clone(), &h["range"], enc))
                .collect();
            locs.sort_by_key(|l| l.start);
            Response::Locations(locs)
        }
        LspOp::PrepareRename => {
            // Range | { range, placeholder } | { defaultBehavior } | null
            let range = if result.get("start").is_some() {
                Some((result, None))
            } else if let Some(r) = result.get("range") {
                Some((r, result["placeholder"].as_str()))
            } else if result.get("defaultBehavior").is_some() {
                let text = lines.line(&q.path, q.pos.line).unwrap_or_default();
                let (a, b) = word_at(text, q.pos.col);
                let line = q.pos.line;
                return Response::RenameRange((a < b).then(|| {
                    let name: String = text.chars().skip(a as usize).take((b - a) as usize).collect();
                    (Pos { line, col: a }, Pos { line, col: b }, name)
                }));
            } else {
                None
            };
            Response::RenameRange(range.map(|(r, placeholder)| {
                let start = lines.pos(&q.path, &r["start"], enc);
                let end = lines.pos(&q.path, &r["end"], enc);
                let name = match placeholder {
                    Some(p) => p.to_string(),
                    None => {
                        let text = lines.line(&q.path, start.line).unwrap_or_default();
                        let n = if end.line == start.line { end.col.saturating_sub(start.col) } else { u32::MAX };
                        text.chars().skip(start.col as usize).take(n as usize).collect()
                    }
                };
                (start, end, name)
            }))
        }
        LspOp::Rename { .. } => {
            let mut per_file: Vec<(PathBuf, &Value)> = Vec::new();
            if let Some(changes) = result.get("changes").and_then(Value::as_object) {
                per_file.extend(changes.iter().map(|(uri, edits)| (uri_to_path(uri), edits)));
            }
            for dc in result.get("documentChanges").and_then(Value::as_array).into_iter().flatten() {
                match dc["textDocument"]["uri"].as_str() {
                    Some(uri) => per_file.push((uri_to_path(uri), &dc["edits"])),
                    None => log!("rename: ignoring file operation {dc}"),
                }
            }
            let files = per_file
                .into_iter()
                .map(|(path, edits)| {
                    let edits = edits
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|e| TextEdit {
                            start: lines.pos(&path, &e["range"]["start"], enc),
                            end: lines.pos(&path, &e["range"]["end"], enc),
                            new_text: e["newText"].as_str().unwrap_or_default().to_string(),
                        })
                        .collect();
                    FileEdit { path: nib_proto::path::to_wire(&path), edits }
                })
                .collect();
            Response::Edits(files)
        }
        LspOp::Hover => {
            let markdown = hover_markdown(&result["contents"]);
            if markdown.trim().is_empty() {
                return Response::Hover(None);
            }
            let (start, end) = match result.get("range") {
                Some(r) if r.is_object() => (lines.pos(&q.path, &r["start"], enc), lines.pos(&q.path, &r["end"], enc)),
                _ => {
                    let (a, b) = word_at(lines.line(&q.path, q.pos.line).unwrap_or_default(), q.pos.col);
                    (Pos { line: q.pos.line, col: a }, Pos { line: q.pos.line, col: b })
                }
            };
            Response::Hover(Some(HoverInfo { markdown, start, end }))
        }
    }
}

/// Hover contents as Markdown: `MarkupContent | MarkedString | MarkedString[]`, where a
/// MarkedString is plain Markdown or `{ language, value }` (a code block).
fn hover_markdown(contents: &Value) -> String {
    match contents {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().map(hover_markdown).filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n\n---\n\n"),
        Value::Object(o) => {
            let value = o.get("value").and_then(Value::as_str).unwrap_or_default();
            match (o.get("kind").and_then(Value::as_str), o.get("language").and_then(Value::as_str)) {
                (_, Some(lang)) => format!("```{lang}\n{value}\n```"),
                // Plain text keeps its line breaks and isn't read as Markdown.
                (Some("plaintext"), _) => format!("```text\n{value}\n```"),
                _ => value.to_string(),
            }
        }
        _ => String::new(),
    }
}

/// Identifier around char column `col`: (start, end) columns.
fn word_at(line: &str, col: u32) -> (u32, u32) {
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: &char| c.is_alphanumeric() || *c == '_';
    let col = (col as usize).min(chars.len());
    let mut a = col;
    while a > 0 && is_word(&chars[a - 1]) {
        a -= 1;
    }
    let mut b = col;
    while b < chars.len() && is_word(&chars[b]) {
        b += 1;
    }
    (a as u32, b as u32)
}

/// Char column -> the server's column units.
fn to_units(line: &str, col: u32, enc: Encoding) -> u32 {
    let chars = line.chars().take(col as usize);
    match enc {
        Encoding::Utf32 => col,
        Encoding::Utf16 => chars.map(|c| c.len_utf16() as u32).sum(),
        Encoding::Utf8 => chars.map(|c| c.len_utf8() as u32).sum(),
    }
}

/// The server's column units -> char column.
fn from_units(line: &str, units: u32, enc: Encoding) -> u32 {
    let mut n = 0;
    let mut col = 0;
    for c in line.chars() {
        if n >= units {
            break;
        }
        n += match enc {
            Encoding::Utf16 => c.len_utf16() as u32,
            Encoding::Utf8 => c.len_utf8() as u32,
            Encoding::Utf32 => 1,
        };
        col += 1;
    }
    col
}

// ---------------------------------------------------------------------------------------------
// edits on disk
// ---------------------------------------------------------------------------------------------

/// Apply non-overlapping edits (char-column positions) to `text`.
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let line_starts: Vec<usize> =
        std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let offset = |p: Pos| -> usize {
        let Some(&ls) = line_starts.get(p.line as usize) else { return text.len() };
        let le = line_starts.get(p.line as usize + 1).map_or(text.len(), |e| e - 1);
        text[ls..le].char_indices().nth(p.col as usize).map_or(le, |(i, _)| ls + i)
    };
    let mut spans: Vec<(usize, usize, &str)> =
        edits.iter().map(|e| (offset(e.start), offset(e.end), e.new_text.as_str())).collect();
    spans.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = text.to_string();
    for (a, b, t) in spans {
        out.replace_range(a..b.max(a), t);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// file URIs
// ---------------------------------------------------------------------------------------------

fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from(if s.starts_with('/') { "file://" } else { "file:///" });
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            b':' if cfg!(windows) => out.push(':'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn uri_to_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&out).into_owned();
    // file:///C:/x -> C:/x; servers may send a lowercase drive (file:///c%3A/x), and the client
    // compares paths as strings, so make it the uppercase one canonicalize gives.
    if cfg!(windows) && s.len() > 2 && s.as_bytes()[2] == b':' {
        let mut d = s[1..].to_string();
        d[..1].make_ascii_uppercase();
        return PathBuf::from(d);
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_servers_message_is_bounded_before_it_is_kept() {
        let msg = |text: &str| read_msg(&mut std::io::Cursor::new(text.as_bytes().to_vec()));
        assert_eq!(msg("Content-Length: 2\r\nContent-Type: x\r\n\r\n{}"), Some(serde_json::json!({})));
        // A length nobody would send: refused before a byte is asked of the system for it.
        assert_eq!(msg(&format!("Content-Length: {}\r\n\r\n{{}}", usize::MAX)), None);
        assert_eq!(msg(&format!("Content-Length: {}\r\n\r\n{{}}", MAX_MESSAGE + 1)), None);
        // A header line that never ends is read no further than a header may be long.
        struct Endless(u64);
        impl std::io::Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += buf.len() as u64;
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        let mut endless = BufReader::new(Endless(0));
        assert_eq!(read_msg(&mut endless), None);
        assert!(endless.get_ref().0 <= MAX_HEADER + 64 * 1024, "read {} bytes of a header", endless.get_ref().0);
    }

    #[test]
    fn config_over_builtins() {
        let u = |name: &str, command: Option<&[&str]>, exts: &[&str]| LspServer {
            name: name.into(),
            command: command.map(|c| c.iter().map(|s| s.to_string()).collect()),
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            roots: vec![],
        };
        let (specs, warnings) = merge(
            builtin(),
            &[
                u("pyright", Some(&["pyright-langserver", "--stdio"]), &[".py"]),
                u("rust", Some(&["ra"]), &[]),
                u("go", Some(&[]), &[]),
                u("zig", None, &["zig"]),
            ],
        );
        let by = |n: &str| specs.iter().find(|s| s.name == n);
        assert_eq!(by("python").unwrap().extensions, ["pyi"]);
        assert_eq!(by("pyright").unwrap().extensions, ["py"]);
        assert_eq!(by("rust").unwrap().command.as_deref(), Some(&["ra".to_string()][..]));
        assert_eq!(by("rust").unwrap().roots, ["Cargo.toml"]);
        assert!(by("go").is_none() && by("zig").is_none());
        assert_eq!(warnings.len(), 1);
        assert_eq!(language_id("typescript", Path::new("a.tsx")), "typescriptreact");
        assert_eq!(language_id("rust", Path::new("a.rs")), "rust");
    }

    #[test]
    fn roots_are_topmost_marker() {
        let dir = std::env::temp_dir().join(format!("nibd-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws/crates/a/src")).unwrap();
        std::fs::write(dir.join("ws/Cargo.toml"), "").unwrap();
        std::fs::write(dir.join("ws/crates/a/Cargo.toml"), "").unwrap();
        let m = ["Cargo.toml".to_string()];
        assert_eq!(project_root(&dir, &m, &dir.join("ws/crates/a/src/lib.rs")), dir.join("ws"));
        assert_eq!(project_root(&dir, &m, &dir.join("other/x.rs")), dir);
        assert_eq!(project_root(&dir, &[], &dir.join("ws/crates/a/src/lib.rs")), dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uri_round_trip() {
        let p = Path::new("/tmp/a b/ü#.py");
        assert_eq!(path_to_uri(p), "file:///tmp/a%20b/%C3%BC%23.py");
        assert_eq!(uri_to_path(&path_to_uri(p)), p);
    }

    #[test]
    fn units() {
        let line = "y = greet(\"é😀\")";
        assert_eq!(to_units(line, 14, Encoding::Utf16), 15);
        assert_eq!(from_units(line, 15, Encoding::Utf16), 14);
        assert_eq!(from_units(line, to_units(line, 13, Encoding::Utf8), Encoding::Utf8), 13);
    }

    #[test]
    fn hover_contents_to_markdown() {
        let md = |v: Value| hover_markdown(&v);
        assert_eq!(md(json!({ "kind": "markdown", "value": "**x**" })), "**x**");
        assert_eq!(md(json!({ "kind": "plaintext", "value": "a\nb" })), "```text\na\nb\n```");
        assert_eq!(
            md(json!([{ "language": "python", "value": "def f()" }, "", "Docs."])),
            "```python\ndef f()\n```\n\n---\n\nDocs."
        );
        assert_eq!(md(Value::Null), "");
    }

    #[test]
    fn edits_apply_back_to_front() {
        let e = |l, a, b, t: &str| TextEdit {
            start: Pos { line: l, col: a },
            end: Pos { line: l, col: b },
            new_text: t.into(),
        };
        let text = "def f(): pass\nx = f() + f()\n";
        let out = apply_edits(text, &[e(0, 4, 5, "go"), e(1, 4, 5, "go"), e(1, 10, 11, "go")]);
        assert_eq!(out, "def go(): pass\nx = go() + go()\n");
        assert_eq!(apply_edits("é = 1\n", &[e(0, 0, 1, "e")]), "e = 1\n");
    }

    /// Drives a real `ty` (skipped when none is installed): sync a buffer, then each query.
    #[test]
    fn ty_end_to_end() {
        let dir = std::env::temp_dir().join(format!("nibd-lsp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // As the daemon makes a root: `canonicalize` alone gives `\\?\C:\…` on Windows.
        let dir = nib_proto::path::canonical(&dir).unwrap();
        if find_ty().is_none() {
            eprintln!("ty not installed; skipping");
            return;
        }
        std::fs::write(dir.join("b.py"), "from a import greet\n\ngreet(\"q\")\n").unwrap();
        // The buffer differs from the disk: queries must see the synced text.
        std::fs::write(dir.join("a.py"), "").unwrap();
        let text = "def greet(name):\n    \"\"\"Say hi.\"\"\"\n    return name\n\nx = greet(\"é😀\"); y = greet(\"b\")\n";
        let (tx, rx) = std::sync::mpsc::channel();
        let mut lsp = Lsp::new(dir.clone(), &[], tx);
        let a = dir.join("a.py");
        lsp.sync(a.clone(), 1, text.into());
        let mut ask = |id: ReqId, line, col, op| {
            lsp.request(id, a.clone(), Pos { line, col }, op).unwrap();
            loop {
                match rx.recv_timeout(std::time::Duration::from_secs(30)).expect("answer") {
                    ServerMsg::Response { id: i, result } if i == id => return result.unwrap(),
                    _ => {}
                }
            }
        };
        let at = |line, col| Pos { line, col };
        // The second call on line 3 sits after the emoji: char columns must line up.
        match ask(1, 4, 23, LspOp::Definition) {
            Response::Locations(l) => {
                assert_eq!(l.len(), 1);
                assert_eq!((l[0].start, l[0].end), (at(0, 4), at(0, 9)));
                assert_eq!(l[0].line_text, "def greet(name):");
            }
            r => panic!("{r:?}"),
        }
        match ask(2, 0, 5, LspOp::Highlights) {
            Response::Locations(l) => {
                let starts: Vec<Pos> = l.iter().map(|l| l.start).collect();
                assert_eq!(starts, [at(0, 4), at(4, 4), at(4, 21)]);
            }
            r => panic!("{r:?}"),
        }
        match ask(3, 0, 5, LspOp::References) {
            Response::Locations(l) => assert!(l.iter().any(|l| l.path.ends_with("b.py")), "{l:?}"),
            r => panic!("{r:?}"),
        }
        match ask(4, 0, 5, LspOp::PrepareRename) {
            Response::RenameRange(Some((s, e, name))) => {
                assert_eq!((s, e, name.as_str()), (at(0, 4), at(0, 9), "greet"))
            }
            r => panic!("{r:?}"),
        }
        match ask(6, 4, 23, LspOp::Hover) {
            Response::Hover(Some(h)) => {
                assert!(h.markdown.contains("greet") && h.markdown.contains("Say hi."), "{h:?}");
                assert_eq!((h.start, h.end), (at(4, 21), at(4, 26)));
            }
            r => panic!("{r:?}"),
        }
        match ask(7, 3, 0, LspOp::Hover) {
            Response::Hover(None) => {}
            r => panic!("{r:?}"),
        }
        match ask(5, 0, 5, LspOp::Rename { new_name: "hello".into() }) {
            Response::Edits(files) => {
                let b = files.iter().find(|f| f.path.ends_with("b.py")).expect("b.py edited");
                let out = apply_edits(&std::fs::read_to_string(dir.join("b.py")).unwrap(), &b.edits);
                assert_eq!(out, "from a import hello\n\nhello(\"q\")\n");
                let a = files.iter().find(|f| f.path.ends_with("a.py")).unwrap();
                assert_eq!(apply_edits(text, &a.edits).matches("hello").count(), 3);
            }
            r => panic!("{r:?}"),
        }
        drop(lsp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A project cannot pick the program nibd starts as its language server: opening a file of a
    /// cloned repository that ships `node_modules/.bin/…` or `.venv/bin/…` must not run them.
    #[cfg(unix)]
    #[test]
    fn a_project_cannot_choose_its_own_language_server() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nibd-lsp-planted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir = {
            std::fs::create_dir_all(&dir).unwrap();
            nib_proto::path::canonical(&dir).unwrap()
        };
        let marks = dir.join("marks");
        std::fs::create_dir_all(&marks).unwrap();
        // What the repository ships: a server under each name nibd might look for there.
        let plant = |at: &str, name: &str| {
            let p = dir.join(at).join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("#!/bin/sh\ntouch '{}/{name}'\n", marks.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        plant("node_modules/.bin", "typescript-language-server");
        plant(".venv/bin", "nib-test-langserver");
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        // A server the user named by its bare name, as a config entry would.
        let user = [LspServer { name: "mine".into(), command: Some(vec!["nib-test-langserver".into()]), extensions: vec!["nibtest".into()], roots: vec![] }];
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut lsp = Lsp::new(dir.clone(), &user, tx);
        lsp.sync(dir.join("a.ts"), 1, "let x = 1;\n".into());
        lsp.sync(dir.join("b.nibtest"), 1, "x\n".into());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let ran: Vec<String> = std::fs::read_dir(&marks).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        drop(lsp);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ran.is_empty(), "the project's own programs ran as language servers: {ran:?}");
    }
}
