//! Settings: the gear menu in the control strip, the Settings panel (Appearance, Keyboard
//! Shortcuts, Languages) and the Beginner-mode cheat sheet.
//!
//! Every change applies immediately and is written back to config.toml (debounced), touching only
//! the keys the UI owns so hand-written comments and profiles survive.

use super::{async_sleep, App, Message};
use crate::color_picker::{from_hsv, to_hsv, ColorPicker, Hsv};
use crate::editor_view::{EditorView, Typeface};
use crate::icons::{hex, Icons};
use crate::theme::{parse_hex, Theme, UI_COLORS};
use iced::widget::{
    button, column, container, mouse_area, pick_list, row, scrollable, slider, space, stack, text, text_input,
    toggler, tooltip,
};
use iced::advanced::widget::{operate, operation, Id as WidgetId, Operation};
use iced::window;
use iced::{mouse, Alignment, Border, Color, Element, Font, Length, Padding, Rectangle, Size, Task, Vector};
use nib_editor::config::{UserSettings, WindowConfig};
use nib_editor::keymap::{Key, KeyChord, Origin};
use nib_editor::{Command, Config, Document, Keymap};
use nib_syntax::LANGUAGES;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Appearance,
    Shortcuts,
    Languages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKey {
    /// A `[theme]` key from `UI_COLORS`.
    Ui(&'static str),
    /// A `[theme.syntax]` scope from `nib_syntax::SCOPES`.
    Syntax(&'static str),
}

/// The colour currently expanded for editing. HSV is kept separately from the hex so dragging
/// saturation to 0 doesn't lose the hue.
struct ColorEdit {
    key: ColorKey,
    hex: String,
    hsv: Hsv,
}

/// The app window as the Appearance page sees it.
#[derive(Default)]
pub struct WindowState {
    pub(super) id: Option<window::Id>,
    /// Logical size of the current monitor.
    monitor: Option<Size>,
    /// Current logical size (tracked from resize events).
    size: Option<Size>,
    pub fullscreen: bool,
    /// Width slider position while dragging. Applied on release: the panel is docked to the
    /// window's right edge, so resizing live would move the slider out from under the cursor.
    drag_width: Option<f32>,
}

impl WindowState {
    /// Current logical height, once known.
    pub fn height(&self) -> Option<f32> {
        self.size.map(|s| s.height)
    }
}

#[derive(Default)]
pub struct State {
    /// Gear menu popover open.
    pub menu: bool,
    /// Settings panel page, if the panel is open.
    pub page: Option<Page>,
    /// Command waiting for its new shortcut.
    pub recording: Option<Command>,
    edit: Option<ColorEdit>,
    /// Index into `LANGUAGES` for the syntax-colour section and preview.
    lang: usize,
    /// Sample documents, one per language (built when Appearance first opens).
    previews: Vec<Document>,
    /// Installed monospace font families (read when Appearance first opens).
    fonts: Vec<String>,
    search: String,
    save_seq: u64,
    pub window: WindowState,
    /// Token clicked in the editor while Appearance is open: (text, scope).
    revealed: Option<(String, &'static str)>,
    langs: Langs,
}

/// The Languages page: grammars to add with `nib grammar add`.
#[derive(Default)]
struct Langs {
    /// `[languages]` in config.toml when the page opened (or after an add).
    added: Vec<String>,
    picked: Vec<&'static str>,
    /// `nib grammar add` is running.
    busy: bool,
    /// Its output: Ok when it succeeded.
    output: Option<Result<String, String>>,
    /// Opened on startup; `[editor] languages_offered` is written when the page closes.
    first_run: bool,
}

#[derive(Debug, Clone)]
pub enum Msg {
    ToggleMenu,
    CloseMenu,
    Open(Page),
    Close,
    Beginner(bool),
    FontSize(f32),
    LineHeight(f32),
    Font(String),
    EditColor(ColorKey),
    HexInput(String),
    Pick(Hsv),
    ResetColor(ColorKey),
    ResetAppearance,
    Lang(usize),
    Search(String),
    Record(Command),
    Unbind(KeyChord),
    ResetCommand(Command),
    ResetAllKeys,
    OpenConfigFile,
    ReloadConfig,
    SaveTick(u64),
    /// The settings writer answered (`save_in_order`): where it wrote, or why it couldn't.
    Saved(Result<PathBuf, String>),
    Noop,
    WindowFound(window::Id),
    Monitor(Option<Size>),
    WindowSize(Size),
    WindowMode(bool),
    /// The window was resized (by the user, the OS, or us).
    Resized(Size),
    Width(f32),
    WidthReleased,
    Height(f32),
    Preset(f32, f32),
    Maximize,
    Fullscreen(bool),
    /// Scroll the revealed scope's row into view (sent one update after revealing, once the
    /// panel has been rebuilt with that row in it).
    ScrollToRevealed,
    PickLanguage(&'static str),
    AddLanguages,
    LanguagesAdded(Result<String, String>),
}

/// Window size presets (logical px). Ones larger than the monitor are hidden.
const PRESETS: &[(&str, f32, f32)] =
    &[("Compact", 1024.0, 700.0), ("Laptop", 1280.0, 800.0), ("Desktop", 1600.0, 1000.0), ("Full HD", 1920.0, 1080.0), ("QHD", 2560.0, 1440.0)];

const SCROLL_ID: &str = "settings-scroll";

fn scope_row_id(scope: &str) -> WidgetId {
    WidgetId::from(format!("scope-row:{scope}"))
}

const SYSTEM_MONO: &str = "System monospace";

/// `Font::with_name` needs `&'static str`; leak each distinct family name once.
pub fn font_for(name: Option<&str>) -> Font {
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let Some(name) = name else { return crate::fonts::mono() };
    let mut names = NAMES.lock().unwrap();
    let s = match names.iter().find(|s| **s == name) {
        Some(s) => *s,
        None => {
            let s: &'static str = Box::leak(name.to_owned().into_boxed_str());
            names.push(s);
            s
        }
    };
    Font::with_name(s)
}

fn installed_monospace_fonts() -> Vec<String> {
    let Ok(mut fs) = iced::advanced::graphics::text::font_system().write() else { return Vec::new() };
    let mut v: Vec<String> = fs
        .raw()
        .db()
        .faces()
        .filter(|f| f.monospaced)
        .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort_by_key(|n| n.to_lowercase());
    v.dedup();
    v
}

/// Category order in the shortcut lists: what a newcomer reaches for first.
const CATEGORIES: &[&str] = &["File", "Edit", "Code", "Navigate", "Tabs", "Panels", "Settings", "Selection", "Cursor"];

fn by_category() -> Vec<(&'static str, Vec<Command>)> {
    let mut v: Vec<(&'static str, Vec<Command>)> = CATEGORIES.iter().map(|c| (*c, Vec::new())).collect();
    for c in Command::ALL {
        match v.iter_mut().find(|(cat, _)| *cat == c.category()) {
            Some((_, cmds)) => cmds.push(*c),
            None => v.push((c.category(), vec![*c])),
        }
    }
    v.retain(|(_, cmds)| !cmds.is_empty());
    v
}

/// `c` moved toward white (dark themes) or black (light themes) by `t`.
pub(super) fn raise(c: Color, t: f32) -> Color {
    let lum = 0.299 * c.r + 0.587 * c.g + 0.114 * c.b;
    let to = if lum < 0.5 { 1.0 } else { 0.0 };
    Color { r: c.r + (to - c.r) * t, g: c.g + (to - c.g) * t, b: c.b + (to - c.b) * t, a: c.a }
}

// ---------------------------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------------------------

impl App {
    /// At boot: `[window]` as configured, so a settings save before the window reports its size
    /// (or headless, where it never does) writes back what is there instead of the defaults.
    pub(super) fn seed_window(&mut self, cfg: &WindowConfig) {
        self.settings.window.size = Some(Size::new(cfg.width, cfg.height));
        if self.headless {
            self.settings.window.fullscreen = cfg.fullscreen;
        }
    }

    pub(super) fn user_settings(&self) -> UserSettings {
        UserSettings {
            font_size: self.face.size,
            font: self.font_name.clone(),
            line_height: self.face.line_height,
            beginner_mode: self.beginner,
            window: {
                let w = &self.settings.window;
                let size = w.size.unwrap_or(Size::new(
                    nib_editor::config::default_window_width(),
                    nib_editor::config::default_window_height(),
                ));
                WindowConfig { width: size.width.round(), height: size.height.round(), fullscreen: w.fullscreen }
            },
            theme: self.theme_cfg.clone(),
            keys: self.keymap.overrides_for_config(),
        }
    }

    fn schedule_save(&mut self) -> Task<Message> {
        self.settings.save_seq += 1;
        let seq = self.settings.save_seq;
        Task::perform(async_sleep(Duration::from_millis(400)), move |_| Message::Settings(Msg::SaveTick(seq)))
    }

    fn save_now(&mut self) -> Result<PathBuf, String> {
        let keys: Vec<&str> = UI_COLORS.iter().map(|u| u.key).collect();
        let path =
            Config::save_settings(&self.user_settings(), &keys).map_err(|e| format!("couldn't save settings: {e}"))?;
        self.refresh_config_tab(&path);
        Ok(path)
    }

    /// Keep an open, unmodified config.toml tab in step with what the settings UI just wrote.
    fn refresh_config_tab(&mut self, path: &std::path::Path) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.local && t.doc.path.as_deref() == Some(path)) else { return };
        if tab.doc.is_modified() {
            return;
        }
        if let Ok(data) = std::fs::read(path) {
            let mut doc = Document::from_bytes(&data, Some(path.to_path_buf()));
            doc.tab_width = self.tab_width;
            doc.disk_mtime_ns = mtime_ns(path);
            let line = tab.doc.cursor_line_col().0.min(doc.text.len_lines().saturating_sub(1));
            let pos = doc.text.line_to_char(line);
            doc.set_cursor(pos, false);
            tab.doc = doc;
        }
    }

    /// Open config.toml as a nibble tab. It is read and written on this machine (not through
    /// nibd, which may be remote); saving it applies the settings straight away.
    fn open_config_tab(&mut self) -> Task<Message> {
        let Some(path) = Config::path() else { return self.flash_warn("no config location (HOME unset)") };
        if let Some(i) = self.tabs.iter().position(|t| t.local && t.doc.path.as_deref() == Some(path.as_path())) {
            self.active = i;
            self.focus = super::Focus::Main;
            return Task::none();
        }
        if !path.exists() {
            if let Err(e) = self.save_now() {
                return self.flash_warn(e);
            }
        }
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => return self.flash_warn(format!("{}: {e}", path.display())),
        };
        let mut doc = Document::from_bytes(&data, Some(path.clone()));
        doc.tab_width = self.tab_width;
        doc.disk_mtime_ns = mtime_ns(&path);
        doc.attach_syntax();
        self.put_doc(doc, true);
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.local = true;
        }
        self.flash_info("config.toml — save to apply. nibble's defaults are built in; this file only holds your changes.")
    }

    /// Save a local tab (config.toml) to disk on this machine. Saving the config reloads it.
    pub(super) fn save_local_tab(&mut self) -> Task<Message> {
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        let Some(path) = tab.doc.path.clone() else { return Task::none() };
        let revision = tab.doc.save_point();
        if let Err(e) = std::fs::write(&path, tab.doc.to_bytes()) {
            return self.flash_warn(format!("{}: {e}", path.display()));
        }
        tab.doc.mark_saved(revision, mtime_ns(&path).unwrap_or(0));
        if Config::path().as_deref() == Some(path.as_path()) {
            return self.reload_config();
        }
        self.flash_info(format!("saved {}", path.display()))
    }

    fn apply_theme(&mut self) -> Vec<String> {
        let (theme, warnings) = Theme::from_config(&self.theme_cfg);
        self.icons = Icons::new(&hex(theme.dim), &hex(theme.accent));
        self.theme = theme;
        warnings
    }

    pub(super) fn open_settings(&mut self, page: Page) -> Task<Message> {
        self.settings.menu = false;
        let mark = self.leave_languages();
        if self.settings.page == Some(page) {
            self.settings.page = None;
            self.settings.recording = None;
            return mark;
        }
        self.settings.page = Some(page);
        // Settings takes the side panel's spot.
        self.git_close();
        self.ssh_close();
        self.apps_close();
        if page == Page::Appearance {
            self.ensure_appearance_data();
        }
        if page == Page::Languages {
            self.settings.langs.added = added_languages();
        }
        mark
    }

    /// On startup, before the user ever saw it: show Settings → Languages.
    pub(super) fn offer_languages(&mut self) {
        let _ = self.open_settings(Page::Languages);
        self.settings.langs.first_run = true;
    }

    /// The Languages page is closing: the first time, remember that it was offered.
    fn leave_languages(&mut self) -> Task<Message> {
        if self.settings.page != Some(Page::Languages) || !std::mem::take(&mut self.settings.langs.first_run) {
            return Task::none();
        }
        Task::perform(
            async {
                std::thread::spawn(|| {
                    let _ = Config::mark_languages_offered();
                });
            },
            |_| s(Msg::Noop),
        )
    }

    fn ensure_appearance_data(&mut self) {
        let s = &mut self.settings;
        if s.fonts.is_empty() {
            s.fonts = installed_monospace_fonts();
        }
        if s.previews.is_empty() {
            s.previews = LANGUAGES
                .iter()
                .map(|l| {
                    let mut d = Document::from_bytes(l.sample.as_bytes(), Some(PathBuf::from(l.sample_path)));
                    d.tab_width = self.tab_width;
                    d.attach_syntax();
                    // Select one identifier so the selection colour is visible too.
                    let word = l.sample_word;
                    if let Some(b) = l.sample.find(word) {
                        let start = l.sample[..b].chars().count();
                        d.set_cursor(start, false);
                        d.set_cursor(start + word.chars().count(), true);
                    }
                    d
                })
                .collect();
        }
        // Default the syntax section to the language being edited.
        if let Some(lang) = self.doc().map(|d| d.language_name()) {
            if let Some(i) = LANGUAGES.iter().position(|l| l.name == lang) {
                self.settings.lang = i;
            }
        }
    }

    pub(super) fn toggle_beginner(&mut self, on: bool) -> Task<Message> {
        self.beginner = on;
        self.schedule_save()
    }

    /// A key pressed while a command waits for its new shortcut. Bare Esc cancels.
    pub(super) fn record_key(&mut self, cmd: Command, chord: KeyChord) -> Task<Message> {
        self.settings.recording = None;
        if chord == KeyChord::new(Key::Esc) {
            return Task::none();
        }
        let prev = self.keymap.bind(chord, cmd);
        let save = self.schedule_save();
        let note = match prev {
            Some(p) => self.flash_warn(format!(
                "{} moved from “{}” to “{}”",
                chord.caps().concat(),
                p.title(),
                cmd.title()
            )),
            None => self.flash_info(format!("{} → {}", chord.caps().concat(), cmd.title())),
        };
        Task::batch([save, note])
    }

    fn color_value(&self, key: ColorKey) -> Color {
        match key {
            ColorKey::Ui(k) => self.theme.get(k).unwrap_or(Color::BLACK),
            ColorKey::Syntax(s) => self.theme.scope(s),
        }
    }

    fn color_is_custom(&self, key: ColorKey) -> bool {
        match key {
            ColorKey::Ui(k) => self.theme_cfg.colors.contains_key(k),
            ColorKey::Syntax(s) => self.theme_cfg.syntax.contains_key(s),
        }
    }

    fn set_color(&mut self, key: ColorKey, c: Option<Color>) {
        let (map, k) = match key {
            ColorKey::Ui(k) => (&mut self.theme_cfg.colors, k),
            ColorKey::Syntax(s) => (&mut self.theme_cfg.syntax, s),
        };
        match c {
            Some(c) => map.insert(k.to_string(), hex(c)),
            None => map.remove(k),
        };
        self.apply_theme();
    }

    pub(super) fn on_settings(&mut self, m: Msg) -> Task<Message> {
        match m {
            Msg::ToggleMenu => self.settings.menu = !self.settings.menu,
            Msg::CloseMenu => self.settings.menu = false,
            Msg::Open(p) => {
                // From the menu, always show (don't toggle closed).
                if self.settings.page != Some(p) {
                    return self.open_settings(p);
                }
                self.settings.menu = false;
            }
            Msg::Close => {
                let mark = self.leave_languages();
                self.settings.page = None;
                self.settings.recording = None;
                self.settings.edit = None;
                return mark;
            }
            Msg::PickLanguage(l) => {
                let p = &mut self.settings.langs.picked;
                if let Some(i) = p.iter().position(|x| *x == l) {
                    p.remove(i);
                } else {
                    p.push(l);
                }
            }
            Msg::AddLanguages => {
                let st = &mut self.settings.langs;
                if st.busy || st.picked.is_empty() {
                    return Task::none();
                }
                st.busy = true;
                st.output = None;
                let langs: Vec<String> = st.picked.iter().map(|l| l.to_string()).collect();
                return Task::perform(add_languages(langs), |r| s(Msg::LanguagesAdded(r)));
            }
            Msg::LanguagesAdded(r) => {
                let st = &mut self.settings.langs;
                st.busy = false;
                st.added = added_languages();
                let added = st.added.clone();
                st.picked.retain(|l| !added.iter().any(|a| a == l));
                st.output = Some(r);
            }
            Msg::Beginner(on) => return self.toggle_beginner(on),
            Msg::FontSize(s) => {
                self.face.size = (s * 2.0).round() / 2.0;
                return self.schedule_save();
            }
            Msg::LineHeight(h) => {
                self.face.line_height = (h * 20.0).round() / 20.0;
                return self.schedule_save();
            }
            Msg::Font(name) => {
                self.font_name = (name != SYSTEM_MONO).then_some(name);
                self.face.font = font_for(self.font_name.as_deref());
                return self.schedule_save();
            }
            Msg::EditColor(key) => {
                if self.settings.edit.as_ref().is_some_and(|e| e.key == key) {
                    self.settings.edit = None;
                } else {
                    let c = self.color_value(key);
                    self.settings.edit = Some(ColorEdit { key, hex: hex(c), hsv: to_hsv(c) });
                }
            }
            Msg::HexInput(s) => {
                let Some(e) = &mut self.settings.edit else { return Task::none() };
                let key = e.key;
                let parsed = parse_hex(&s).or_else(|| parse_hex(&format!("#{}", s.trim())));
                e.hex = s;
                if let Some(c) = parsed {
                    e.hsv = to_hsv(c);
                    self.set_color(key, Some(c));
                    return self.schedule_save();
                }
            }
            Msg::Pick(hsv) => {
                let Some(e) = &mut self.settings.edit else { return Task::none() };
                e.hsv = hsv;
                let c = from_hsv(e.hsv);
                e.hex = hex(c);
                let key = e.key;
                self.set_color(key, Some(c));
                return self.schedule_save();
            }
            Msg::ResetColor(key) => {
                self.set_color(key, None);
                if let Some(e) = self.settings.edit.as_mut().filter(|e| e.key == key) {
                    let c = match key {
                        ColorKey::Ui(k) => self.theme.get(k).unwrap_or(Color::BLACK),
                        ColorKey::Syntax(s) => self.theme.scope(s),
                    };
                    e.hex = hex(c);
                    e.hsv = to_hsv(c);
                }
                return self.schedule_save();
            }
            Msg::ResetAppearance => {
                self.theme_cfg = Default::default();
                self.apply_theme();
                self.settings.edit = None;
                self.font_name = None;
                self.face = Typeface {
                    font: crate::fonts::mono(),
                    size: nib_editor::config::default_font_size(),
                    line_height: nib_editor::config::default_line_height(),
                };
                return self.schedule_save();
            }
            Msg::Lang(i) => {
                self.settings.lang = i.min(LANGUAGES.len() - 1);
                if matches!(self.settings.edit, Some(ColorEdit { key: ColorKey::Syntax(_), .. })) {
                    self.settings.edit = None;
                }
            }
            Msg::Search(s) => self.settings.search = s,
            Msg::Record(c) => {
                self.settings.recording = if self.settings.recording == Some(c) { None } else { Some(c) };
            }
            Msg::Unbind(chord) => {
                self.keymap.unbind(chord);
                return self.schedule_save();
            }
            Msg::ResetCommand(c) => {
                self.keymap.reset(c);
                return self.schedule_save();
            }
            Msg::ResetAllKeys => {
                self.keymap.reset_all();
                self.settings.recording = None;
                return self.schedule_save();
            }
            Msg::OpenConfigFile => return self.open_config_tab(),
            Msg::ReloadConfig => return self.reload_config(),
            Msg::SaveTick(seq) => {
                if seq == self.settings.save_seq {
                    // Written off this thread: a busy disk would otherwise hold up the window.
                    return Task::perform(save_in_order(self.user_settings()), |r| Message::Settings(Msg::Saved(r)));
                }
            }
            Msg::Saved(Ok(path)) => self.refresh_config_tab(&path),
            Msg::Saved(Err(e)) => return self.flash_warn(e),
            Msg::Noop => {}
            Msg::WindowFound(id) => {
                self.settings.window.id = Some(id);
                crate::gpu_log::window_up();
                return Task::batch([
                    window::monitor_size(id).map(|m| s(Msg::Monitor(m))),
                    window::size(id).map(|z| s(Msg::WindowSize(z))),
                    window::mode(id).map(|m| s(Msg::WindowMode(m == window::Mode::Fullscreen))),
                ]);
            }
            Msg::Monitor(m) => self.settings.window.monitor = m,
            Msg::WindowSize(z) => self.settings.window.size = Some(z),
            Msg::WindowMode(f) => self.settings.window.fullscreen = f,
            Msg::Resized(z) => {
                // In fullscreen the size is the screen's; remember the windowed size instead.
                if !self.settings.window.fullscreen && self.settings.window.size != Some(z) {
                    self.settings.window.size = Some(z);
                    return self.schedule_save();
                }
            }
            Msg::Width(w) => self.settings.window.drag_width = Some(w.round()),
            Msg::WidthReleased => {
                if let Some(w) = self.settings.window.drag_width.take() {
                    let h = self.window_size().height;
                    return self.resize_window(Size::new(w, h));
                }
            }
            Msg::Height(h) => {
                let w = self.window_size().width;
                return self.resize_window(Size::new(w, h.round()));
            }
            Msg::Preset(w, h) => return self.resize_window(Size::new(w, h)),
            Msg::Maximize => {
                let Some(id) = self.settings.window.id else { return Task::none() };
                let exit = self.leave_fullscreen(id);
                return exit.chain(window::maximize(id, true));
            }
            Msg::Fullscreen(on) => return self.set_fullscreen(on),
            Msg::ScrollToRevealed => {
                if let Some((_, scope)) = self.settings.revealed {
                    return operate(ScrollIntoView {
                        scroll: WidgetId::from(SCROLL_ID),
                        target: scope_row_id(scope),
                        found: None,
                    });
                }
            }
        }
        Task::none()
    }

    fn window_size(&self) -> Size {
        self.settings.window.size.unwrap_or(Size::new(
            nib_editor::config::default_window_width(),
            nib_editor::config::default_window_height(),
        ))
    }

    fn leave_fullscreen(&mut self, id: window::Id) -> Task<Message> {
        if !self.settings.window.fullscreen {
            return Task::none();
        }
        self.settings.window.fullscreen = false;
        window::set_mode(id, window::Mode::Windowed)
    }

    fn resize_window(&mut self, size: Size) -> Task<Message> {
        let Some(id) = self.settings.window.id else { return Task::none() };
        self.settings.window.size = Some(size);
        let exit = self.leave_fullscreen(id);
        Task::batch([exit.chain(window::resize(id, size)), self.schedule_save()])
    }

    pub(super) fn set_fullscreen(&mut self, on: bool) -> Task<Message> {
        let Some(id) = self.settings.window.id else { return Task::none() };
        self.settings.window.fullscreen = on;
        let mode = if on { window::Mode::Fullscreen } else { window::Mode::Windowed };
        Task::batch([window::set_mode(id, mode), self.schedule_save()])
    }

    /// Run at startup: find the window, then learn its monitor, size and mode.
    pub fn window_probe(fullscreen: bool) -> Task<Message> {
        window::oldest().and_then(move |id| {
            let found = Task::done(s(Msg::WindowFound(id)));
            if fullscreen {
                Task::batch([found, Task::done(s(Msg::Fullscreen(true)))])
            } else {
                found
            }
        })
    }

    /// Settings > Appearance is open and the user clicked a token in the editor: jump to that
    /// token's highlight scope and open its colour picker.
    pub(super) fn reveal_scope_at_cursor(&mut self) -> Task<Message> {
        if self.settings.page != Some(Page::Appearance) {
            return Task::none();
        }
        let Some(doc) = self.doc() else { return Task::none() };
        let t = doc.text.slice(..);
        let pos = doc.selection.primary().head.min(t.len_chars());
        let found = doc.syntax.as_ref().and_then(|syn| {
            let line = t.char_to_line(pos);
            let lo = t.line_to_byte(line);
            let hi = if line + 1 < t.len_lines() { t.line_to_byte(line + 1) } else { t.len_bytes() };
            let spans = syn.spans(&doc.text, lo..hi);
            let byte = t.char_to_byte(pos);
            // The character right of the cursor, else the one left of it (clicked past a word's end).
            [byte, byte.saturating_sub(1)].into_iter().find_map(|b| {
                spans.iter().find(|sp| sp.bytes.contains(&b)).and_then(|sp| {
                    let scope = nib_syntax::scope_name(sp.scope?);
                    let token: String = t.byte_slice(sp.bytes.clone()).to_string();
                    Some((token, scope))
                })
            })
        });
        let lang = doc.language_name();
        let Some((token, scope)) = found else {
            self.settings.revealed = None;
            let c = self.theme.foreground;
            self.settings.edit = Some(ColorEdit { key: ColorKey::Ui("foreground"), hex: hex(c), hsv: to_hsv(c) });
            return self.flash_info("plain text — no highlight scope here; showing the Text colour");
        };
        if let Some(i) = LANGUAGES.iter().position(|l| l.name == lang) {
            self.settings.lang = i;
        }
        let token = token.trim();
        let token = if token.chars().count() > 32 { format!("{}…", token.chars().take(32).collect::<String>()) } else { token.to_string() };
        self.settings.revealed = Some((token, scope));
        let c = self.theme.scope(scope);
        self.settings.edit = Some(ColorEdit { key: ColorKey::Syntax(scope), hex: hex(c), hsv: to_hsv(c) });
        Task::done(s(Msg::ScrollToRevealed))
    }

    fn reload_config(&mut self) -> Task<Message> {
        let cfg = match Config::load() {
            Ok(c) => c,
            Err(e) => return self.flash_warn(e.to_string()),
        };
        let (keymap, mut warnings) = Keymap::with_overrides(&cfg.keys);
        self.keymap = keymap;
        self.theme_cfg = cfg.theme;
        warnings.extend(self.apply_theme());
        self.font_name = cfg.editor.font;
        self.face = Typeface {
            font: font_for(self.font_name.as_deref()),
            size: cfg.editor.font_size,
            line_height: cfg.editor.line_height,
        };
        self.beginner = cfg.editor.beginner_mode;
        self.blame.on = cfg.editor.blame;
        // Tab width: the open files too (what they hold stays as typed).
        self.tab_width = cfg.editor.tab_width;
        for t in &mut self.tabs {
            t.doc.tab_width = self.tab_width;
        }
        // Language servers: the daemon gets them with the next Hello (a connect, another folder).
        let lsp: Vec<nib_proto::LspServer> = cfg
            .lsp
            .iter()
            .map(|(name, s)| nib_proto::LspServer { name: name.clone(), command: s.command.clone(), extensions: s.extensions.clone(), roots: s.roots.clone() })
            .collect();
        let lsp_changed = lsp != self.lsp;
        self.lsp = lsp;
        if self.ai.adopt_switches(&cfg.ai) {
            self.ai_tell_wpf();
        }
        self.ai.retune(cfg.ai.effort, cfg.ai.prompt, cfg.ai.image_zoom_delay);
        let mut window_task = Task::none();
        if let Some(id) = self.settings.window.id {
            let size = Size::new(cfg.window.width, cfg.window.height);
            if !cfg.window.fullscreen && self.settings.window.size != Some(size) {
                self.settings.window.size = Some(size);
                window_task = window::resize(id, size);
            }
            if cfg.window.fullscreen != self.settings.window.fullscreen {
                self.settings.window.fullscreen = cfg.window.fullscreen;
                let mode = if cfg.window.fullscreen { window::Mode::Fullscreen } else { window::Mode::Windowed };
                window_task = window_task.chain(window::set_mode(id, mode));
            }
        }
        self.settings.edit = None;
        self.settings.recording = None;
        // Drop a pending debounced save: it would write back the pre-reload state.
        self.settings.save_seq += 1;
        let note = match warnings.first() {
            Some(w) => {
                let w = w.clone();
                self.flash_warn(w)
            }
            None if lsp_changed => self.flash_info("settings applied from config.toml; [lsp] takes effect on the next connect (renderer: on the next start)"),
            None => self.flash_info("settings applied from config.toml (renderer: on the next start)"),
        };
        Task::batch([window_task, note])
    }
}

fn mtime_ns(path: &std::path::Path) -> Option<u128> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// Finds the widget `target` inside scrollable `scroll`, then chains a scroll that puts it near
/// the top of the viewport.
struct ScrollIntoView {
    scroll: WidgetId,
    target: WidgetId,
    /// (target's y within the scroll content, content top)
    found: Option<f32>,
}

impl<T: 'static> Operation<T> for ScrollIntoView {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&WidgetId>, bounds: Rectangle) {
        if id == Some(&self.target) {
            self.found = Some(bounds.y);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&WidgetId>,
        _bounds: Rectangle,
        _content: Rectangle,
        _translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
    }

    fn finish(&self) -> operation::Outcome<T> {
        match self.found {
            Some(_) => operation::Outcome::Chain(Box::new(ScrollTo { scroll: self.scroll.clone(), target_y: self.found })),
            None => operation::Outcome::None,
        }
    }
}

/// Second pass: now that the target's layout y is known, scroll so it sits 40 px below the top.
struct ScrollTo {
    scroll: WidgetId,
    target_y: Option<f32>,
}

impl<T> Operation<T> for ScrollTo {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&WidgetId>,
        _bounds: Rectangle,
        content: Rectangle,
        _translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&self.scroll) {
            if let Some(y) = self.target_y {
                let y = (y - content.y - 40.0).max(0.0);
                state.scroll_to(operation::scrollable::AbsoluteOffset { x: None, y: Some(y) });
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// view
// ---------------------------------------------------------------------------------------------

fn s(m: Msg) -> Message {
    Message::Settings(m)
}

impl App {
    /// One keycap per key of `chord`.
    pub(super) fn chord_caps<'a>(&self, chord: &KeyChord, accent: bool, dim: bool) -> Element<'a, Message> {
        let th = &self.theme;
        let fg = if dim { th.dim } else { th.foreground };
        let border = if accent { th.accent } else { raise(th.panel, 0.16) };
        let bg = raise(th.panel, 0.07);
        let caps = chord.caps().into_iter().map(|c| {
            container(text(c).size(11).color(fg))
                .padding([1, 6])
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    border: Border { color: border, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .into()
        });
        row(caps).spacing(3).align_y(Alignment::Center).into()
    }

    fn flat_button<'a>(&self, label: impl Into<Element<'a, Message>>, msg: Message) -> button::Button<'a, Message> {
        let hover = raise(self.theme.panel, 0.08);
        button(label).on_press(msg).padding([3, 8]).style(move |_, st| button::Style {
            background: matches!(st, button::Status::Hovered).then(|| hover.into()),
            border: Border { radius: 4.0.into(), ..Default::default() },
            ..Default::default()
        })
    }

    fn small_button<'a>(&self, label: &str, msg: Message) -> Element<'a, Message> {
        let th = &self.theme;
        let border = raise(th.panel, 0.16);
        let hover = raise(th.panel, 0.08);
        button(text(label.to_string()).size(11).color(th.foreground))
            .on_press(msg)
            .padding([3, 10])
            .style(move |_, st| button::Style {
                background: matches!(st, button::Status::Hovered).then(|| hover.into()),
                border: Border { color: border, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            })
            .into()
    }

    fn icon_button<'a>(&self, glyph: &str, tip: &str, msg: Message) -> Element<'a, Message> {
        let th = &self.theme;
        let b = self.flat_button(text(glyph.to_string()).size(12).color(th.dim), msg).padding([1, 6]);
        let bg = raise(th.panel, 0.1);
        tooltip(
            b,
            container(text(tip.to_string()).size(11).color(th.foreground))
                .padding([3, 6])
                .style(move |_| container::Style::default().background(bg)),
            tooltip::Position::Top,
        )
        .into()
    }

    fn section<'a>(&self, title: impl Into<String>) -> Element<'a, Message> {
        let th = &self.theme;
        container(text(title.into().to_uppercase()).size(10).color(th.dim))
            .padding(Padding { top: 14.0, bottom: 4.0, left: 0.0, right: 0.0 })
            .into()
    }

    // --- gear menu -------------------------------------------------------------------------

    /// Overlay `base` with the gear menu (and a click-outside catcher) when it's open.
    pub(super) fn with_settings_menu<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        if !self.settings.menu {
            return base;
        }
        let th = &self.theme;
        let item = |label: &str, cmd: Command, msg: Message| {
            let mut r = row![text(label.to_string()).size(12).color(th.foreground), space::horizontal()]
                .align_y(Alignment::Center)
                .spacing(12);
            if let Some(k) = self.keymap.chords_for(cmd).first() {
                r = r.push(self.chord_caps(k, false, true));
            }
            self.flat_button(r, msg).padding([5, 10]).width(Length::Fill)
        };
        let check = if self.beginner { "☑" } else { "☐" };
        let beginner = self
            .flat_button(
                row![
                    text(check).size(13).color(if self.beginner { th.accent } else { th.dim }),
                    text("Beginner mode").size(12).color(th.foreground),
                    space::horizontal(),
                    text("cheat sheet").size(10).color(th.dim),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                s(Msg::Beginner(!self.beginner)),
            )
            .padding([5, 10])
            .width(Length::Fill);
        let (panel, border) = (th.panel, raise(th.panel, 0.16));
        let card = container(
            column![
                container(text("SETTINGS").size(10).color(th.dim)).padding([4, 10]),
                item("Appearance", Command::OpenSettings, s(Msg::Open(Page::Appearance))),
                item("Keyboard Shortcuts", Command::KeyboardShortcuts, s(Msg::Open(Page::Shortcuts))),
                item("Languages", Command::OpenLanguages, s(Msg::Open(Page::Languages))),
                container(space()).height(1).width(Length::Fill).style(move |_| container::Style::default().background(border)),
                beginner,
            ]
            .spacing(2),
        )
        .padding(4)
        .width(250)
        .style(move |_| container::Style {
            background: Some(panel.into()),
            border: Border { color: border, width: 1.0, radius: 6.0.into() },
            shadow: iced::Shadow { color: Color { a: 0.5, ..Color::BLACK }, offset: iced::Vector::new(0.0, 4.0), blur_radius: 16.0 },
            ..Default::default()
        });
        // Sits just above the gear (the 4th strip button).
        let placed = container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Start)
            .align_y(Alignment::End)
            .padding(Padding { bottom: 36.0, left: 8.0 + 3.0 * 34.0, top: 0.0, right: 0.0 });
        let catcher = mouse_area(container(space()).width(Length::Fill).height(Length::Fill))
            .on_press(s(Msg::CloseMenu))
            .interaction(mouse::Interaction::Idle);
        stack![base, catcher, placed].into()
    }

    // --- panel -----------------------------------------------------------------------------

    pub(super) fn settings_panel(&self) -> Option<Element<'_, Message>> {
        let page = self.settings.page?;
        let th = &self.theme;
        let tab = |label: &'static str, p: Page| {
            let on = page == p;
            let fg = if on { th.foreground } else { th.dim };
            let line = if on { th.accent } else { Color::TRANSPARENT };
            column![
                button(text(label).size(12).color(fg))
                    .on_press(s(Msg::Open(p)))
                    .padding([6, 10])
                    .style(|_, _| button::Style::default()),
                container(space()).height(2).width(Length::Fill).style(move |_| container::Style::default().background(line)),
            ]
            .width(Length::Shrink)
        };
        let header = row![
            tab("Appearance", Page::Appearance),
            tab("Keyboard Shortcuts", Page::Shortcuts),
            tab("Languages", Page::Languages),
            space::horizontal(),
            self.icon_button("✕", "Close settings", s(Msg::Close)),
        ]
        .align_y(Alignment::Center)
        .padding(Padding { left: 8.0, right: 8.0, top: 4.0, bottom: 0.0 });
        let body = match page {
            Page::Appearance => self.appearance_page(),
            Page::Shortcuts => self.shortcuts_page(),
            Page::Languages => self.languages_page(),
        };
        let path = Config::path().map(|p| p.display().to_string()).unwrap_or_default();
        let footer = row![
            toggler(self.beginner)
                .label("Beginner mode")
                .on_toggle(|b| s(Msg::Beginner(b)))
                .size(16)
                .text_size(12),
            space::horizontal(),
            tooltip(
                text("config.toml").size(10).color(th.dim),
                container(text(path).size(11).color(th.foreground)).padding([3, 6]).style({
                    let bg = raise(th.panel, 0.1);
                    move |_| container::Style::default().background(bg)
                }),
                tooltip::Position::Top,
            ),
        ]
        .align_y(Alignment::Center)
        .padding([6, 12]);
        let width = if page == Page::Shortcuts { 520 } else { 440 };
        let panel = th.panel;
        Some(
            container(column![
                header,
                self.hrule(),
                scrollable(container(body).padding(Padding { left: 14.0, right: 16.0, top: 0.0, bottom: 14.0 }))
                    .id(SCROLL_ID)
                    .style(crate::theme::scrollbar)
                    .height(Length::Fill),
                self.hrule(),
                footer
            ])
            .width(width)
            .height(Length::Fill)
            .style(move |_| container::Style::default().background(panel))
            .into(),
        )
    }

    fn labeled<'a>(&self, label: &str, control: impl Into<Element<'a, Message>>, value: String) -> Element<'a, Message> {
        let th = &self.theme;
        row![
            text(label.to_string()).size(12).color(th.foreground).width(96),
            container(control).width(Length::Fill),
            text(value).size(11).color(th.dim).width(52).align_x(Alignment::End),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    }

    fn window_section(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let w = &self.settings.window;
        let size = self.window_size();
        let screen = w.monitor.unwrap_or(Size::new(3840.0, 2160.0));
        let mut col = column![].spacing(6);
        if w.fullscreen {
            col = col.push(text("Fullscreen — turn it off to set a window size.").size(11).color(th.dim));
        } else {
            let max_w = screen.width.max(size.width).max(640.0);
            let max_h = screen.height.max(size.height).max(400.0);
            col = col.push(self.labeled(
                "Width",
                {
                    let width = w.drag_width.unwrap_or(size.width);
                    slider(480.0..=max_w, width.clamp(480.0, max_w), |v| s(Msg::Width(v)))
                        .step(10.0_f32)
                        .on_release(s(Msg::WidthReleased))
                },
                match w.drag_width {
                    Some(d) => format!("{d:.0}"),
                    None => format!("{:.0}", size.width),
                },
            ));
            col = col.push(self.labeled(
                "Height",
                slider(320.0..=max_h, size.height.clamp(320.0, max_h), |v| s(Msg::Height(v))).step(10.0_f32),
                format!("{:.0}", size.height),
            ));
            let mut presets = row![text("Presets").size(12).color(th.foreground).width(96)]
                .spacing(4)
                .align_y(Alignment::Center);
            for (name, pw, ph) in PRESETS.iter().filter(|(_, pw, ph)| *pw <= screen.width && *ph <= screen.height) {
                let on = (size.width - pw).abs() < 1.0 && (size.height - ph).abs() < 1.0;
                presets = presets.push(self.chip(
                    column![
                        text(*name).size(11).color(if on { th.foreground } else { th.dim }),
                        text(format!("{pw:.0}×{ph:.0}")).size(9).color(th.dim),
                    ]
                    .align_x(Alignment::Center),
                    on,
                    s(Msg::Preset(*pw, *ph)),
                ));
            }
            presets = presets.push(self.chip(
                column![text("Maximize").size(11).color(th.dim), text("fill screen").size(9).color(th.dim)]
                    .align_x(Alignment::Center),
                false,
                s(Msg::Maximize),
            ));
            col = col.push(scrollable(presets).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(4).scroller_width(4),
            )).style(crate::theme::scrollbar));
        }
        let fs_key = self
            .keymap
            .chords_for(Command::ToggleFullscreen)
            .first()
            .map(|k| self.chord_caps(k, false, true))
            .unwrap_or_else(|| space().into());
        col = col.push(
            row![
                text("Fullscreen").size(12).color(th.foreground).width(96),
                toggler(w.fullscreen).on_toggle(|b| s(Msg::Fullscreen(b))).size(16),
                space::horizontal(),
                fs_key,
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        col.into()
    }

    /// A rounded toggle-style button; `on` draws it with the accent border.
    fn chip<'a>(&self, label: impl Into<Element<'a, Message>>, on: bool, msg: Message) -> Element<'a, Message> {
        let th = &self.theme;
        let bd = if on { th.accent } else { raise(th.panel, 0.16) };
        let hover = raise(th.panel, 0.08);
        button(label)
            .on_press(msg)
            .padding([3, 8])
            .style(move |_, st| button::Style {
                background: matches!(st, button::Status::Hovered).then(|| hover.into()),
                border: Border { color: bd, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            })
            .into()
    }

    fn appearance_page(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let st = &self.settings;
        let mut col = column![].spacing(6);

        // Window
        col = col.push(self.section("Window"));
        col = col.push(self.window_section());

        // Font
        col = col.push(self.section("Font"));
        let mut fonts = vec![SYSTEM_MONO.to_string()];
        fonts.extend(st.fonts.iter().cloned());
        let selected = self.font_name.clone().unwrap_or_else(|| SYSTEM_MONO.into());
        col = col.push(self.labeled(
            "Family",
            pick_list(fonts, Some(selected), |f| s(Msg::Font(f))).text_size(12).padding([4, 8]).width(Length::Fill),
            String::new(),
        ));
        col = col.push(self.labeled(
            "Size",
            slider(8.0..=32.0, self.face.size, |v| s(Msg::FontSize(v))).step(0.5_f32),
            format!("{} px", self.face.size),
        ));
        col = col.push(self.labeled(
            "Line height",
            slider(1.0..=2.2, self.face.line_height, |v| s(Msg::LineHeight(v))).step(0.05_f32),
            format!("{:.2}×", self.face.line_height),
        ));

        // Preview + language
        let lang = &LANGUAGES[st.lang.min(LANGUAGES.len() - 1)];
        let mut langs = row![text("Language").size(12).color(th.foreground).width(96)].spacing(4).align_y(Alignment::Center);
        for (i, l) in LANGUAGES.iter().enumerate() {
            let on = i == st.lang;
            let (fg, bd) = if on { (th.foreground, th.accent) } else { (th.dim, raise(th.panel, 0.16)) };
            langs = langs.push(
                button(text(l.name).size(11).color(fg))
                    .on_press(s(Msg::Lang(i)))
                    .padding([3, 10])
                    .style(move |_, _| button::Style {
                        border: Border { color: bd, width: 1.0, radius: 10.0.into() },
                        ..Default::default()
                    }),
            );
        }
        col = col.push(self.section("Preview"));
        col = col.push(langs.wrap());
        if let Some(doc) = st.previews.get(st.lang) {
            let h = doc.text.len_lines() as f32 * self.face.size * self.face.line_height + 4.0;
            let border = th.divider;
            col = col.push(
                container(EditorView::new(doc, th, 0, 0, self.face, |_| s(Msg::Noop)))
                    .height(h)
                    .width(Length::Fill)
                    .clip(true)
                    .style(move |_| container::Style {
                        border: Border { color: border, width: 1.0, radius: 4.0.into() },
                        ..Default::default()
                    }),
            );
        }

        // Syntax colours for this language's grammar
        col = col.push(self.section(format!("Syntax colours · {}", lang.name)));
        col = col.push(
            text("Scopes this language's grammar produces. A colour set on a scope also applies to its sub-scopes unless they have their own. Tip: click a word in your code to jump to its scope.")
                .size(11)
                .color(th.dim),
        );
        let mut scopes = lang.scopes();
        if let Some((_, sc)) = st.revealed {
            // A token from an embedded language may use a scope this grammar doesn't list.
            if !scopes.contains(&sc) {
                scopes.push(sc);
            }
        }
        for scope in scopes {
            col = col.push(container(self.color_row(ColorKey::Syntax(scope))).id(scope_row_id(scope)));
        }

        // UI colours by group
        let mut group = "";
        for u in UI_COLORS {
            if u.group != group {
                group = u.group;
                col = col.push(self.section(group));
            }
            col = col.push(self.color_row(ColorKey::Ui(u.key)));
        }

        col = col.push(container(self.small_button("Reset appearance to defaults", s(Msg::ResetAppearance))).padding(Padding {
            top: 16.0,
            ..Padding::ZERO
        }));
        col.into()
    }

    fn color_row(&self, key: ColorKey) -> Element<'_, Message> {
        let th = &self.theme;
        let c = self.color_value(key);
        let custom = self.color_is_custom(key);
        let border = raise(th.panel, 0.25);
        let swatch = container(space()).width(16).height(16).style(move |_| container::Style {
            background: Some(c.into()),
            border: Border { color: border, width: 1.0, radius: 3.0.into() },
            ..Default::default()
        });
        let label: Element<'_, Message> = match key {
            ColorKey::Ui(k) => {
                let l = UI_COLORS.iter().find(|u| u.key == k).map_or(k, |u| u.label);
                text(l).size(12).color(th.foreground).into()
            }
            ColorKey::Syntax(scope) => {
                // Sub-scopes indent under their parent; the name is drawn in its own colour.
                let depth = scope.matches('.').count() as f32;
                row![space().width(depth * 12.0), text(scope).size(12).color(c)].into()
            }
        };
        let dot = text(if custom { "●" } else { "" }).size(8).color(th.accent);
        let revealed = match (key, &self.settings.revealed) {
            (ColorKey::Syntax(sc), Some((tok, r))) if sc == *r => Some(tok.clone()),
            _ => None,
        };
        let mut content = row![swatch, label, space::horizontal()].spacing(10).align_y(Alignment::Center);
        if let Some(tok) = &revealed {
            content = content.push(text(format!("“{tok}”")).size(11).color(th.foreground));
        }
        content = content.push(dot).push(text(hex(c)).size(11).color(th.dim));
        let accent = th.accent;
        let hover = raise(th.panel, 0.08);
        let line = button(content)
            .on_press(s(Msg::EditColor(key)))
            .padding([3, 6])
            .width(Length::Fill)
            .style(move |_, st| button::Style {
                background: matches!(st, button::Status::Hovered).then(|| hover.into()),
                border: Border {
                    color: if revealed.is_some() { accent } else { Color::TRANSPARENT },
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            });
        let Some(e) = self.settings.edit.as_ref().filter(|e| e.key == key) else { return line.into() };

        let default = match key {
            ColorKey::Ui(k) => UI_COLORS.iter().find(|u| u.key == k).map(|u| u.default).unwrap_or(Color::BLACK),
            ColorKey::Syntax(sc) => self.theme.default_scope(sc),
        };
        let mut top = row![
            text_input("#rrggbb", &e.hex).on_input(|v| s(Msg::HexInput(v))).size(12).padding([3, 6]).width(96),
            text(format!("default {}", hex(default))).size(11).color(th.dim),
            space::horizontal(),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if custom {
            top = top.push(self.small_button("Reset", s(Msg::ResetColor(key))));
        }
        let bg = raise(th.panel, 0.04);
        let editor = container(column![ColorPicker::new(e.hsv, |hsv| s(Msg::Pick(hsv))), top].spacing(10))
            .padding([8, 10])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            });
        column![line, editor].spacing(2).into()
    }

    fn languages_page(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let st = &self.settings.langs;
        let mut col = column![
            self.section("Languages"),
            text(format!(
                "nib highlights {} out of the box. Pick the languages you work in to add them:",
                nib_syntax::BUILTIN_LANGUAGES.join(", ")
            ))
            .size(12)
            .color(th.foreground),
        ]
        .spacing(4);
        for &l in crate::grammar_cli::OFFERED {
            let added = st.added.iter().any(|a| a == l);
            let picked = st.picked.contains(&l);
            let (glyph, color) = match (added, picked) {
                (true, _) => ("✓", th.accent),
                (false, true) => ("☑", th.accent),
                (false, false) => ("☐", th.dim),
            };
            let r = row![
                text(glyph).size(13).color(color).width(16),
                text(l).size(12).color(th.foreground),
                space::horizontal(),
                text(if added { "added" } else { "" }).size(10).color(th.dim),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            let b = self.flat_button(r, s(Msg::PickLanguage(l))).padding([5, 10]).width(Length::Fill);
            col = col.push(if added || st.busy { b.on_press_maybe(None) } else { b });
        }
        let add: Element<'_, Message> = if st.busy {
            text("Adding…").size(11).color(th.dim).into()
        } else if st.picked.is_empty() {
            text("").size(11).into()
        } else {
            self.small_button(&format!("Add {}", st.picked.join(", ")), s(Msg::AddLanguages))
        };
        col = col.push(container(add).padding(Padding { top: 8.0, ..Padding::ZERO }));
        if let Some(out) = &st.output {
            let (msg, color) = match out {
                Ok(o) => (o.as_str(), th.dim),
                Err(e) => (e.as_str(), th.warn),
            };
            col = col.push(container(text(msg.trim().to_string()).size(11).color(color)).padding(Padding { top: 6.0, ..Padding::ZERO }));
            if out.is_ok() {
                col = col.push(text("Open a new nib window to use them.").size(12).color(th.foreground));
            }
        }
        col = col.push(self.section("Other languages"));
        col = col.push(
            text("Add any other language with `nib grammar add <language>`; `nib grammar list --available` lists them, `nib grammar rm` takes one away.")
                .size(12)
                .color(th.dim),
        );
        col.into()
    }

    fn shortcuts_page(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let st = &self.settings;
        let q = st.search.trim().to_lowercase();
        let matches = |c: Command| {
            q.is_empty()
                || c.title().to_lowercase().contains(&q)
                || c.name().contains(&q)
                || c.category().to_lowercase().contains(&q)
                || self.keymap.chords_for(c).iter().any(|k| {
                    k.to_string().to_lowercase().contains(&q) || k.caps().concat().to_lowercase().contains(&q)
                })
        };

        let mut col = column![].spacing(2);
        col = col.push(
            container(
                text_input("Search commands or keys…", &st.search)
                    .on_input(|v| s(Msg::Search(v)))
                    .size(13)
                    .padding([6, 10]),
            )
            .padding(Padding { top: 12.0, bottom: 4.0, ..Padding::ZERO }),
        );
        if let Some(c) = st.recording {
            let accent = th.accent;
            col = col.push(
                container(
                    text(format!("Press the new shortcut for “{}” — Esc cancels", c.title())).size(12).color(th.foreground),
                )
                .padding([6, 10])
                .width(Length::Fill)
                .style(move |_| container::Style {
                    border: Border { color: accent, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                }),
            );
        }

        let custom: Vec<Command> =
            Command::ALL.iter().copied().filter(|c| self.keymap.is_customized(*c) && matches(*c)).collect();
        if !custom.is_empty() {
            col = col.push(
                row![self.section(format!("Your shortcuts · {}", custom.len())), space::horizontal(), {
                    let b: Element<'_, Message> = self.small_button("Reset all", s(Msg::ResetAllKeys));
                    container(b).padding(Padding { top: 10.0, ..Padding::ZERO })
                }]
                .align_y(Alignment::Center),
            );
            for c in &custom {
                col = col.push(self.shortcut_row(*c));
            }
        }

        col = col.push(self.section("nibble defaults"));
        let mut any = false;
        for (cat, cmds) in by_category() {
            let cmds: Vec<Command> = cmds.into_iter().filter(|c| !self.keymap.is_customized(*c) && matches(*c)).collect();
            if cmds.is_empty() {
                continue;
            }
            any = true;
            col = col.push(container(text(cat).size(11).color(th.accent)).padding(Padding { top: 8.0, bottom: 2.0, ..Padding::ZERO }));
            for c in cmds {
                col = col.push(self.shortcut_row(c));
            }
        }
        if !any && custom.is_empty() {
            col = col.push(text(format!("No commands match “{}”.", st.search.trim())).size(12).color(th.dim));
        }

        col = col.push(self.section("File"));
        col = col.push(
            text("Your changes are saved to [keys] in config.toml; nibble's defaults are built in. Edit the file by hand, then Reload.")
                .size(11)
                .color(th.dim),
        );
        col = col.push(
            row![
                self.small_button("Open config file", s(Msg::OpenConfigFile)),
                self.small_button("Reload", s(Msg::ReloadConfig)),
            ]
            .spacing(8),
        );
        col.into()
    }

    fn shortcut_row(&self, c: Command) -> Element<'_, Message> {
        let th = &self.theme;
        let recording = self.settings.recording == Some(c);
        let customized = self.keymap.is_customized(c);

        let title = column![
            text(c.title()).size(13).color(th.foreground),
            text(c.name()).size(10).color(th.dim),
        ]
        .spacing(1)
        .width(Length::Fill);

        let mut keys = row![].spacing(6).align_y(Alignment::Center);
        if recording {
            let accent = th.accent;
            keys = keys.push(container(text("Recording…").size(11).color(th.accent)).padding([2, 8]).style(move |_| {
                container::Style { border: Border { color: accent, width: 1.0, radius: 4.0.into() }, ..Default::default() }
            }));
        } else {
            let bindings = self.keymap.bindings_for(c);
            if bindings.is_empty() {
                keys = keys.push(text("Unassigned").size(11).color(th.dim));
            }
            for (k, origin) in bindings {
                keys = keys.push(
                    row![
                        self.chord_caps(&k, origin == Origin::User, false),
                        self.icon_button("×", "Remove this shortcut", s(Msg::Unbind(k))),
                    ]
                    .spacing(0)
                    .align_y(Alignment::Center),
                );
            }
            for k in self.keymap.removed_defaults(c) {
                keys = keys.push(tooltip(
                    text(format!("was {}", k.caps().concat())).size(10).color(th.dim),
                    container(text("nibble default, removed by you").size(11).color(th.foreground)).padding([3, 6]).style({
                        let bg = raise(th.panel, 0.1);
                        move |_| container::Style::default().background(bg)
                    }),
                    tooltip::Position::Top,
                ));
            }
        }
        let mut actions = row![self.icon_button(
            if recording { "esc" } else { "+" },
            if recording { "Cancel" } else { "Add a shortcut (press the keys)" },
            s(Msg::Record(c))
        )]
        .align_y(Alignment::Center);
        if customized {
            actions = actions.push(self.icon_button("↺", "Reset to nibble default", s(Msg::ResetCommand(c))));
        }

        let (bg, border) = if recording { (raise(th.panel, 0.05), th.accent) } else { (Color::TRANSPARENT, Color::TRANSPARENT) };
        container(row![title, keys, actions].spacing(10).align_y(Alignment::Center))
            .padding([5, 8])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: Border { color: border, width: 1.0, radius: 5.0.into() },
                ..Default::default()
            })
            .into()
    }

    // --- beginner pane ---------------------------------------------------------------------

    pub(super) fn beginner_pane(&self) -> Element<'_, Message> {
        let th = &self.theme;
        let mut col = column![].spacing(3);
        for (cat, cmds) in by_category() {
            let cmds: Vec<_> = cmds.into_iter().filter_map(|c| self.keymap.chords_for(c).first().map(|k| (c, *k))).collect();
            if cmds.is_empty() {
                continue;
            }
            col = col.push(container(text(cat.to_uppercase()).size(10).color(th.accent)).padding(Padding {
                top: 10.0,
                bottom: 2.0,
                ..Padding::ZERO
            }));
            for (c, k) in cmds {
                col = col.push(
                    row![text(c.title()).size(12).color(th.foreground).width(Length::Fill), self.chord_caps(&k, false, false)]
                        .spacing(8)
                        .align_y(Alignment::Center),
                );
            }
        }
        let panel = th.panel;
        container(column![
            container(
                row![
                    text("Shortcuts").size(13).color(th.foreground),
                    space::horizontal(),
                    self.icon_button("⚙", "Edit shortcuts", s(Msg::Open(Page::Shortcuts))),
                    self.icon_button("✕", "Turn off Beginner mode", s(Msg::Beginner(false))),
                ]
                .align_y(Alignment::Center)
            )
            .padding(Padding { top: 8.0, bottom: 4.0, left: 12.0, right: 8.0 }),
            self.hrule(),
            scrollable(container(col).padding(Padding { left: 12.0, right: 14.0, top: 0.0, bottom: 12.0 })).style(crate::theme::scrollbar).height(Length::Fill),
        ])
        .width(300)
        .height(Length::Fill)
        .style(move |_| container::Style::default().background(panel))
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_category_slot() {
        let n: usize = by_category().iter().map(|(_, v)| v.len()).sum();
        assert_eq!(n, Command::ALL.len());
    }
}

/// Write `settings` into config.toml on the settings writer, a thread of its own that takes
/// saves in the order they come, so a later save is never overwritten by an earlier one.
/// The languages `nib grammar add` turned on (`[languages]` in config.toml).
fn added_languages() -> Vec<String> {
    Config::load().map(|c| c.languages.into_keys().collect()).unwrap_or_default()
}

/// Run `nib grammar add LANGS…` (this nib's own binary) off the UI thread: its output, Ok when it
/// succeeded.
fn add_languages(langs: Vec<String>) -> impl std::future::Future<Output = Result<String, String>> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let r = crate::app_bundle::exe()
            .and_then(|nib| std::process::Command::new(nib).arg("grammar").arg("add").args(&langs).output())
            .map_err(|e| format!("couldn't run nib grammar add: {e}"))
            .and_then(|o| {
                let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                if o.status.success() { Ok(text) } else { Err(text) }
            });
        let _ = tx.send(r);
    });
    async move { rx.await.unwrap_or_else(|_| Err("nib grammar add stopped".into())) }
}

fn save_in_order(settings: UserSettings) -> impl std::future::Future<Output = Result<PathBuf, String>> {
    type Job = (UserSettings, iced::futures::channel::oneshot::Sender<Result<PathBuf, String>>);
    static WRITER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = std::sync::OnceLock::new();
    let writer = WRITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            let keys: Vec<&str> = UI_COLORS.iter().map(|u| u.key).collect();
            for (settings, reply) in rx {
                let _ = reply.send(Config::save_settings(&settings, &keys).map_err(|e| format!("couldn't save settings: {e}")));
            }
        });
        std::sync::Mutex::new(tx)
    });
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    let sent = writer.lock().unwrap().send((settings, tx)).is_ok();
    async move {
        if !sent {
            return Err("couldn't save settings: the settings writer is gone".to_string());
        }
        rx.await.unwrap_or_else(|_| Err("couldn't save settings: the settings writer is gone".to_string()))
    }
}
