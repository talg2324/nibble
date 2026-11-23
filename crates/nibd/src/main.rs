//! `nibd`: speaks the nib protocol on stdin/stdout. stdout is reserved for frames; logs go to stderr.

/// A line on stderr, which the client appends to its per-target log (`~/.cache/nib/<target>.log`,
/// shared by every window on that target): stamped with the UTC time and this nibd's pid, so a
/// line can be matched to a window's `nib.log`.
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("nibd {} [{}]: {}", nib_proto::log_clock(), std::process::id(), format_args!($($arg)*))
    };
}

mod ai;
mod history;
mod mail;
mod attach;
mod diff;
mod fs_ops;
mod git;
mod jobs;
mod lsp;
mod ollama;
mod places;
mod pty;
mod review;
mod search;
mod serve;
mod tokens;
mod tree;
mod watch;
mod web;
mod work;

use nib_proto::{
    recv, send, AiEvent, ClientMsg, ClientNotify, ErrorKind, Place, ReqId, Request, Response, RpcError, ServerMsg,
    ServerNotify, PROTOCOL_VERSION,
};
use nib_proto::timing;
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!(
                "nibd {} (protocol {})",
                env!("CARGO_PKG_VERSION"),
                PROTOCOL_VERSION
            );
            return Ok(());
        }
        // Run by an agent nib started: the agents' mailbox (see `mail`).
        Some("mail") => std::process::exit(mail::cli(&args[1..])),
        // And their branches and plans (see `work`).
        Some("work") => std::process::exit(work::cli(&args[1..])),
        // And their answers to the user's review of their work (see `review`).
        Some("review") => std::process::exit(review::cli(&args[1..])),
        // And the project's apps (see `serve`).
        Some("app") => std::process::exit(serve::cli(&args[1..])),
        // And the page of the project's app in the window's web tab (see `web`).
        Some("web") => std::process::exit(web::cli(&args[1..])),
        None | Some("--stdio") => {}
        Some(other) => anyhow::bail!("unknown argument {other:?}; usage: nibd [--stdio|--version]"),
    }
    {
        use std::io::Write;
        let mut out = io::stdout().lock();
        out.write_all(nib_proto::MAGIC)?;
        out.flush()?;
    }
    log!("start, version {}", env!("CARGO_PKG_VERSION"));
    match serve_stdio(IDLE_LIMIT) {
        Ok(()) => {
            log!("exit");
            Ok(())
        }
        Err(e) => {
            log!("exit: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The client pings every `nib_proto::PING_EVERY`; this long without hearing anything means it is
/// gone (a dropped network, a sleeping laptop) even if sshd hasn't noticed: exit, closing
/// terminals and language servers, instead of lingering for hours.
const IDLE_LIMIT: Duration = Duration::from_secs(120);

/// `serve` on stdin/stdout, ending the session after `idle` without a message from the client.
fn serve_stdio(idle: Duration) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::sync_channel(64);
    thread::spawn(move || {
        let mut input = BufReader::new(io::stdin().lock());
        loop {
            let m = recv::<_, ClientMsg>(&mut input);
            let more = matches!(m, Ok(Some(_)));
            if tx.send(m).is_err() || !more {
                break;
            }
        }
    });
    let next = || match rx.recv_timeout(idle) {
        Ok(m) => Ok(m?),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            log!("nothing from nib for {}s; exiting", idle.as_secs());
            Ok(None)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
    };
    serve_with(next, io::stdout())
}

/// `serve_with` over any byte stream, with no idle limit (tests).
#[cfg(test)]
pub fn serve<R: io::Read, W: io::Write + Send + 'static>(input: R, output: W) -> anyhow::Result<()> {
    let mut input = BufReader::new(input);
    serve_with(move || Ok(recv::<_, ClientMsg>(&mut input)?), output)
}

/// One connection. Requests are handled on the reader thread (all spike ops are fast and local);
/// a dedicated writer thread owns stdout so later async producers (watchers, LSP, PTYs) can push
/// notifications without contending with request handling.
/// `next` yields the client's messages; `None` ends the session.
fn serve_with<W: io::Write + Send + 'static>(
    mut next: impl FnMut() -> anyhow::Result<Option<ClientMsg>>,
    output: W,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<ServerMsg>();
    // Requests in flight: when each arrived and what it was. A request is timed from here until
    // its response is written, wherever it was answered (this thread, a worker, a language server).
    let started: Arc<Mutex<HashMap<ReqId, (Instant, String)>>> = Default::default();
    let started_w = started.clone();
    let writer = thread::spawn(move || -> anyhow::Result<()> {
        let mut out = BufWriter::new(output);
        for msg in rx {
            send(&mut out, &msg)?;
            if let ServerMsg::Response { id, .. } = &msg {
                if let Some((t, name)) = started_w.lock().unwrap().remove(id) {
                    let d = t.elapsed();
                    if let Some(kind) = timing::verdict(d, timing::SLOW_RPC) {
                        log!("{kind}  {}", timing::line("req", &name, d));
                    }
                }
            }
        }
        Ok(())
    });

    let mut state =
        State { root: None, places: None, ptys: Default::default(), serve: None, web: None, lsp: None, ai: Default::default(), watch: watch::Watcher::new(tx.clone()), tx: tx.clone(), history: worker() };
    while let Some(msg) = next()? {
        let t0 = Instant::now();
        // Requests are timed by the writer; a notification is done when this loop turns.
        let notify = match &msg {
            ClientMsg::Request { id, req } => {
                started.lock().unwrap().insert(*id, (t0, timing::label(req)));
                None
            }
            ClientMsg::Notify(n) => Some(n),
        }
        .map(timing::label);
        match msg {
            ClientMsg::Request { id, req } => {
                let shutdown = matches!(req, Request::Shutdown);
                // `None`: answered later (language server queries).
                if let Some(result) = state.handle(id, req) {
                    if tx.send(ServerMsg::Response { id, result }).is_err() {
                        break;
                    }
                }
                if shutdown {
                    break;
                }
            }
            ClientMsg::Notify(ClientNotify::Ping { seq }) => {
                let _ = tx.send(ServerMsg::Notify(ServerNotify::Pong { seq }));
            }
            ClientMsg::Notify(ClientNotify::PtyInput { id, data }) => state.ptys.lock().unwrap().input(id, &data),
            ClientMsg::Notify(ClientNotify::PtyResize { id, cols, rows }) => state.ptys.lock().unwrap().resize(id, cols, rows),
            ClientMsg::Notify(ClientNotify::PtyClose { id }) => {
                state.ptys.lock().unwrap().close(id);
                // A server's terminal: the server is stopped.
                if let Some(serve) = &state.serve {
                    serve.lock().unwrap().closed(id);
                }
            }
            ClientMsg::Notify(ClientNotify::WebReply { id, reply }) => {
                if let Some(web) = &state.web {
                    web.lock().unwrap().reply(id, &reply);
                }
            }
            ClientMsg::Notify(ClientNotify::DocSync { path, version, text }) => {
                if let Some(lsp) = &mut state.lsp {
                    let path = resolve(state.root.as_ref(), &path);
                    lsp.sync(path, version, text);
                }
            }
            ClientMsg::Notify(ClientNotify::AiSend { id, text, attach, images }) => {
                let text = match &state.root {
                    Some(root) if !attach.is_empty() => attach::expand(root, &text, &attach),
                    _ => text,
                };
                ai::send(&state.ai, id, &text, &images)
            }
            ClientMsg::Notify(ClientNotify::AiCompact { id, focus }) => ai::compact(&state.ai, id, focus.as_deref()),
            ClientMsg::Notify(ClientNotify::AiClose { id }) => ai::close(&state.ai, id),
            ClientMsg::Notify(ClientNotify::AiInterrupt { id }) => ai::interrupt(&state.ai, id),
            ClientMsg::Notify(ClientNotify::AiSetMode { id, mode }) => ai::set_mode(&state.ai, id, mode),
            ClientMsg::Notify(ClientNotify::AiWorktreePerFeature { on }) => ai::set_worktree_per_feature(&state.ai, on),
            ClientMsg::Notify(ClientNotify::AiAnswer { id, request, allow, message, answers }) => ai::answer(&state.ai, id, &request, allow, message.as_deref(), &answers),
            ClientMsg::Notify(ClientNotify::AiSuspend { id, on }) => {
                let error = ai::suspend(&state.ai, id, on).err();
                let _ = state.tx.send(ServerMsg::Notify(ServerNotify::AiEvent { id, event: AiEvent::Suspended { on, error } }));
            }
            ClientMsg::Notify(ClientNotify::Watch { paths }) => {
                let paths = paths.into_iter().map(|p| { let r = resolve(state.root.as_ref(), &p); (p, r) }).collect();
                state.watch.set(paths);
            }
            ClientMsg::Notify(ClientNotify::DocClose { path }) => {
                if let Some(lsp) = &mut state.lsp {
                    lsp.close(resolve(state.root.as_ref(), &path));
                }
            }
        }
        if let Some(name) = notify {
            let d = t0.elapsed();
            if let Some(kind) = timing::verdict(d, timing::SLOW_RPC) {
                log!("{kind}  {}", timing::line("notify", &name, d));
            }
        }
    }
    // Terminals and servers end here, whoever else still holds them for a moment.
    state.ptys.lock().unwrap().close_all();
    drop(state); // releases its sender clone
    drop(tx);
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
    Ok(())
}

/// A thread running the jobs sent to it one after another; it ends with the sender.
fn worker() -> mpsc::Sender<Box<dyn FnOnce() + Send>> {
    let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
    thread::spawn(move || rx.into_iter().for_each(|job| job()));
    tx
}

/// The requests the history worker answers.
fn history_request(root: &std::path::Path, req: Request) -> Result<Response, RpcError> {
    match req {
        Request::AiSkills { agent } => Ok(Response::AiSkills(ai::skills(agent, Some(root)))),
        Request::AiHistory { query } => Ok(Response::AiHistory(history::list(root, &query))),
        Request::AiMailHistory { query } => Ok(Response::AiMailHistory(history::letters(root, &query))),
        Request::AiHistoryLoad { id } => match history::load(root, &id) {
            Some((past, entries)) => Ok(Response::AiPastLoaded { past, entries }),
            None => Err(RpcError::new(ErrorKind::NotFound, "no such conversation")),
        },
        Request::AiHistoryDelete { id } => history::delete(root, &id).map(|()| Response::Ok).map_err(|e| RpcError::new(ErrorKind::Io, format!("deleting the conversation: {e}"))),
        _ => unreachable!("not a history request"),
    }
}

struct State {
    root: Option<PathBuf>,
    /// Scratch and NIBDIR, made by Hello (None if they couldn't be).
    places: Option<places::Places>,
    /// Shared with the project's servers, which open their own.
    ptys: Arc<Mutex<pty::Ptys>>,
    /// The project's servers; created by Hello (they need the root and NIBDIR).
    serve: Option<serve::Shared>,
    /// Agents' requests for the window's web tab, passed on; created by Hello.
    web: Option<web::Shared>,
    /// Created by Hello (it needs the root).
    lsp: Option<lsp::Lsp>,
    ai: ai::Shared,
    /// The client's open files, polled for changes made outside nib.
    watch: watch::Watcher,
    tx: mpsc::Sender<ServerMsg>,
    /// One worker for the requests that read conversation files (up to megabytes each): off this
    /// thread so a ping or a keystroke for a terminal never waits behind them, and in the order
    /// they came.
    history: mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

/// `path` relative to the root, or absolute.
fn resolve(root: Option<&PathBuf>, path: &str) -> PathBuf {
    let p = fs_ops::expand_home(path);
    match root {
        Some(r) if !p.is_absolute() => r.join(p),
        _ => p,
    }
}

impl State {
    /// `None` when the answer is sent later.
    fn handle(&mut self, id: ReqId, req: Request) -> Option<Result<Response, RpcError>> {
        if let Request::Lsp { path, pos, op } = req {
            let path = self.resolve(&path);
            let Some(lsp) = &mut self.lsp else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            return lsp.request(id, path, pos, op).err().map(Err);
        }
        if let Request::AiOpen { replace, agent, model, tools, mode, resume, effort, prompt } = req {
            let Some(root) = self.root.clone() else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            // Asks the assistant whether it is signed in first, which takes a moment.
            let (ai, tx) = (self.ai.clone(), self.tx.clone());
            let (scratch, nibdir) = (self.places.as_ref().map(|p| p.scratch.clone()), self.places.as_ref().map(|p| p.nibdir.clone()));
            thread::spawn(move || {
                let result = ai::open(&ai, replace, agent, model, ai::Tuning { effort, prompt }, tools, mode, resume.as_deref(), &root, ai::Dirs { scratch: scratch.as_deref(), nibdir: nibdir.as_deref() }, tx.clone()).map(|id| Response::AiOpened { id });
                let _ = tx.send(ServerMsg::Response { id, result });
            });
            return None;
        }
        if let Request::AiWarm { model, warm, gpu } = req {
            let Some(root) = self.root.clone() else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            // Loading a model and priming it can take minutes: off this thread.
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result = ollama::GpuSel::parse(gpu.as_deref())
                    .and_then(|sel| ai::set_warm(&model, warm, &sel, &root))
                    .map(|_| Response::Ok)
                    .map_err(|e| RpcError::new(ErrorKind::Io, e));
                let _ = tx.send(ServerMsg::Response { id, result });
            });
            return None;
        }
        if let Request::AiJob { job } = req {
            let Some(root) = self.root.clone() else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result = jobs::run_job(&job, &root).to_string();
                let _ = tx.send(ServerMsg::Response { id, result: Ok(Response::AiJobDone { result }) });
            });
            return None;
        }
        if let Request::AiBatch { jobs: file, out, concurrency, retries } = req {
            let Some(root) = self.root.clone() else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            let tx = self.tx.clone();
            thread::spawn(move || {
                let progress_tx = tx.clone();
                let progress = move |line: &serde_json::Value| {
                    let _ = progress_tx.send(ServerMsg::Notify(ServerNotify::AiProgress { req: id, line: line.to_string() }));
                };
                let result = jobs::run_batch(&file, &out, concurrency, retries, &root, progress)
                    .map(|s| Response::AiBatchDone { summary: s.to_string() })
                    .map_err(|e| RpcError::new(ErrorKind::BadRequest, e));
                let _ = tx.send(ServerMsg::Response { id, result });
            });
            return None;
        }
        if matches!(req, Request::AiInstall { .. } | Request::AiPull { .. } | Request::AiDelete { .. }) {
            // Installing and downloading take minutes: off this thread, with progress lines.
            let tx = self.tx.clone();
            thread::spawn(move || {
                let progress_tx = tx.clone();
                let progress = move |line: serde_json::Value| {
                    let _ = progress_tx.send(ServerMsg::Notify(ServerNotify::AiProgress { req: id, line: line.to_string() }));
                };
                let result = match req {
                    Request::AiInstall { agent } => ai::install(agent, |l| progress(serde_json::json!({ "line": l }))),
                    Request::AiPull { model } => ollama::pull(&model, |v| progress(v.clone())),
                    Request::AiDelete { model } => ollama::delete(&model),
                    _ => unreachable!(),
                };
                let result = result.map(|_| Response::Ok).map_err(|e| RpcError::new(ErrorKind::Io, e));
                let _ = tx.send(ServerMsg::Response { id, result });
            });
            return None;
        }
        if let Request::AiAgents = req {
            let (root, tx) = (self.root.clone(), self.tx.clone());
            thread::spawn(move || {
                let _ = tx.send(ServerMsg::Response { id, result: Ok(Response::AiAgents(ai::agents(root.as_deref()))) });
            });
            return None;
        }
        if let Request::Search { query, max } = req {
            let Some(root) = self.root.clone() else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            // Reads the whole project: off the reader thread.
            let tx = self.tx.clone();
            thread::spawn(move || {
                let found = search::search(&root, &query, max as usize);
                let _ = tx.send(ServerMsg::Response { id, result: Ok(Response::Search(found)) });
            });
            return None;
        }
        if matches!(req, Request::GitInfo | Request::GitDetails { .. } | Request::GitRun { .. } | Request::Diff { .. } | Request::WorkLand { .. } | Request::Review { .. } | Request::ReviewEdit { .. } | Request::Tree { .. }) {
            let Some(root) = self.root.clone() else {
                return Some(Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")));
            };
            let tree_root = match &req {
                Request::Tree { place, .. } => match self.place_dir(*place) {
                    Ok(d) => d,
                    Err(e) => return Some(Err(e)),
                },
                _ => root.clone(),
            };
            // `git` runs (a pull can take a while) or the project is walked: off the reader thread,
            // so input to terminals is never held up.
            let tx = self.tx.clone();
            let ai = self.ai.clone();
            // Whose work an agents' diff shows, as it stands now.
            let targets = match &req {
                Request::Diff { scope: nib_proto::DiffScope::Agent(id), .. } => ai::diff_target(&self.ai, *id).map(|t| vec![(*id, t)]).unwrap_or_default(),
                Request::Diff { scope: nib_proto::DiffScope::Everyone, .. } => ai::diff_targets(&self.ai),
                Request::Diff { scope: nib_proto::DiffScope::Since(id), .. } => ai::diff_since(&self.ai, *id).map(|t| vec![(*id, t)]).unwrap_or_default(),
                _ => Vec::new(),
            };
            thread::spawn(move || {
                let resp = match req {
                    Request::Tree { known, place, open } => {
                        Response::Tree(tree::snapshot(&tree_root, known, place == Place::Project, &open))
                    }
                    Request::GitDetails { log_limit } => {
                        let (commits, changes) = git::details(&root, log_limit);
                        Response::GitDetails { commits, changes }
                    }
                    Request::Diff { scope, known } => Response::Diff(diff::diff(&root, scope, known, &targets)),
                    Request::Review { id } => Response::Review(ai::review(&ai, id)),
                    Request::ReviewEdit { id: agent, op } => match ai::review_edit(&ai, agent, op) {
                        Ok((review, Some(message))) => Response::ReviewSent { review, message },
                        Ok((review, None)) => Response::Review(review),
                        Err(e) => {
                            let _ = tx.send(ServerMsg::Response { id, result: Err(RpcError::new(ErrorKind::BadRequest, e)) });
                            return;
                        }
                    },
                    Request::WorkLand { id, message } => {
                        let (ok, output) = ai::land(&ai, id, &root, &message);
                        Response::GitRan { ok, output }
                    }
                    Request::GitRun { op } => {
                        let (ok, output) = git::run(&root, &op);
                        Response::GitRan { ok, output }
                    }
                    _ => Response::Git(git::info(&root)),
                };
                let _ = tx.send(ServerMsg::Response { id, result: Ok(resp) });
            });
            return None;
        }
        if matches!(req, Request::AiSkills { .. } | Request::AiHistory { .. } | Request::AiMailHistory { .. } | Request::AiHistoryLoad { .. } | Request::AiHistoryDelete { .. }) {
            if let Some(root) = self.root.clone() {
                let tx = self.tx.clone();
                let job = Box::new(move || {
                    let _ = tx.send(ServerMsg::Response { id, result: history_request(&root, req) });
                });
                if self.history.send(job).is_ok() {
                    return None;
                }
                return Some(Err(RpcError::new(ErrorKind::Io, "history worker gone")));
            }
        }
        Some(self.handle_now(req))
    }

    fn handle_now(&mut self, req: Request) -> Result<Response, RpcError> {
        if self.root.is_none() && !matches!(req, Request::Hello { .. } | Request::Shutdown) {
            return Err(RpcError::new(
                ErrorKind::BadRequest,
                "Hello must be the first request",
            ));
        }
        match req {
            Request::Hello { protocol, root, lsp, .. } => {
                if protocol != PROTOCOL_VERSION {
                    return Err(RpcError::new(
                        ErrorKind::ProtocolMismatch,
                        format!("client protocol {protocol}, daemon {PROTOCOL_VERSION}"),
                    ));
                }
                let root = fs_ops::expand_home(&root);
                let root = nib_proto::path::canonical(&root).map_err(|e| {
                    RpcError::new(
                        ErrorKind::NotFound,
                        format!("project root {}: {e}", root.display()),
                    )
                })?;
                // Not fatal: the editor works without them; the tree just doesn't offer them.
                self.places = places::setup(&root)
                    .inspect_err(|e| log!("scratch / NIBDIR: {e}"))
                    .ok();
                let lsp = lsp::Lsp::new(root.clone(), &lsp, self.tx.clone());
                let path_of = |p: Option<&PathBuf>| p.map(|p| nib_proto::path::to_wire(p)).unwrap_or_default();
                let resp = Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").into(),
                    root: nib_proto::path::to_wire(&root),
                    scratch: path_of(self.places.as_ref().map(|p| &p.scratch)),
                    nibdir: path_of(self.places.as_ref().map(|p| &p.nibdir)),
                    hostname: fs_ops::hostname(),
                    os: std::env::consts::OS.into(),
                    arch: std::env::consts::ARCH.into(),
                    lsp_extensions: lsp.extensions(),
                };
                self.lsp = Some(lsp);
                let (nibdir, scratch) = (self.places.as_ref().map(|p| p.nibdir.as_path()), self.places.as_ref().map(|p| p.scratch.as_path()));
                self.serve = Some(serve::new(&root, nibdir, scratch, &mail::path(scratch), self.ptys.clone(), self.tx.clone()));
                self.web = Some(web::new(&mail::path(scratch), self.tx.clone()));
                self.root = Some(root);
                Ok(resp)
            }
            Request::ReadFile { path } => fs_ops::read_file(&self.resolve(&path)),
            Request::WriteFile {
                path,
                data,
                expected_mtime_ns,
            } => {
                let path = self.resolve(&path);
                let r = fs_ops::write_file(&path, &data, expected_mtime_ns)?;
                self.watch.wrote(&path);
                self.files_changed(&[path]);
                Ok(r)
            }
            Request::ApplyEdits { files } => {
                let mut written = Vec::new();
                let mut result = Ok(Response::Ok);
                for f in files {
                    let path = self.resolve(&f.path);
                    let r = std::fs::read_to_string(&path).map_err(RpcError::from).and_then(|text| {
                        let out = lsp::apply_edits(&text, &f.edits);
                        fs_ops::write_file(&path, out.as_bytes(), None)
                    });
                    match r {
                        Ok(_) => written.push(path),
                        Err(e) => {
                            result = Err(RpcError::new(e.kind, format!("{}: {}", path.display(), e.msg)));
                            break;
                        }
                    }
                }
                self.files_changed(&written);
                result
            }
            Request::Lsp { .. }
            | Request::GitInfo
            | Request::GitDetails { .. }
            | Request::GitRun { .. }
            | Request::Diff { .. }
            | Request::WorkLand { .. }
            | Request::Review { .. }
            | Request::ReviewEdit { .. }
            | Request::Tree { .. }
            | Request::Search { .. }
            | Request::AiOpen { .. }
            | Request::AiWarm { .. }
            | Request::AiJob { .. }
            | Request::AiBatch { .. }
            | Request::AiInstall { .. }
            | Request::AiPull { .. }
            | Request::AiDelete { .. }
            | Request::AiAgents => {
                unreachable!("handled in State::handle")
            }
            // In order, on this thread: a rename followed by a delete must not race.
            Request::Fs { op, place } => fs_ops::fs_op(&self.place_dir(place)?, &op),
            Request::ListDir { path } => fs_ops::list_dir(&self.resolve(&path)),
            Request::WorkPlan { id } => Ok(Response::WorkPlan(ai::plan(&self.ai, id))),
            Request::AiSkills { agent } => Ok(Response::AiSkills(ai::skills(agent, self.root.as_deref()))),
            Request::AiHistory { query } => Ok(Response::AiHistory(self.root.as_deref().map(|r| history::list(r, &query)).unwrap_or_default())),
            Request::AiMailHistory { query } => Ok(Response::AiMailHistory(self.root.as_deref().map(|r| history::letters(r, &query)).unwrap_or_default())),
            Request::AiHistoryLoad { id } => match self.root.as_deref().and_then(|r| history::load(r, &id)) {
                Some((past, entries)) => Ok(Response::AiPastLoaded { past, entries }),
                None => Err(RpcError::new(ErrorKind::NotFound, "no such conversation")),
            },
            Request::AiHistoryDelete { id } => match self.root.as_deref().map(|r| history::delete(r, &id)) {
                Some(Ok(())) => Ok(Response::Ok),
                Some(Err(e)) => Err(RpcError::new(ErrorKind::Io, format!("deleting the conversation: {e}"))),
                None => Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")),
            },
            Request::PtyOpen { cwd, cmd, cols, rows } => {
                let cwd = self.resolve(&cwd);
                let env: Vec<(&str, &std::path::Path)> = match &self.places {
                    Some(p) => vec![("NIB_SCRATCH", &p.scratch), ("NIBDIR", &p.nibdir)],
                    None => Vec::new(),
                };
                let id = self.ptys.lock().unwrap().open(&cwd, cmd, cols, rows, &env, self.tx.clone())?;
                Ok(Response::PtyOpened { id })
            }
            Request::AiSetup { agent, cols, rows, login } => {
                let (cmd, path) = ai::setup_cmd(agent, login)?;
                let root = self.root.clone().expect("checked above");
                let env = [("PATH", std::path::Path::new(&path))];
                let id = self.ptys.lock().unwrap().open(&root, Some(cmd), cols, rows, &env, self.tx.clone())?;
                Ok(Response::PtyOpened { id })
            }
            Request::AiUpdate { agent, cols, rows } => {
                let (cmd, path) = ai::update_cmd(agent)?;
                let root = self.root.clone().expect("checked above");
                let env = [("PATH", std::path::Path::new(&path))];
                let id = self.ptys.lock().unwrap().open(&root, Some(cmd), cols, rows, &env, self.tx.clone())?;
                Ok(Response::PtyOpened { id })
            }
            Request::App { op, user } => match &self.serve {
                Some(serve) => serve.lock().unwrap().request(op, user).map(Response::Apps),
                None => Err(RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")),
            },
            Request::Shutdown => Ok(Response::Ok),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        resolve(self.root.as_ref(), path)
    }

    /// The directory a `Place` names (Hello has run).
    fn place_dir(&self, place: Place) -> Result<PathBuf, RpcError> {
        let missing = || RpcError::new(ErrorKind::NotFound, "no scratch / NIBDIR on this machine (see the daemon log)");
        match place {
            Place::Project => self.root.clone().ok_or_else(|| RpcError::new(ErrorKind::BadRequest, "Hello must be the first request")),
            Place::Scratch => self.places.as_ref().map(|p| p.scratch.clone()).ok_or_else(missing),
            Place::Nibdir => self.places.as_ref().map(|p| p.nibdir.clone()).ok_or_else(missing),
        }
    }

    fn files_changed(&mut self, paths: &[PathBuf]) {
        if let Some(lsp) = &mut self.lsp {
            lsp.files_changed(paths);
        }
    }
}

#[cfg(test)]
mod tests;
