//! Local apps in nib (v0.3): a page served by this machine, shown in a web tab and readable and
//! operable through the control socket, so agents need no browser automation of their own.
//!
//! The page lives in a headless browser that belongs to this window (`engine.rs`); what nib
//! shows of it and what agents do to it both go through the `web.*` requests answered here.
//! Not a web browser: only this machine's pages are ever shown (`local.rs`).

mod bidi;
mod browser;
mod cdp;
mod engine;
pub mod local;
mod page;
mod tab;
mod ws;

pub use browser::{watch_main, WATCH_ARG};

use crate::control::Call;
use engine::{Engine, Job};
use serde_json::Value;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

static JOBS: Mutex<Option<mpsc::Sender<Job>>> = Mutex::new(None);
static STATE: Mutex<Value> = Mutex::new(Value::Null);
/// What the window's web tabs are told: the pages there are, the newest picture of the one in
/// front, text the page gave to copy, and how to wake the window when any of it changes.
static TOLD: Mutex<Told> = Mutex::new(Told { tabs: Vec::new(), front: None, shown: 0, copied: None });
static PICTURE: Mutex<Option<Picture>> = Mutex::new(None);
static WAKE: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// The page as pixels: `width` x `height` RGBA.
pub struct Picture {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What a web tab shows the page at: CSS pixels, and device pixels per CSS pixel.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

/// A `<select>` pressed in the tab: nib shows its options (a browser nobody sees draws no list).
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    frame: engine::Frame,
    pub options: Vec<String>,
    pub selected: Option<usize>,
    /// Where the `<select>` is in the page, in CSS pixels.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One page, as its web tab needs to know it.
#[derive(Clone, Debug, PartialEq)]
pub struct TabInfo {
    pub id: u32,
    pub name: String,
    /// `localhost:6006/path`, and the address in full.
    pub address: String,
    pub url: String,
    pub loading: bool,
    /// An address outside this machine the page tried to show.
    pub blocked: Option<String>,
    /// `{type, message, default}` of an alert / confirm / prompt waiting for its answer.
    pub dialog: Option<Value>,
    pub choice: Option<Choice>,
    /// The shape the pointer should have where it is.
    pub cursor: String,
    /// Files the page saved since last told.
    pub saved: Vec<String>,
    /// How much bigger the page is drawn than at 100%: laid out at the tab's size / `zoom` CSS
    /// pixels, so a pixel of the tab is 1 / `zoom` CSS pixels.
    pub zoom: f32,
}

/// The zoom levels a step goes between, as browsers have them.
pub const ZOOMS: &[f32] = &[0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

/// `steps` levels up (in) or down (out) from `zoom`; from a level not in the list, to the next
/// one in that direction.
pub fn zoom_step(zoom: f32, steps: i32) -> f32 {
    let mut z = zoom;
    for _ in 0..steps.unsigned_abs() {
        z = if steps > 0 {
            ZOOMS.iter().copied().find(|l| *l > z + 0.001).unwrap_or(ZOOMS[ZOOMS.len() - 1])
        } else {
            ZOOMS.iter().rev().copied().find(|l| *l < z - 0.001).unwrap_or(ZOOMS[0])
        };
    }
    z
}

#[derive(Clone)]
pub struct Told {
    pub tabs: Vec<TabInfo>,
    /// The page a request was last about, and how many times a page was asked to be shown:
    /// when that count moves, `front`'s tab comes forward.
    pub front: Option<u32>,
    pub shown: u64,
    pub copied: Option<String>,
}

/// What a person does in a web tab, in CSS pixels of the page.
#[derive(Clone, Debug)]
pub enum Input {
    Move { x: f32, y: f32 },
    /// A mouse button (0 left, 1 middle, 2 right) pressed, with these modifier keys held.
    Down { button: u8, mods: Vec<char> },
    Up { button: u8 },
    Wheel { dx: f32, dy: f32 },
    Keys(Vec<page::Stroke>),
}

/// What the window asks of the engine for its web tabs.
#[derive(Debug)]
pub enum TabJob {
    /// This page is in front, at this size: draw it.
    View { page: u32, view: View },
    /// No web tab is in front: draw nothing.
    Hide,
    Close(u32),
    Input { page: u32, input: Input },
    Back(u32),
    Reload(u32),
    Dialog { page: u32, accept: bool, text: Option<String> },
    /// The user chose from the list (None: put it away).
    Choose { page: u32, index: Option<usize> },
    /// The user has seen what was not opened.
    Seen(u32),
    Copy { page: u32, cut: bool },
    Paste { page: u32, text: String },
    /// Zoom the page `steps` levels in (or out); 0: back to 100%.
    Zoom { page: u32, steps: i32 },
}

pub use page::{key, press};

/// How the engine wakes the window (a message into its loop). Set once at start.
pub fn wake_with(wake: impl Fn() + Send + 'static) {
    *WAKE.lock().unwrap() = Some(Box::new(wake));
}

pub(crate) fn wake() {
    if let Some(w) = WAKE.lock().unwrap().as_ref() {
        w();
    }
}

fn tell(mut tabs: Vec<TabInfo>, front: Option<u32>, shown: u64) {
    let mut t = TOLD.lock().unwrap();
    // Files saved are said once: keep those not yet taken.
    for tab in &mut tabs {
        if let Some(old) = t.tabs.iter_mut().find(|o| o.id == tab.id) {
            let mut saved = std::mem::take(&mut old.saved);
            saved.append(&mut tab.saved);
            tab.saved = saved;
        }
    }
    t.tabs = tabs;
    t.front = front;
    t.shown = shown;
    drop(t);
    wake();
}

fn copied(text: String) {
    TOLD.lock().unwrap().copied = Some(text);
    wake();
}

fn picture(p: Picture) {
    *PICTURE.lock().unwrap() = Some(p);
    wake();
}

/// The pages there are, for the window's tabs. Takes with it what is said only once (text to
/// copy, files saved).
pub fn told() -> Told {
    let mut t = TOLD.lock().unwrap();
    let out = t.clone();
    t.copied = None;
    t.tabs.iter_mut().for_each(|tab| tab.saved.clear());
    out
}

/// The newest picture not yet taken.
pub fn take_picture() -> Option<Picture> {
    PICTURE.lock().unwrap().take()
}

/// Something for the engine from a web tab. Nothing happens when no page was ever opened.
pub fn tab(job: TabJob) {
    if let Some(tx) = JOBS.lock().unwrap().as_ref() {
        let _ = tx.send(Job::Tab(job));
    }
}

/// Hand a `web.*` request to the engine, which answers it itself. The engine's thread starts
/// with the first request; the browser only when a page is first opened.
pub fn submit(call: Call, root: &str) {
    let mut jobs = JOBS.lock().unwrap();
    if jobs.is_none() {
        let (tx, rx) = mpsc::channel();
        let engine = Engine::new(rx, tx.clone(), crate::transport::cache_dir().join("web"), root.to_owned());
        std::thread::spawn(move || engine.run());
        *jobs = Some(tx);
    }
    if let Some(tx) = jobs.as_ref() {
        let _ = tx.send(Job::Call(call));
    }
}

/// What `state` reports under `web`.
pub fn state() -> Value {
    let s = STATE.lock().unwrap();
    if s.is_null() {
        return serde_json::json!({ "browser": browser::describe(), "running": false, "tabs": [] });
    }
    s.clone()
}

fn publish(state: Value) {
    *STATE.lock().unwrap() = state;
}

/// The window is closing: stop the browser, and wait (briefly) until it is gone.
pub fn shutdown() {
    let Some(tx) = JOBS.lock().unwrap().take() else { return };
    let (done, wait) = mpsc::channel();
    if tx.send(Job::Stop(done)).is_ok() {
        let _ = wait.recv_timeout(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_go_between_the_levels() {
        assert_eq!(zoom_step(1.0, -1), 0.9);
        assert_eq!(zoom_step(1.0, 2), 1.25);
        assert_eq!(zoom_step(0.7, 1), 0.75);
        assert_eq!(zoom_step(0.7, -1), 0.67);
        assert_eq!(zoom_step(0.25, -3), 0.25);
        assert_eq!(zoom_step(3.0, 9), 3.0);
        assert_eq!(zoom_step(1.0, 0), 1.0);
    }
}
