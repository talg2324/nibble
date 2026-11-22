//! The engine's side of a web tab: pictures of the page for nib to draw, and what a person
//! does in the tab (mouse, wheel, keys, a choice from a list, an answer to a dialog) passed on
//! to the page. The window's side is `app/web.rs`.
//!
//! A browser nobody sees draws no dropdown lists and shows no dialogs, so those are nib's to
//! show: a click on a `<select>` is not passed on but turned into a list for the tab to offer.

use super::engine::{Engine, Frame};
use super::page::{self, key, Stroke};
use super::{Choice, Input, Picture, TabJob, View};
use anyhow::Result;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Pictures are asked for this often while something is happening in the page…
const BUSY_EVERY: Duration = Duration::from_millis(33);
/// …and this often while it is still (a still page costs next to nothing).
const STILL_EVERY: Duration = Duration::from_millis(500);
/// After input, how long the page counts as busy; and after a picture that differed.
const BUSY_AFTER_INPUT: Duration = Duration::from_millis(1000);
const BUSY_AFTER_CHANGE: Duration = Duration::from_millis(400);
/// Two presses this close in time and place are a double click.
const DOUBLE_WITHIN: Duration = Duration::from_millis(400);
/// The pointer's shape is asked for at most this often.
const SHAPE_EVERY: Duration = Duration::from_millis(100);

/// What the mouse did, for a browser.
pub(super) enum Pointer {
    Move,
    /// `clicks`: 1, or 2 for the second press of a double click.
    Down { button: u8, clicks: u8 },
    Up { button: u8, clicks: u8 },
}

pub(super) struct TabState {
    /// The page in front and the size it is shown at; None: no web tab shows.
    view: Option<(u32, View)>,
    /// What the browser was last told (page, size, zoom).
    applied: Option<(u32, View, f32)>,
    last_shot: Instant,
    last_data: String,
    busy_until: Instant,
    /// Where the mouse is over the page, the modifier keys held with a mouse button, and the
    /// buttons the page knows to be down (as the bits browsers use: 1, 4, 2).
    pub at: (f64, f64),
    pub mods: Vec<char>,
    pub buttons: u32,
    last_press: Option<(Instant, (f64, f64), u8)>,
    clicks: u8,
    /// A press that became a list of choices: its release is not the page's either.
    swallowed: bool,
    last_shape: Instant,
}

impl TabState {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            view: None, applied: None, last_shot: now, last_data: String::new(), busy_until: now, at: (0.0, 0.0), mods: Vec::new(),
            buttons: 0, last_press: None, clicks: 1, swallowed: false, last_shape: now,
        }
    }

    /// How long the engine may wait for work before the next picture is due (None: no tab shows).
    pub fn due_in(&self) -> Option<Duration> {
        self.view?;
        let every = if Instant::now() < self.busy_until { BUSY_EVERY } else { STILL_EVERY };
        Some(every.saturating_sub(self.last_shot.elapsed()))
    }

    fn busy(&mut self, for_: Duration) {
        self.busy_until = self.busy_until.max(Instant::now() + for_);
    }
}

impl Engine {
    pub(super) fn tab_job(&mut self, job: TabJob) {
        // A browser that went away, or a page that closed, makes these moot: errors are dropped.
        let _ = self.tab_do(job);
    }

    fn tab_do(&mut self, job: TabJob) -> Result<()> {
        match job {
            TabJob::View { page, view } => {
                self.tab.view = Some((page, view));
                self.live()?.current = Some(page);
                self.tab.busy(BUSY_AFTER_INPUT);
            }
            TabJob::Hide => self.tab.view = None,
            TabJob::Close(page) => {
                self.drop_page(page);
                if self.browser.as_ref().is_some_and(|b| b.pages.is_empty()) {
                    self.quit();
                }
            }
            TabJob::Input { page, input } => {
                self.tab.busy(BUSY_AFTER_INPUT);
                self.tab_input(page, input)?;
            }
            TabJob::Back(page) => {
                let frame = self.top_frame(page)?;
                let _ = self.b_call(&frame, "() => { history.back(); }", &[]);
                self.tab.busy(BUSY_AFTER_INPUT);
            }
            TabJob::Reload(page) => {
                self.page(page)?.blocked = None;
                self.tab.busy(BUSY_AFTER_INPUT);
                self.b_reload(page)?;
            }
            TabJob::Dialog { page, accept, text } => {
                if self.page(page)?.dialog.take().is_some() {
                    self.b_dialog(page, accept, text.as_deref())?;
                }
                self.tab.busy(BUSY_AFTER_INPUT);
            }
            TabJob::Choose { page, index } => {
                if let (Some(c), Some(i)) = (self.page(page)?.choice.take(), index) {
                    self.b_call(&c.frame, page::CHOOSE, &[super::engine::Arg::Num(i as u64)])?;
                }
                self.tab.busy(BUSY_AFTER_INPUT);
            }
            TabJob::Seen(page) => self.page(page)?.blocked = None,
            TabJob::Copy { page, cut } => {
                let frame = self.top_frame(page)?;
                let text = self.b_call(&frame, page::SELECTED, &[])?.value.as_str().unwrap_or("").to_owned();
                if !text.is_empty() {
                    if cut {
                        self.b_strokes(page, &page::press(&[], key::BACKSPACE))?;
                    }
                    super::copied(text);
                }
            }
            TabJob::Paste { page, text } => {
                let frame = self.top_frame(page)?;
                self.b_call(&frame, page::INSERT, &[super::engine::Arg::Str(text)])?;
                self.tab.busy(BUSY_AFTER_INPUT);
            }
            TabJob::Zoom { page, steps } => {
                let z = self.page(page)?.zoom;
                self.set_zoom(page, if steps == 0 { 1.0 } else { super::zoom_step(z, steps) })?;
            }
        }
        Ok(())
    }

    /// The size page `id` is laid out at now, in CSS pixels: its tab's, or (no tab shows it)
    /// the size an unseen page has, over its zoom.
    pub(super) fn laid_out(&mut self, id: u32) -> Result<(f64, f64)> {
        let z = self.page(id)?.zoom as f64;
        let (w, h) = match self.tab.view {
            Some((page, view)) if page == id => (view.width, view.height),
            _ => super::engine::UNSEEN,
        };
        Ok(((w as f64 / z).round(), (h as f64 / z).round()))
    }

    /// Zoom page `id` to `zoom`: laid out at its size / `zoom`, drawn `zoom` times bigger, as a
    /// browser zooms. A page in front gets its new size with its next picture (now).
    pub(super) fn set_zoom(&mut self, id: u32, zoom: f32) -> Result<()> {
        // Steps there and back land on 100% again.
        let zoom = if (zoom - 1.0).abs() < 0.02 { 1.0 } else { zoom.clamp(super::ZOOMS[0], super::ZOOMS[super::ZOOMS.len() - 1]) };
        self.page(id)?.zoom = zoom;
        if self.tab.view.is_some_and(|(page, _)| page == id) {
            self.tab.applied = None;
            self.tab.last_shot = Instant::now().checked_sub(STILL_EVERY).unwrap_or(self.tab.last_shot);
            self.tab.busy(BUSY_AFTER_INPUT);
            self.tab_picture();
        } else {
            let (w, h) = super::engine::UNSEEN;
            self.b_viewport(id, (w as f32 / zoom).round() as u32, (h as f32 / zoom).round() as u32, zoom)?;
            // The tab, when it comes to show this page, sets its own size.
            if self.tab.applied.is_some_and(|(page, ..)| page == id) {
                self.tab.applied = None;
            }
        }
        Ok(())
    }

    fn tab_input(&mut self, page: u32, input: Input) -> Result<()> {
        // Scripts of a page with a dialog up are stopped; input would only queue behind it.
        if self.page(page)?.dialog.is_some() {
            return Ok(());
        }
        match input {
            Input::Move { x, y } => {
                self.tab.at = (x as f64, y as f64);
                self.b_pointer(page, Pointer::Move)?;
                if self.tab.last_shape.elapsed() >= SHAPE_EVERY && self.tab.buttons == 0 {
                    self.tab.last_shape = Instant::now();
                    let frame = self.top_frame(page)?;
                    let (x, y) = self.tab.at;
                    if let Ok(r) = self.b_call(&frame, page::SHAPE, &[num(x), num(y)]) {
                        self.page(page)?.cursor = r.value.as_str().unwrap_or("").to_owned();
                    }
                }
            }
            Input::Down { button, mods } => {
                let (x, y) = self.tab.at;
                // A press on a list to choose from opens nib's own list instead.
                if button == 0 && self.page(page)?.choice.is_none() {
                    let frame = self.top_frame(page)?;
                    if let Some(c) = self.b_call(&frame, page::PICK, &[num(x), num(y)]).ok().and_then(|r| choice(&frame, &r.value)) {
                        self.page(page)?.choice = Some(c);
                        self.tab.swallowed = true;
                        return Ok(());
                    }
                }
                self.page(page)?.choice = None;
                let again = self.tab.last_press.is_some_and(|(t, at, b)| b == button && t.elapsed() < DOUBLE_WITHIN && (at.0 - x).abs() < 5.0 && (at.1 - y).abs() < 5.0);
                self.tab.clicks = if again && self.tab.clicks == 1 { 2 } else { 1 };
                self.tab.last_press = Some((Instant::now(), (x, y), button));
                self.tab.mods = mods;
                self.tab.buttons |= bit(button);
                self.b_pointer(page, Pointer::Down { button, clicks: self.tab.clicks })?;
            }
            Input::Up { button } => {
                if std::mem::take(&mut self.tab.swallowed) {
                    return Ok(());
                }
                self.tab.buttons &= !bit(button);
                let done = self.b_pointer(page, Pointer::Up { button, clicks: self.tab.clicks });
                self.tab.mods.clear();
                // A release that opened a dialog "fails" by not returning; that is no failure.
                if done.is_err() && !self.dialog_open() {
                    return done;
                }
            }
            Input::Wheel { dx, dy } => {
                let (x, y) = self.tab.at;
                self.b_wheel(page, x, y, dx as f64, dy as f64)?;
            }
            Input::Keys(strokes) => {
                let strokes: Vec<Stroke> = strokes;
                let done = self.b_strokes(page, &strokes);
                if done.is_err() && !self.dialog_open() {
                    return done;
                }
            }
        }
        Ok(())
    }

    /// Take a picture of the page in front if one is due, and leave it for the window.
    pub(super) fn tab_picture(&mut self) {
        let Some((page, view)) = self.tab.view else { return };
        if self.tab.due_in().is_some_and(|d| !d.is_zero()) || self.browser.as_ref().is_none_or(|b| !b.pages.iter().any(|p| p.id == page)) {
            return;
        }
        self.tab.last_shot = Instant::now();
        let zoom = self.browser.as_mut().and_then(|b| b.page(page)).map_or(1.0, |p| p.zoom);
        if self.tab.applied != Some((page, view, zoom)) {
            let (w, h) = ((view.width as f32 / zoom).round() as u32, (view.height as f32 / zoom).round() as u32);
            if self.b_viewport(page, w, h, view.scale * zoom).is_err() {
                return;
            }
            self.tab.applied = Some((page, view, zoom));
            self.tab.last_data.clear();
        }
        let Ok(data) = self.b_shot(page, false, None) else { return };
        if data == self.tab.last_data {
            return;
        }
        self.tab.busy(BUSY_AFTER_CHANGE);
        use base64::Engine as _;
        let Ok(png) = base64::engine::general_purpose::STANDARD.decode(&data) else { return };
        let Ok(img) = image::load_from_memory_with_format(&png, image::ImageFormat::Png) else { return };
        let img = img.into_rgba8();
        self.tab.last_data = data;
        super::picture(Picture { page, width: img.width(), height: img.height(), rgba: img.into_raw() });
    }
}

fn num(v: f64) -> super::engine::Arg {
    super::engine::Arg::Num(v.max(0.0).round() as u64)
}

/// The bit browsers use for a mouse button in "which are down": left 1, middle 4, right 2.
pub(super) fn bit(button: u8) -> u32 {
    match button {
        0 => 1,
        1 => 4,
        _ => 2,
    }
}

/// What `page::PICK` said about a `<select>` under the pointer.
fn choice(frame: &Frame, said: &Value) -> Option<Choice> {
    let v: Value = serde_json::from_str(said.as_str().filter(|s| !s.is_empty())?).ok()?;
    let n = |k: &str| v[k].as_f64().unwrap_or(0.0) as f32;
    Some(Choice {
        frame: frame.clone(),
        options: v["options"].as_array()?.iter().map(|o| o.as_str().unwrap_or("").to_owned()).collect(),
        selected: v["selected"].as_u64().map(|i| i as usize),
        x: n("x"), y: n("y"), width: n("w"), height: n("h"),
    })
}
