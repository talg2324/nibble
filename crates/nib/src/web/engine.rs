//! The thread that owns this window's browser and answers every `web.*` request.
//!
//! One browser per window, one of its tabs per web tab (D-071). Everything the browser says
//! arrives on the same queue as the requests, so the thread never sleeps: while it waits for an
//! answer it keeps handling events, which matters because the browser holds every request a
//! page makes until this thread has judged it (the local-only rule, `local.rs`).
//!
//! Two kinds of browser are driven, each in its own file: Firefox (`bidi.rs`) and Chromium
//! (`cdp.rs`). What they must each be able to do is the list of `match self.kind()` at the end
//! of this file; everything else here is the same for both.

use super::browser::{Kind, Process};
use super::page::{self, key, Stroke};
use super::{local, ws};
use crate::control::Call;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long one command to the browser may take.
pub(super) const ANSWER_WITHIN: Duration = Duration::from_secs(20);
/// Console lines and failed requests kept per page between two reads.
const KEPT: usize = 100;
/// Text of a page that `read` gives unless asked for all of it.
const TEXT: usize = 8000;
/// After typing into a field, how long before the next request is served: pages commonly act
/// on a typed value only after a pause (Swagger UI waits 350 ms), and a click that comes sooner
/// is a click on stale state.
const TYPED_SETTLE: Duration = Duration::from_millis(400);
/// The page size when no web tab is showing it (`nib --headless`).
pub(super) const UNSEEN: (u32, u32) = (1280, 800);

pub enum Job {
    Call(Call),
    /// A message from browser number `0` (a restarted browser is a new number).
    Wire(u64, Value),
    /// Browser number `0` hung up.
    Closed(u64),
    /// Stop the browser and end the thread; the sender hears when it is done.
    Stop(mpsc::Sender<()>),
    /// From the window's web tab (`tab.rs`).
    Tab(super::TabJob),
}

/// A document inside a page: the page itself or a frame in it. `ctx` is what commands about it
/// are addressed to (Firefox: the frame's own context; Chromium: the tab's session), `frame`
/// Chromium's name for the frame within that.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Frame {
    pub ctx: String,
    pub frame: Option<String>,
}

/// An argument for a function run in a page.
pub(super) enum Arg {
    Num(u64),
    Str(String),
}

/// What a function run in a page gave back: a plain value, or an element (the browser's handle
/// on it, to click it or picture it).
pub(super) struct Ret {
    pub value: Value,
    pub node: Option<Value>,
}

/// One web tab's page.
pub(super) struct Page {
    pub id: u32,
    /// Its tab in the browser: Firefox's context, Chromium's session.
    pub ctx: String,
    /// Chromium's names for the tab and for its own document.
    pub target: String,
    pub main_frame: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    /// The document being loaded can be read (set by the browser's events; `navigate` waits on it).
    pub dom: bool,
    pub console: Vec<Value>,
    pub failed: Vec<Value>,
    /// The last address outside this machine that this page tried to show, and how many it
    /// has tried in all.
    pub blocked: Option<String>,
    pub blocks: u64,
    /// An `alert` / `confirm` / `prompt` waiting for its answer.
    pub dialog: Option<Value>,
    /// The frames of the last `read`: where, the number of its first element, how many.
    frames: Vec<(Frame, u64, u64)>,
    pub downloads: Vec<Value>,
    /// A list of choices nib shows for a `<select>` the user pressed in the tab.
    pub choice: Option<super::Choice>,
    /// The shape the pointer should have where it is over the page.
    pub cursor: String,
    /// Files this page saved, not yet said to the user.
    pub saved: Vec<String>,
    /// Its zoom (1: 100%); see `super::TabInfo::zoom`.
    pub zoom: f32,
}

/// What an event asks for that needs a full command (events themselves only send, never wait).
pub(super) enum Later {
    /// A page opened a new window on a local address: it becomes a page of ours.
    Open(String),
}

/// A window a page opened (`window.open`, `target=_blank`): never shown, closed once its
/// address is known.
pub(super) struct Popup {
    pub opener: Option<u32>,
    /// Chromium's name for it, to close it by.
    pub target: String,
}

/// A running browser and what nib knows of its pages.
pub(super) struct Browser {
    pub proc: Process,
    out: ws::Sender,
    pub version: String,
    next: u64,
    pub pages: Vec<Page>,
    next_page: u32,
    pub current: Option<u32>,
    /// Every context or session (tabs and their frames) and the page it belongs to.
    pub owner: HashMap<String, u32>,
    pub popups: HashMap<String, Popup>,
    /// The tab the browser started with (see `Engine::new_page`).
    pub first: Option<String>,
    /// nib asked for a tab and has not heard its name yet: the next one to appear is ours.
    pub making: bool,
    /// Chromium: the tab that appeared while `making`, as (session, target).
    pub made: Option<(String, String)>,
    pub later: Vec<Later>,
    /// Commands the local-only rule depends on: if the browser refuses one, it is stopped.
    pub critical: HashSet<u64>,
    /// Chromium: addresses of requests under way, to name the ones that fail.
    pub urls: HashMap<String, String>,
}

pub struct Engine {
    rx: mpsc::Receiver<Job>,
    tx: mpsc::Sender<Job>,
    backlog: VecDeque<Job>,
    /// Where profiles are kept, and the project the window shows.
    base: PathBuf,
    root: String,
    pub(super) browser: Option<Browser>,
    generation: u64,
    pub(super) tab: super::tab::TabState,
    /// What the window was last told about the tabs, and how many times a page was asked to
    /// be shown (`web open` without `background`).
    told: Vec<super::TabInfo>,
    told_shown: u64,
    shown: u64,
}

/// The local-only rule applied to one request the browser is about to send. `document`: it is
/// for a page or a frame (`Some(true)`), for something a document uses (`Some(false)`), or the
/// browser's word on it could not be read (`None`).
///
/// Documents must be of this machine, or made by the page itself (`about:blank`,
/// `about:srcdoc`, `data:`, a `blob:` of a local page). Everything else a page loads is its
/// own business. What cannot be read is refused.
pub(super) fn allowed(url: &str, document: Option<bool>) -> bool {
    match document {
        Some(false) => true,
        Some(true) => local::is_local(url) || url == "about:blank" || url == "about:srcdoc" || url.starts_with("data:") || url.strip_prefix("blob:").is_some_and(local::is_local),
        None => false,
    }
}

pub(super) fn keep(log: &mut Vec<Value>, v: Value) {
    if log.len() >= KEPT {
        log.remove(0);
    }
    log.push(v);
}

impl Browser {
    /// Send a command without waiting for its answer. `session`: Chromium's, for commands
    /// about one tab.
    pub fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> u64 {
        self.next += 1;
        let mut m = json!({ "id": self.next, "method": method, "params": params });
        if let Some(s) = session {
            m["sessionId"] = json!(s);
        }
        let _ = self.out.text(&m.to_string());
        self.next
    }

    /// Send a command the local-only rule cannot do without.
    pub fn send_critical(&mut self, session: Option<&str>, method: &str, params: Value) {
        let id = self.send(session, method, params);
        self.critical.insert(id);
    }

    pub fn page(&mut self, id: u32) -> Option<&mut Page> {
        self.pages.iter_mut().find(|p| p.id == id)
    }

    /// The page a context or session (a tab or one of its frames) belongs to.
    pub fn page_of(&mut self, ctx: &str) -> Option<&mut Page> {
        let id = *self.owner.get(ctx)?;
        self.page(id)
    }

    /// The page whose own tab `ctx` is (not a frame in it).
    pub fn top_page(&mut self, ctx: &str) -> Option<&mut Page> {
        self.pages.iter_mut().find(|p| p.ctx == ctx)
    }

    /// A request was refused: say so on the page it was for, when it was for the page itself.
    pub fn refused(&mut self, ctx: &str, url: String, top: bool) {
        crate::control::log("web", &format!("refused {url}"));
        if let Some(page) = self.page_of(ctx) {
            page.blocks += 1;
            if top {
                page.blocked = Some(url);
                page.loading = false;
            }
        }
    }

    /// A window a page opened is about to load `url`: it is never shown. A local address
    /// becomes a page of ours; any other is reported on the page that opened it.
    pub fn popup_wants(&mut self, opener: Option<u32>, url: String) {
        if local::is_local(&url) {
            self.later.push(Later::Open(url));
        } else if let Some(page) = opener.and_then(|o| self.page(o)) {
            crate::control::log("web", &format!("refused {url} (a new window)"));
            page.blocked = Some(url);
            page.blocks += 1;
        }
    }

    /// A tab is gone (closed by its page, or with a crash).
    pub fn forget(&mut self, ctx: &str) {
        self.owner.remove(ctx);
        self.popups.remove(ctx);
        if let Some(at) = self.pages.iter().position(|pg| pg.ctx == ctx) {
            let gone = self.pages.remove(at).id;
            self.owner.retain(|_, id| *id != gone);
            if self.current == Some(gone) {
                self.current = self.pages.last().map(|pg| pg.id);
            }
        }
    }

    fn event(&mut self, m: &Value) {
        match self.proc.found.kind {
            Kind::Firefox => self.bidi_event(m),
            Kind::Chromium => self.cdp_event(m),
        }
    }
}

/// The width and height a PNG says it has.
fn png_size(png: &[u8]) -> (u32, u32) {
    match png.get(16..24) {
        Some(b) => (u32::from_be_bytes([b[0], b[1], b[2], b[3]]), u32::from_be_bytes([b[4], b[5], b[6], b[7]])),
        None => (0, 0),
    }
}

fn number(n: u64) -> Arg {
    Arg::Num(n)
}

fn string(s: &str) -> Arg {
    Arg::Str(s.to_owned())
}

/// Whether a message from the browser is an event (not the answer to a command).
fn is_event(m: &Value) -> bool {
    m["type"] == "event" || m.get("id").is_none_or(Value::is_null)
}

impl Engine {
    pub fn new(rx: mpsc::Receiver<Job>, tx: mpsc::Sender<Job>, base: PathBuf, root: String) -> Self {
        Self { rx, tx, backlog: VecDeque::new(), base, root, browser: None, generation: 0, tab: super::tab::TabState::new(), told: Vec::new(), told_shown: 0, shown: 0 }
    }

    pub fn run(mut self) {
        loop {
            let job = match self.backlog.pop_front() {
                Some(j) => j,
                None => match self.rx.recv_timeout(self.tab.due_in().unwrap_or(Duration::from_millis(500)).min(Duration::from_millis(500))) {
                    Ok(j) => j,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if self.browser.as_mut().is_some_and(|f| f.proc.gone()) {
                            self.lost();
                        }
                        self.tab_picture();
                        self.publish();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
            };
            match job {
                Job::Call(call) => {
                    let started = Instant::now();
                    match self.serve(&call) {
                        Ok(v) => call.ok(v),
                        Err(e) => call.err(format!("{e:#}")),
                    }
                    crate::control::log("web", &format!("{} {} ms", call.cmd(), started.elapsed().as_millis()));
                }
                Job::Wire(g, m) if g == self.generation => {
                    if is_event(&m) {
                        if let Some(f) = self.browser.as_mut() {
                            f.event(&m);
                        }
                    } else {
                        self.refused_critical(&m);
                    }
                }
                Job::Closed(g) if g == self.generation => self.lost(),
                Job::Wire(..) | Job::Closed(_) => {}
                Job::Tab(job) => {
                    // A run of mouse moves is worth only its last.
                    let moves = |j: &Job| matches!(j, Job::Tab(super::TabJob::Input { input: super::Input::Move { .. }, .. }));
                    let mut job = Job::Tab(job);
                    while moves(&job) && self.backlog.front().is_some_and(moves) {
                        job = self.backlog.pop_front().unwrap_or(job);
                    }
                    if moves(&job) {
                        while let Ok(next) = self.rx.try_recv() {
                            if moves(&next) {
                                job = next;
                            } else {
                                self.backlog.push_back(next);
                                break;
                            }
                        }
                    }
                    if let Job::Tab(job) = job {
                        self.tab_job(job);
                    }
                }
                Job::Stop(done) => {
                    self.quit();
                    self.publish();
                    let _ = done.send(());
                    break;
                }
            }
            self.do_later();
            self.tab_picture();
            self.publish();
        }
    }

    /// The browser is gone (it crashed, or was killed): forget it. The next `open` starts another.
    fn lost(&mut self) {
        if let Some(f) = self.browser.take() {
            f.out.close();
        }
        self.generation += 1;
    }

    pub(super) fn quit(&mut self) {
        if self.browser.is_some() {
            self.b_quit();
        }
        self.lost();
    }

    fn do_later(&mut self) {
        while let Some(l) = self.browser.as_mut().and_then(|f| f.later.pop()) {
            match l {
                Later::Open(url) => {
                    let _ = self.open(&url);
                }
            }
        }
    }

    fn publish(&mut self) {
        // The window's own copy, and a nudge when it changed (a tab appeared, was renamed, …).
        let tabs: Vec<super::TabInfo> = self.browser.iter_mut().flat_map(|b| b.pages.iter_mut()).map(|p| super::TabInfo {
            id: p.id, name: page::tab_name(&p.title, &p.url), address: page::short_address(&p.url), url: p.url.clone(), loading: p.loading,
            blocked: p.blocked.clone(), dialog: p.dialog.clone(), choice: p.choice.clone(), cursor: p.cursor.clone(), saved: std::mem::take(&mut p.saved),
            zoom: p.zoom,
        }).collect();
        let plain: Vec<super::TabInfo> = tabs.iter().cloned().map(|mut t| { t.saved.clear(); t }).collect();
        if plain != self.told || self.shown != self.told_shown || tabs.iter().any(|t| !t.saved.is_empty()) {
            self.told = plain;
            self.told_shown = self.shown;
            super::tell(tabs, self.browser.as_ref().and_then(|b| b.current), self.shown);
        }
        let state = match &self.browser {
            Some(f) => json!({
                "browser": super::browser::describe(), "running": true, "version": f.version,
                "current": f.current.and_then(|id| f.pages.iter().position(|p| p.id == id)),
                "tabs": f.pages.iter().enumerate().map(|(i, p)| json!({
                    "index": i, "id": p.id, "name": page::tab_name(&p.title, &p.url), "url": p.url,
                    "address": page::short_address(&p.url), "loading": p.loading,
                    "console": p.console.len(), "failed": p.failed.len(),
                    "blocked": p.blocked, "dialog": p.dialog, "downloads": p.downloads,
                })).collect::<Vec<_>>(),
            }),
            None => json!({ "browser": super::browser::describe(), "running": false, "tabs": [] }),
        };
        super::publish(state);
    }

    // ---- talking to Firefox ----

    /// Handle one thing from the queue while waiting for something else: Firefox's events are
    /// handled, its answers to `want` returned, everything else kept for later.
    fn take(&mut self, wait: Duration, want: Option<u64>) -> Result<Option<Value>> {
        match self.rx.recv_timeout(wait) {
            Ok(Job::Wire(g, m)) if g == self.generation => {
                if is_event(&m) {
                    if let Some(f) = self.browser.as_mut() {
                        f.event(&m);
                    }
                } else if want.is_some() && m["id"].as_u64() == want {
                    return Ok(Some(m));
                } else if self.refused_critical(&m) {
                    bail!("the browser would not hold requests for nib, so it was stopped");
                }
            }
            Ok(Job::Closed(g)) if g == self.generation => {
                self.lost();
                bail!("the browser ended");
            }
            Ok(Job::Wire(..) | Job::Closed(_)) => {}
            Ok(other) => self.backlog.push_back(other),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("nib is closing"),
        }
        Ok(None)
    }

    /// An answer nobody waits for says that a command the local-only rule needs was refused:
    /// pages could load unjudged, so there is no browser any more.
    fn refused_critical(&mut self, m: &Value) -> bool {
        let failed = m["type"] == "error" || m.get("error").is_some_and(|e| !e.is_null());
        let Some(b) = self.browser.as_mut() else { return false };
        let critical = m["id"].as_u64().is_some_and(|id| b.critical.remove(&id));
        if critical && failed {
            crate::control::log("web", &format!("stopped the browser: it refused {m}"));
            self.lost();
            return true;
        }
        false
    }

    /// Let `time` pass, still answering the browser.
    fn pump(&mut self, time: Duration) -> Result<()> {
        let until = Instant::now() + time;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            self.take(left, None)?;
        }
        Ok(())
    }

    /// One command to the browser, and its answer. `session`: Chromium's, for one tab.
    pub(super) fn command(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        // What runs or prods the page's scripts, which a dialog stops.
        let stalls = ["script.", "input.", "Runtime.", "Input."].iter().any(|p| method.starts_with(p));
        let id = self.browser.as_mut().context("the browser is not running")?.send(session, method, params);
        let until = Instant::now() + ANSWER_WITHIN;
        loop {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                bail!("{method}: the browser did not answer in {} s", ANSWER_WITHIN.as_secs());
            };
            if let Some(m) = self.take(left, Some(id))? {
                // Firefox says `type: error` with a message; Chromium an `error` object.
                if m["type"] == "error" {
                    bail!("{}", m["message"].as_str().unwrap_or("the browser refused"));
                }
                if let Some(e) = m.get("error").filter(|e| !e.is_null()) {
                    bail!("{}", e["message"].as_str().unwrap_or("the browser refused"));
                }
                return Ok(m["result"].clone());
            }
            // A script, or a click, that opened a dialog does not return until it is answered.
            if stalls && self.dialog_open() {
                bail!("a dialog opened");
            }
        }
    }

    pub(super) fn dialog_open(&self) -> bool {
        self.browser.as_ref().is_some_and(|f| f.pages.iter().any(|p| p.dialog.is_some()))
    }

    /// Let events come until `done` says so, or for `at_most`.
    pub(super) fn until(&mut self, at_most: Duration, done: impl Fn(&mut Self) -> bool) -> Result<bool> {
        let end = Instant::now() + at_most;
        while !done(self) {
            let Some(left) = end.checked_duration_since(Instant::now()) else { return Ok(false) };
            self.take(left.min(Duration::from_millis(100)), None)?;
        }
        Ok(true)
    }

    fn kind(&self) -> Kind {
        self.browser.as_ref().map_or(Kind::Firefox, |b| b.proc.found.kind)
    }

    /// The page's own document, as something to run functions in.
    pub(super) fn top_frame(&mut self, id: u32) -> Result<Frame> {
        let kind = self.kind();
        let p = self.page(id)?;
        Ok(Frame { ctx: p.ctx.clone(), frame: (kind == Kind::Chromium).then(|| p.main_frame.clone()) })
    }

    /// Run `function` in nib's own world of the page's document.
    fn js(&mut self, id: u32, function: &str, args: &[Arg]) -> Result<Ret> {
        let frame = self.top_frame(id)?;
        self.b_call(&frame, function, args)
    }

    fn start(&mut self) -> Result<()> {
        let proc = Process::start(&self.base, &self.root)?;
        let name = proc.found.name.clone();
        let (out, mut rx) = ws::connect(proc.port, &proc.path).with_context(|| format!("connecting to {name}"))?;
        self.generation += 1;
        let (g, tx) = (self.generation, self.tx.clone());
        std::thread::spawn(move || loop {
            match rx.next() {
                Ok(text) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        if tx.send(Job::Wire(g, v)).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => {
                    let _ = tx.send(Job::Closed(g));
                    break;
                }
            }
        });
        self.browser = Some(Browser {
            proc, out, version: String::new(), next: 0, pages: Vec::new(), next_page: 0, current: None,
            owner: HashMap::new(), popups: HashMap::new(), first: None, making: false, made: None, later: Vec::new(),
            critical: HashSet::new(), urls: HashMap::new(),
        });
        if let Err(e) = self.b_set_up() {
            self.lost();
            return Err(e.context(format!("{name} cannot be used to show pages")));
        }
        Ok(())
    }

    // ---- pages ----

    /// A new tab in the browser for a new web tab.
    fn new_page(&mut self) -> Result<u32> {
        self.live()?.making = true;
        let made = self.b_new_tab();
        let f = self.live()?;
        f.making = false;
        let (ctx, target, main_frame) = made?;
        // Had its "created" come after the answer, it would have been taken for a page's window.
        f.popups.remove(&ctx);
        f.next_page += 1;
        let id = f.next_page;
        f.owner.insert(ctx.clone(), id);
        f.pages.push(Page {
            id, ctx, target, main_frame, url: String::new(), title: String::new(), loading: false, dom: false,
            console: Vec::new(), failed: Vec::new(), blocked: None, blocks: 0, dialog: None, frames: Vec::new(), downloads: Vec::new(),
            choice: None, cursor: String::new(), saved: Vec::new(), zoom: 1.0,
        });
        let _ = self.b_viewport(id, UNSEEN.0, UNSEEN.1, 1.0);
        // The tab a browser starts with is never used. In Firefox it keeps the keyboard in its
        // address bar, so a page in it never has focus (no focus events; shortcuts like
        // Jupyter's Shift+Enter are refused); in Chromium it was there before nib could watch
        // its requests. It goes once another tab exists.
        if let Some(first) = self.live()?.first.take() {
            self.b_close_first(&first);
        }
        Ok(id)
    }

    /// Close a page's tab in the browser and forget the page.
    pub(super) fn drop_page(&mut self, id: u32) {
        self.b_close_tab(id);
        let Some(f) = self.browser.as_mut() else { return };
        f.pages.retain(|p| p.id != id);
        f.owner.retain(|_, o| *o != id);
        if f.current == Some(id) {
            f.current = f.pages.last().map(|p| p.id);
        }
    }

    pub(super) fn live(&mut self) -> Result<&mut Browser> {
        self.browser.as_mut().context("no page is open (`web open PORT` shows one)")
    }

    pub(super) fn page(&mut self, id: u32) -> Result<&mut Page> {
        self.live()?.page(id).context("that page was closed")
    }

    /// Which page a request is about: `tab` (its number in `web list`, its port, or the start
    /// of its name), else the only page there is. With several pages and no `tab` the request
    /// is refused: which one is in front is the user's business (they switch tabs, the window
    /// tells the engine), and a guess sends keys into the wrong app.
    fn target(&mut self, call: &Call) -> Result<u32> {
        let f = self.live()?;
        let found = match call.req.get("tab") {
            None | Some(Value::Null) => match f.pages.as_slice() {
                [only] => Some(only.id),
                [] => None,
                many => bail!("{} web tabs are open; say which with --tab ({})", many.len(), tab_choices(many)),
            },
            Some(t) => {
                let want = t.as_str().map(str::to_owned).unwrap_or_else(|| t.to_string());
                let by_index = want.parse::<usize>().ok().and_then(|i| f.pages.get(i)).map(|p| p.id);
                let by_port = f.pages.iter().find(|p| local::app_of(&p.url).is_some_and(|a| a.ends_with(&format!(":{want}")))).map(|p| p.id);
                let lower = want.to_lowercase();
                let by_name = f.pages.iter().find(|p| page::tab_name(&p.title, &p.url).to_lowercase().starts_with(&lower)).map(|p| p.id);
                Some(by_port.or(by_index).or(by_name).with_context(|| format!("no web tab {want:?} (`web list` shows them)"))?)
            }
        };
        let id = found.context("no page is open (`web open PORT` shows one)")?;
        f.current = Some(id);
        Ok(id)
    }

    /// Refuse to go on while a dialog waits: the page's scripts are stopped until it is answered.
    fn no_dialog(&mut self, id: u32) -> Result<()> {
        if let Some(d) = &self.page(id)?.dialog {
            bail!("a dialog is open ({}: {}); answer it with `web dialog accept` or `web dialog dismiss`", d["type"].as_str().unwrap_or("dialog"), d["message"].as_str().unwrap_or(""));
        }
        Ok(())
    }

    /// Where the page is after what was just done to it, and anything it was stopped from opening.
    fn settle(&mut self, id: u32, blocks_before: u64) -> Result<Value> {
        // A page that is navigating away, or showing a dialog, does not answer: say what is known.
        if let Ok(r) = self.js(id, page::WHERE, &[]) {
            if let Ok(v) = serde_json::from_str::<Value>(r.value.as_str().unwrap_or("")) {
                let p = self.page(id)?;
                p.url = v["url"].as_str().unwrap_or("").to_owned();
                p.title = v["title"].as_str().unwrap_or("").to_owned();
            }
        }
        let p = self.page(id)?;
        let mut out = json!({ "url": p.url, "title": p.title });
        if p.blocks > blocks_before {
            out["blocked"] = json!(p.blocked);
        }
        if let Some(d) = &p.dialog {
            out["dialog"] = d.clone();
        }
        Ok(out)
    }

    fn blocks(&mut self, id: u32) -> Result<u64> {
        Ok(self.page(id)?.blocks)
    }

    fn open(&mut self, asked: &str) -> Result<Value> {
        let url = local::spell_out(asked);
        if !local::is_local(&url) {
            bail!("{} is not this machine: the web tab shows local apps only (localhost)", local::host_of(&url));
        }
        if self.browser.is_none() {
            self.start()?;
        }
        // The same app again is the same tab.
        let app = local::app_of(&url);
        let f = self.live()?;
        let existing = f.pages.iter().find(|p| local::app_of(&p.url) == app).map(|p| p.id);
        let id = match existing {
            Some(id) => id,
            None => self.new_page()?,
        };
        self.live()?.current = Some(id);
        let before = self.blocks(id)?;
        {
            let p = self.page(id)?;
            p.console.clear();
            p.failed.clear();
            p.blocked = None;
            p.frames.clear();
        }
        if let Err(e) = self.b_navigate(id, &url) {
            let text = e.to_string();
            // A tab made for a page that never came is not kept.
            let blocked = self.browser.as_mut().and_then(|f| f.page(id)).filter(|p| p.blocks > before).and_then(|p| p.blocked.clone());
            if existing.is_none() {
                self.drop_page(id);
            }
            if text.contains("CONNECTION_REFUSED") {
                bail!("nothing is listening at {}", app.unwrap_or(url));
            }
            if let Some(b) = blocked {
                bail!("blocked: {b} is not this machine (the page sent there)");
            }
            bail!("{text}");
        }
        self.page(id)?.url = url;
        let mut out = self.settle(id, before)?;
        out["tab"] = json!(self.live()?.pages.iter().position(|p| p.id == id));
        out["name"] = json!(page::tab_name(out["title"].as_str().unwrap_or(""), out["url"].as_str().unwrap_or("")));
        Ok(out)
    }

    fn read(&mut self, id: u32, full: bool) -> Result<Value> {
        // With a dialog up the page cannot be asked anything: the dialog is what there is to read.
        if let Some(d) = self.page(id)?.dialog.clone() {
            let p = self.page(id)?;
            return Ok(json!({ "url": p.url, "title": p.title, "dialog": d, "text": "", "elements": [], "note": "a dialog is open; answer it with `web dialog accept` or `web dialog dismiss`" }));
        }
        let max = if full { 400_000 } else { TEXT };
        let mut out = Value::Null;
        let mut frames = Vec::new();
        let mut base = 0u64;
        for (n, (frame, url)) in self.b_frames(id)?.into_iter().enumerate() {
            let read = self.b_call(&frame, page::READ, &[number(max as u64), number(base)]);
            let part: Value = match read.ok().and_then(|r| serde_json::from_str(r.value.as_str().unwrap_or("")).ok()) {
                Some(v) => v,
                // The page itself must answer; a frame that does not (still loading, empty) is skipped.
                None if n == 0 => bail!("the page did not answer (it may be loading: `web wait`)"),
                None => continue,
            };
            let count = part["count"].as_u64().unwrap_or(0);
            if n == 0 {
                out = part;
            } else if count > 0 || !part["text"].as_str().unwrap_or("").is_empty() {
                let text = format!("{}\n--- frame {} ---\n{}", out["text"].as_str().unwrap_or(""), page::short_address(&url), part["text"].as_str().unwrap_or(""));
                out["text"] = json!(text);
                if let (Some(all), Some(more)) = (out["elements"].as_array_mut(), part["elements"].as_array()) {
                    all.extend(more.iter().cloned().map(|mut e| {
                        e["frame"] = json!(page::short_address(&url));
                        e
                    }));
                }
                if part["truncated"] == true {
                    out["truncated"] = json!(true);
                }
            }
            frames.push((frame, base, count));
            base += count;
        }
        let tab = self.live()?.pages.iter().position(|p| p.id == id);
        let p = self.page(id)?;
        p.frames = frames;
        p.url = out["url"].as_str().unwrap_or("").to_owned();
        p.title = out["title"].as_str().unwrap_or("").to_owned();
        if let Some(o) = out.as_object_mut() {
            o.remove("count");
            o.insert("tab".into(), json!(tab));
            o.insert("name".into(), json!(page::tab_name(&p.title, &p.url)));
            o.insert("loading".into(), json!(p.loading));
            if p.zoom != 1.0 {
                o.insert("zoom".into(), json!(percent(p.zoom)));
            }
            o.insert("console".into(), Value::Array(std::mem::take(&mut p.console)));
            o.insert("failed".into(), Value::Array(std::mem::take(&mut p.failed)));
            if let Some(b) = &p.blocked {
                o.insert("blocked".into(), json!(b));
            }
            if !p.downloads.is_empty() {
                o.insert("downloads".into(), Value::Array(std::mem::take(&mut p.downloads)));
            }
        }
        Ok(out)
    }

    /// Element `r` of the last read: the frame it is in, and its number there.
    fn place(&mut self, id: u32, r: u64) -> Result<(Frame, u64)> {
        let p = self.page(id)?;
        if p.frames.is_empty() {
            bail!("`web read` the page first: elements are numbered by it");
        }
        p.frames
            .iter()
            .find(|(_, base, count)| (*base..*base + *count).contains(&r))
            .map(|(frame, base, _)| (frame.clone(), r - *base))
            .with_context(|| format!("there is no element {r} on the page; `web read` again"))
    }

    /// Element `r`, scrolled into view: its frame and the browser's handle on it.
    fn node(&mut self, id: u32, r: u64) -> Result<(Frame, Value)> {
        let (frame, n) = self.place(id, r)?;
        let e = self.b_call(&frame, page::NODE, &[number(n)]).map_err(|e| match e.to_string().as_str() {
            "gone" => anyhow!("element {r} is no longer on the page; `web read` again"),
            _ => e,
        })?;
        let node = e.node.with_context(|| format!("element {r} is no longer on the page; `web read` again"))?;
        Ok((frame, node))
    }

    /// Input to the page. That it opened a dialog (and so has not "finished") is not a failure:
    /// whoever asked is told about the dialog.
    fn unless_dialog(&mut self, done: Result<()>) -> Result<()> {
        match done {
            Err(_) if self.dialog_open() => Ok(()),
            r => r,
        }
    }

    fn press_mouse(&mut self, frame: &Frame, node: &Value) -> Result<()> {
        let done = self.b_click(frame, node);
        self.unless_dialog(done)
    }

    fn strokes(&mut self, id: u32, strokes: &[Stroke]) -> Result<()> {
        let done = self.b_strokes(id, strokes);
        self.unless_dialog(done)
    }

    fn click(&mut self, id: u32, r: u64) -> Result<Value> {
        let before = self.blocks(id)?;
        let (frame, node) = self.node(id, r)?;
        self.press_mouse(&frame, &node)?;
        self.settle(id, before)
    }

    fn fill(&mut self, id: u32, r: u64, text: &str, wait: bool) -> Result<Value> {
        let before = self.blocks(id)?;
        let (frame, n) = self.place(id, r)?;
        // A <select> is chosen from; anything else is clicked, emptied and typed into.
        let chosen = self.b_call(&frame, page::SELECT, &[number(n), string(text)]).map_err(|e| match e.to_string().as_str() {
            "gone" => anyhow!("element {r} is no longer on the page; `web read` again"),
            _ => e,
        })?;
        if chosen.value != true {
            let (frame, node) = self.node(id, r)?;
            self.press_mouse(&frame, &node)?;
            // Everything in it selected (by the page's own means: the keys for that differ by
            // system and are not always heard by a browser nobody sees), then typed over.
            self.b_call(&frame, page::SELECT_ALL, &[number(n)])?;
            let mut strokes = page::press(&[], key::BACKSPACE);
            strokes.extend(page::text_strokes(text));
            self.strokes(id, &strokes)?;
            if wait {
                self.pump(TYPED_SETTLE)?;
            }
        }
        self.settle(id, before)
    }

    fn select(&mut self, id: u32, r: u64, option: &str) -> Result<Value> {
        let before = self.blocks(id)?;
        let (frame, n) = self.place(id, r)?;
        let chosen = self.b_call(&frame, page::SELECT, &[number(n), string(option)])?;
        if chosen.value != true {
            bail!("element {r} is not a list to choose from (a <select>)");
        }
        self.settle(id, before)
    }

    fn wait(&mut self, id: u32, call: &Call) -> Result<Value> {
        let secs = call.req.get("secs").and_then(Value::as_f64).unwrap_or(10.0).clamp(0.0, 600.0);
        let gone = call.req.get("gone").and_then(Value::as_bool).unwrap_or(false);
        let text = call.str_arg("text").map(str::to_owned);
        let element = call.req.get("ref").and_then(as_ref);
        let started = Instant::now();
        loop {
            self.no_dialog(id)?;
            let there = if let Some(t) = &text {
                let mut any = false;
                for (frame, _) in self.b_frames(id)? {
                    if self.b_call(&frame, page::HAS_TEXT, &[string(t)]).is_ok_and(|v| v.value == true) {
                        any = true;
                        break;
                    }
                }
                any != gone
            } else if let Some(r) = element {
                let (frame, n) = self.place(id, r)?;
                self.b_call(&frame, page::HAS_NODE, &[number(n)]).is_ok_and(|v| v.value == true) != gone
            } else {
                // Nothing named: until the page has loaded.
                let ready = self.js(id, page::WHERE, &[]).ok().and_then(|r| serde_json::from_str::<Value>(r.value.as_str().unwrap_or("")).ok());
                !self.page(id)?.loading && ready.is_some_and(|v| v["ready"] == "complete")
            };
            if there {
                return Ok(json!({ "waited_ms": started.elapsed().as_millis() as u64 }));
            }
            if started.elapsed().as_secs_f64() >= secs {
                let what = match (&text, element) {
                    (Some(t), _) => format!("{t:?} is {}", if gone { "still there" } else { "not there" }),
                    (_, Some(r)) => format!("element {r} is {}", if gone { "still there" } else { "not there" }),
                    _ => "the page is still loading".to_string(),
                };
                bail!("after {secs} s {what}");
            }
            self.pump(Duration::from_millis(50))?;
        }
    }

    fn shot(&mut self, id: u32, call: &Call) -> Result<Value> {
        let full = call.req.get("full").and_then(Value::as_bool).unwrap_or(false);
        let element = match call.req.get("ref").and_then(as_ref) {
            Some(r) => Some(self.node(id, r)?),
            None => None,
        };
        let data = self.b_shot(id, full, element.as_ref())?;
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.decode(&data)?;
        let (width, height) = png_size(&png);
        // An agent on another machine gets the picture itself (`nibd web` writes it there).
        if call.req.get("inline").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(json!({ "png": data, "width": width, "height": height }));
        }
        let path = call.str_arg("path").map(PathBuf::from).or_else(crate::control::next_snapshot_path).context("no file named and no run folder to save in")?;
        std::fs::write(&path, &png).with_context(|| format!("writing {}", path.display()))?;
        Ok(json!({ "path": path, "width": width, "height": height }))
    }

    fn serve(&mut self, call: &Call) -> Result<Value> {
        let cmd = call.cmd();
        let text = |k: &str| call.str_arg(k).with_context(|| format!("{cmd} needs {k:?}"));
        let element = || call.req.get("ref").and_then(as_ref).with_context(|| format!("{cmd} needs \"ref\": an element's number from `web read`"));
        match cmd {
            "web.open" => {
                let opened = self.open(text("url")?)?;
                // The tab comes forward, unless asked to stay behind.
                if !call.req.get("background").and_then(Value::as_bool).unwrap_or(false) {
                    self.shown += 1;
                }
                return Ok(opened);
            }
            "web.list" => {
                let tabs = match &self.browser {
                    Some(f) => f.pages.iter().enumerate().map(|(i, p)| json!({ "tab": i, "name": page::tab_name(&p.title, &p.url), "url": p.url, "loading": p.loading, "current": f.current == Some(p.id) })).collect(),
                    None => Vec::new(),
                };
                return Ok(json!({ "tabs": tabs }));
            }
            "web.quit" => {
                self.quit();
                return Ok(json!({}));
            }
            _ => {}
        }
        let id = self.target(call)?;
        match cmd {
            "web.close" => {
                self.drop_page(id);
                // The last page gone: no reason to keep a browser.
                if self.browser.as_ref().is_some_and(|f| f.pages.is_empty()) {
                    self.quit();
                }
                Ok(json!({}))
            }
            "web.read" => self.read(id, call.req.get("full").and_then(Value::as_bool).unwrap_or(false)),
            "web.shot" => self.shot(id, call),
            "web.dialog" => {
                if self.page(id)?.dialog.is_none() {
                    bail!("no dialog is open");
                }
                self.b_dialog(id, call.req.get("accept").and_then(Value::as_bool).unwrap_or(true), call.str_arg("text"))?;
                self.page(id)?.dialog = None;
                let before = self.blocks(id)?;
                self.settle(id, before)
            }
            _ => {
                self.no_dialog(id)?;
                let before = self.blocks(id)?;
                match cmd {
                    "web.click" => self.click(id, element()?),
                    "web.fill" => self.fill(id, element()?, text("text")?, call.req.get("wait").and_then(Value::as_bool).unwrap_or(true)),
                    "web.select" => self.select(id, element()?, text("option")?),
                    "web.type" => {
                        self.strokes(id, &page::text_strokes(text("text")?))?;
                        self.settle(id, before)
                    }
                    "web.key" => {
                        self.strokes(id, &page::chord_strokes(text("chord")?)?)?;
                        self.settle(id, before)
                    }
                    "web.scroll" => {
                        if let Some(r) = call.req.get("ref").and_then(as_ref) {
                            self.node(id, r)?;
                            // Scrolled to, but a box around it may leave it no room at all.
                            let (frame, n) = self.place(id, r)?;
                            let seen = self.b_call(&frame, page::VISIBLE, &[number(n)]).ok().and_then(|v| serde_json::from_str::<Value>(v.value.as_str().unwrap_or("")).ok());
                            let mut out = self.settle(id, before)?;
                            if let Some(s) = seen.filter(|s| s["seen"].as_f64() == Some(0.0)) {
                                out["visible"] = json!(false);
                                out["note"] = json!(format!(
                                    "element {r} is scrolled to but cannot be seen: a box around it clips it ({}x{} px); the page may be laid out too small for it (`zoom 75`, then read again)",
                                    s["w"], s["h"]
                                ));
                            }
                            return Ok(out);
                        }
                        let dy = call.req.get("dy").and_then(Value::as_f64).context("web.scroll needs \"ref\" or \"dy\"")?;
                        let (w, h) = self.laid_out(id)?;
                        let done = self.b_wheel(id, w / 2.0, h / 2.0, 0.0, dy);
                        self.unless_dialog(done)?;
                        self.settle(id, before)
                    }
                    "web.wait" => self.wait(id, call),
                    "web.zoom" => {
                        let z = self.page(id)?.zoom;
                        let want = match (call.req.get("percent").and_then(Value::as_f64), call.req.get("steps").and_then(Value::as_i64)) {
                            (Some(p), _) => p as f32 / 100.0,
                            (None, Some(0)) => 1.0,
                            (None, Some(s)) => super::zoom_step(z, s as i32),
                            (None, None) => z,
                        };
                        if want != z {
                            self.set_zoom(id, want)?;
                            // Laid out anew: give the page a moment, as after any input.
                            self.pump(Duration::from_millis(150))?;
                        }
                        let (width, height) = self.laid_out(id)?;
                        let mut out = self.settle(id, before)?;
                        out["zoom"] = json!(percent(self.page(id)?.zoom));
                        out["laid_out"] = json!({ "width": width, "height": height });
                        Ok(out)
                    }
                    "web.back" => {
                        // The page's own history, as its Back button would: both browsers alike.
                        self.page(id)?.frames.clear();
                        let _ = self.js(id, "() => { history.back(); }", &[]);
                        self.pump(Duration::from_millis(150))?;
                        self.settle(id, before)
                    }
                    "web.reload" => {
                        self.page(id)?.frames.clear();
                        self.b_reload(id)?;
                        self.settle(id, before)
                    }
                    "web.upload" => {
                        let files: Vec<String> = call.req.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(|f| f.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                        if files.is_empty() {
                            bail!("web.upload needs \"files\"");
                        }
                        if let Some(missing) = files.iter().find(|f| !std::path::Path::new(f).is_file()) {
                            bail!("{missing} is not a file on this machine");
                        }
                        let (frame, node) = self.node(id, element()?)?;
                        self.b_files(&frame, &node, &files)?;
                        self.settle(id, before)
                    }
                    "web.eval" => {
                        // The page's own world (not nib's sandbox), for the user at the socket;
                        // agents are not given this one (`nibd web` refuses it).
                        let said = self.b_eval(id, &format!("(async () => JSON.stringify(await ({})))()", text("js")?))?;
                        Ok(json!({ "value": serde_json::from_str::<Value>(&said).unwrap_or(Value::Null) }))
                    }
                    other => Err(anyhow!("unknown cmd {other:?}")),
                }
            }
        }
    }
}

/// What each kind of browser must be able to do. `bidi.rs` and `cdp.rs` hold the two halves.
macro_rules! either {
    ($self:ident, $bidi:ident, $cdp:ident ( $($arg:expr),* )) => {
        match $self.kind() {
            Kind::Firefox => $self.$bidi($($arg),*),
            Kind::Chromium => $self.$cdp($($arg),*),
        }
    };
}

impl Engine {
    /// After connecting: a session, and the local-only rule in place before any page exists.
    fn b_set_up(&mut self) -> Result<()> {
        either!(self, bidi_set_up, cdp_set_up())
    }
    fn b_quit(&mut self) {
        either!(self, bidi_quit, cdp_quit())
    }
    /// A new tab: what to address it by, and Chromium's names for it and its document.
    fn b_new_tab(&mut self) -> Result<(String, String, String)> {
        either!(self, bidi_new_tab, cdp_new_tab())
    }
    fn b_close_tab(&mut self, id: u32) {
        either!(self, bidi_close_tab, cdp_close_tab(id))
    }
    /// Close the tab the browser started with.
    fn b_close_first(&mut self, first: &str) {
        either!(self, bidi_close_first, cdp_close_first(first))
    }
    /// Go to `url` and return once the document can be read (not: has finished loading).
    fn b_navigate(&mut self, id: u32, url: &str) -> Result<()> {
        either!(self, bidi_navigate, cdp_navigate(id, url))
    }
    pub(super) fn b_reload(&mut self, id: u32) -> Result<()> {
        either!(self, bidi_reload, cdp_reload(id))
    }
    /// The page's documents: itself first, then its frames, outermost first, with their addresses.
    fn b_frames(&mut self, id: u32) -> Result<Vec<(Frame, String)>> {
        either!(self, bidi_frames, cdp_frames(id))
    }
    /// Run a function in nib's own world of a document (not among the page's globals).
    pub(super) fn b_call(&mut self, frame: &Frame, function: &str, args: &[Arg]) -> Result<Ret> {
        either!(self, bidi_call, cdp_call(frame, function, args))
    }
    /// A real click of the mouse in the middle of an element.
    fn b_click(&mut self, frame: &Frame, node: &Value) -> Result<()> {
        either!(self, bidi_click, cdp_click(frame, node))
    }
    /// Keys, to whatever has the keyboard in the page.
    pub(super) fn b_strokes(&mut self, id: u32, strokes: &[Stroke]) -> Result<()> {
        either!(self, bidi_strokes, cdp_strokes(id, strokes))
    }
    /// The wheel turned at a point of the page.
    pub(super) fn b_wheel(&mut self, id: u32, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        either!(self, bidi_wheel, cdp_wheel(id, x, y, dx, dy))
    }
    /// A PNG, as base64: what shows, the whole document, or one element.
    pub(super) fn b_shot(&mut self, id: u32, full: bool, element: Option<&(Frame, Value)>) -> Result<String> {
        either!(self, bidi_shot, cdp_shot(id, full, element))
    }
    /// The mouse, where `self.tab.at` says it is, with `self.tab.mods` held.
    pub(super) fn b_pointer(&mut self, id: u32, what: super::tab::Pointer) -> Result<()> {
        either!(self, bidi_pointer, cdp_pointer(id, what))
    }
    pub(super) fn b_dialog(&mut self, id: u32, accept: bool, text: Option<&str>) -> Result<()> {
        either!(self, bidi_dialog, cdp_dialog(id, accept, text))
    }
    fn b_files(&mut self, frame: &Frame, node: &Value, files: &[String]) -> Result<()> {
        either!(self, bidi_files, cdp_files(frame, node, files))
    }
    /// An expression in the page's own world; what it gave, as text.
    fn b_eval(&mut self, id: u32, expression: &str) -> Result<String> {
        either!(self, bidi_eval, cdp_eval(id, expression))
    }
    /// The size the page is laid out at, and its pixels per CSS pixel.
    pub(super) fn b_viewport(&mut self, id: u32, width: u32, height: u32, scale: f32) -> Result<()> {
        either!(self, bidi_viewport, cdp_viewport(id, width, height, scale))
    }
}

/// A zoom as a whole percent (0.67: 67).
fn percent(zoom: f32) -> u32 {
    (zoom * 100.0).round() as u32
}

/// The open pages as `--tab` would name them: `5173 (Performance Explorer), 2718 (lvef)`.
fn tab_choices(pages: &[Page]) -> String {
    pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let port = local::app_of(&p.url).and_then(|a| a.rsplit_once(':').map(|(_, port)| port.to_owned()));
            format!("{} ({})", port.unwrap_or_else(|| i.to_string()), page::tab_name(&p.title, &p.url))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// An element's number, given as a number or as text.
fn as_ref(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_must_be_local_and_the_rest_is_the_pages_business() {
        assert!(allowed("http://127.0.0.1:8951/", Some(true)));
        assert!(allowed("http://localhost:3000/inner", Some(true)));
        assert!(!allowed("http://192.168.1.150:8952/direct", Some(true)));
        assert!(!allowed("http://example.com/via-frame", Some(true)));
        // What a page uses: images, scripts, fonts, fetch.
        assert!(allowed("http://example.com/pixel.png", Some(false)));
        assert!(allowed("https://api.example/x", Some(false)));
        // What a page makes itself.
        assert!(allowed("about:blank", Some(true)));
        assert!(allowed("about:srcdoc", Some(true)));
        assert!(allowed("data:text/html,hi", Some(true)));
        assert!(allowed("blob:http://localhost:8888/3f2a", Some(true)));
        assert!(!allowed("blob:https://example.com/3f2a", Some(true)));
        assert!(!allowed("file:///etc/passwd", Some(true)));
        assert!(!allowed("javascript:alert(1)", Some(true)));
        assert!(!allowed("about:config", Some(true)));
        assert!(!allowed("chrome://settings", Some(true)));
        // What the browser said could not be read: refused, even a local address.
        assert!(!allowed("http://localhost/x.png", None));
        assert!(!allowed("http://example.com/x.png", None));
    }

    #[test]
    fn a_png_says_its_size() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        png.extend(1280u32.to_be_bytes());
        png.extend(800u32.to_be_bytes());
        assert_eq!(png_size(&png), (1280, 800));
        assert_eq!(png_size(b"short"), (0, 0));
    }
}
