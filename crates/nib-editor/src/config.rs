//! Single TOML config file. nibble's defaults live in code (the `default_*` functions and the
//! client's theme table); the file only holds what the user changed and is applied over them at
//! startup and on reload. The settings UI writes back only values that differ from the defaults.
//! Location: `$NIB_CONFIG`, else `$XDG_CONFIG_HOME/nib/config.toml`,
//! else `~/.config/nib/config.toml` (also on macOS), `%APPDATA%\nib\config.toml` on Windows.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub editor: EditorConfig,
    /// `nib @name` targets.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// chord -> command name (or "none").
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    /// Colours as "#rrggbb". Unset keys use the built-in defaults.
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default)]
    pub window: WindowConfig,
    /// `[lsp.<name>]`: language servers, applied over nibd's built-in ones.
    #[serde(default)]
    pub lsp: BTreeMap<String, LspServerConfig>,
    #[serde(default)]
    pub ai: AiConfig,
    /// `[languages.<name>]`: grammars loaded at runtime (written by `nib grammar add`).
    #[serde(default)]
    pub languages: BTreeMap<String, LanguageEntry>,
}

/// `[languages.<name>]`: a language whose grammar is a shared library in `grammars/` beside
/// config.toml, with queries in `queries/<name>/`. A built-in name (python, bash, json, yaml,
/// toml, markdown, html) replaces the built-in.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageEntry {
    /// `grammars/<grammar>.{dylib,so,dll}` and its `tree_sitter_<grammar>` symbol. Default: the name.
    pub grammar: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Whole file names (`Makefile`).
    #[serde(default)]
    pub filenames: Vec<String>,
    /// Interpreters on a `#!` line.
    #[serde(default)]
    pub shebangs: Vec<String>,
    /// Other names a Markdown fence or an injection may use (`js`, `rs`).
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// `[ai]`: the AI panel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiConfig {
    /// List OpenCode's cloud models too. Off: only the ones whose weights are on the machine
    /// with the code (pulled with Ollama).
    #[serde(default)]
    pub cloud_models: bool,
    /// The assistant last picked (`claude`, `codex`, `grok`, `opencode`): new agents start with
    /// it. Written by nib when a model is picked.
    pub agent: Option<String>,
    /// The model last picked for `agent`; unset: the one the assistant is set up with.
    pub model: Option<String>,
    /// A new agent opens the assistant / model dropdown. Off: it starts with the last one.
    #[serde(default = "yes")]
    pub pick_model: bool,
    /// The thinking level last used (`high`, …): new agents start with it when their assistant
    /// has that level. Unset: the assistant's own. Written by nib when an agent starts.
    pub effort: Option<String>,
    /// The system prompt last used: new agents start with it. Written by nib when an agent
    /// starts.
    pub prompt: Option<String>,
    /// Seconds the mouse rests on a picture in the chat before it is shown four times bigger.
    #[serde(default = "default_image_zoom_delay")]
    pub image_zoom_delay: f32,
    /// Worktree-per-feature: an agent that changes files does it on a branch of its own
    /// that lands on yours as one commit (`nib-work`), and is held to it. Off: agents change the
    /// project folder and nothing is committed for them. The WPF switch in the agents tab.
    #[serde(default = "yes")]
    pub worktree_per_feature: bool,
}

fn yes() -> bool {
    true
}

pub fn default_image_zoom_delay() -> f32 {
    1.0
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            cloud_models: false,
            agent: None,
            model: None,
            pick_model: true,
            effort: None,
            prompt: None,
            image_zoom_delay: default_image_zoom_delay(),
            worktree_per_feature: true,
        }
    }
}

/// `[lsp.<name>]`: a language server. They run inside nibd, on the machine with the code. nibd
/// has built-in ones (`python`: ty, `rust`: rust-analyzer, `go`: gopls, `typescript`:
/// typescript-language-server, `c`: clangd); an entry with one of those names changes only the
/// fields it sets, any other name adds a server (then `command` and `extensions` are needed).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    /// argv, e.g. `["rust-analyzer"]`; a bare name is also looked up in the project's `.venv`
    /// and `node_modules/.bin`, `~/.local/bin`, `~/.cargo/bin`, `~/go/bin`. `[]` turns it off.
    pub command: Option<Vec<String>>,
    /// File extensions it serves, without the dot. Taken away from any other server.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Files marking a project root; the topmost directory holding one is the server's root.
    #[serde(default)]
    pub roots: Vec<String>,
}

/// `[window]`: initial size (logical pixels) and fullscreen.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    #[serde(default = "default_window_width")]
    pub width: f32,
    #[serde(default = "default_window_height")]
    pub height: f32,
    #[serde(default)]
    pub fullscreen: bool,
}

pub fn default_window_width() -> f32 {
    1200.0
}

pub fn default_window_height() -> f32 {
    800.0
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { width: default_window_width(), height: default_window_height(), fullscreen: false }
    }
}

/// `[theme]`: UI colour key -> "#rrggbb" (keys are listed in the client's theme table; unknown
/// keys are reported there), plus `[theme.syntax]`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ThemeConfig {
    /// Syntax scope -> colour, e.g. `keyword = "#569cd6"`, `"string.regexp" = "#d16969"`.
    #[serde(default)]
    pub syntax: BTreeMap<String, String>,
    #[serde(flatten)]
    pub colors: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorConfig {
    #[serde(default = "default_tab")]
    pub tab_width: usize,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// "auto" (GPU, falling back to CPU; on WSL nib finds the GPU setup once and caches it, see
    /// `nib/src/gpu_pick.rs`), "cpu" (tiny-skia; use on GPU-less VMs), "gpu" (wgpu).
    #[serde(default = "default_renderer")]
    pub renderer: String,
    /// Code font family (e.g. "JetBrains Mono"). Unset = the system monospace font.
    #[serde(default)]
    pub font: Option<String>,
    /// Line height as a multiple of the font size.
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// Show the shortcut cheat-sheet pane.
    #[serde(default)]
    pub beginner_mode: bool,
    /// Who last committed the selected lines, when and in which commit: dim, after the
    /// selection's last line.
    #[serde(default = "default_true")]
    pub blame: bool,
}

fn default_true() -> bool {
    true
}

pub fn default_line_height() -> f32 {
    1.4
}

pub fn default_font_size() -> f32 {
    14.0
}

fn default_renderer() -> String {
    "auto".into()
}

fn default_tab() -> usize {
    4
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_width: default_tab(),
            font_size: default_font_size(),
            renderer: default_renderer(),
            font: None,
            line_height: default_line_height(),
            beginner_mode: false,
            blame: true,
        }
    }
}

/// `[profiles.<name>]`: a place nib can open, as `nib @name` or from the SSH panel. The SSH panel
/// and `nib ctl ssh …` write these back with `Config::save_profile` / `Config::remove_profile`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Host alias from ~/.ssh/config, a hostname, or user@host.
    pub ssh: Option<String>,
    /// WSL distro name (Windows only).
    pub wsl: Option<String>,
    /// Project root on the target machine. `~` allowed.
    pub root: String,
    /// Use this nibd on the target instead of uploading a bundled one.
    pub nibd: Option<String>,
    /// ssh only: login name (`ssh -l`). Leave unset to use ~/.ssh/config or `user@` in `ssh`.
    pub user: Option<String>,
    /// ssh only: port (`ssh -p`).
    pub port: Option<u16>,
    /// ssh only: private key file (`ssh -i`).
    pub identity: Option<String>,
    /// ssh only: jump host(s) (`ssh -J`), e.g. `bastion` or `me@bastion:2222`.
    pub jump: Option<String>,
    /// Folders opened as the root on this machine, most recent first (`root` is the first).
    /// The SSH panel offers them when connecting again.
    #[serde(default)]
    pub roots: Vec<String>,
}

/// Roots remembered per profile.
pub const MAX_ROOTS: usize = 8;

impl Profile {
    /// Make `root` the current root and the first remembered one.
    pub fn use_root(&mut self, root: &str) {
        // A profile written by hand has only `root`: that one was used too.
        if self.roots.is_empty() && !is_home(&self.root) {
            self.roots.push(self.root.clone());
        }
        self.root = root.to_owned();
        // Home is where every connection starts, not a project: never remembered.
        self.roots.retain(|r| r != root && !is_home(r));
        if !is_home(root) {
            self.roots.insert(0, root.to_owned());
        }
        self.roots.truncate(MAX_ROOTS);
    }

    /// The project folders used here, most recent first. Home only when there is nothing else.
    pub fn all_roots(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for r in std::iter::once(&self.root).chain(&self.roots) {
            if !is_home(r) && !v.contains(r) {
                v.push(r.clone());
            }
        }
        if v.is_empty() {
            v.push(self.root.clone());
        }
        v
    }
}

/// `~` (or `~/`): the home folder, which is no project.
fn is_home(root: &str) -> bool {
    matches!(root.trim_end_matches('/'), "~" | "")
}

/// How to reach an ssh host: what `ssh` gets on its command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshHost {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<String>,
    pub jump: Option<String>,
}

impl SshHost {
    /// The options before the host on ssh's command line. Values never start with `-`
    /// (`Profile::check`), so none of them can be read as another option.
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        let mut push = |flag: &str, v: &Option<String>| {
            if let Some(v) = v {
                a.push(flag.to_owned());
                a.push(v.clone());
            }
        };
        push("-l", &self.user);
        push("-p", &self.port.map(|p| p.to_string()));
        push("-i", &self.identity);
        push("-J", &self.jump);
        a
    }

    /// `user@host:port`, as people write it.
    pub fn display(&self) -> String {
        let user = self.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("{user}{}{port}", self.host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Local {
        nibd: Option<String>,
    },
    Ssh {
        ssh: SshHost,
        nibd: Option<String>,
    },
    Wsl {
        distro: String,
        nibd: Option<String>,
    },
}

/// A profile name: what follows `@` on the command line and the key under `[profiles]`.
pub fn check_profile_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '.'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        anyhow::bail!("profile name {name:?}: use letters, digits, '-', '_' or '.' (not first), up to 64");
    }
    Ok(())
}

/// A value that ends up as one argument of `ssh`: one word that can't pass for an option.
fn check_word(field: &str, v: &str) -> anyhow::Result<()> {
    if v.is_empty() {
        anyhow::bail!("{field} is empty");
    }
    if v.starts_with('-') {
        anyhow::bail!("{field} {v:?} can't start with '-'");
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("{field} {v:?} can't contain spaces or control characters");
    }
    Ok(())
}

/// A path: may contain spaces, but not line breaks, and can't pass for an option.
fn check_path(field: &str, v: &str) -> anyhow::Result<()> {
    if v.trim().is_empty() {
        anyhow::bail!("{field} is empty");
    }
    if v.starts_with('-') {
        anyhow::bail!("{field} {v:?} can't start with '-'");
    }
    if v.chars().any(char::is_control) {
        anyhow::bail!("{field} can't contain control characters");
    }
    Ok(())
}

impl Profile {
    pub fn target(&self) -> anyhow::Result<Target> {
        self.check()?;
        Ok(match (&self.ssh, &self.wsl) {
            (Some(h), _) => Target::Ssh {
                ssh: SshHost {
                    host: h.clone(),
                    user: self.user.clone(),
                    port: self.port,
                    identity: self.identity.clone(),
                    jump: self.jump.clone(),
                },
                nibd: self.nibd.clone(),
            },
            (None, Some(d)) => Target::Wsl {
                distro: d.clone(),
                nibd: self.nibd.clone(),
            },
            (None, None) => Target::Local {
                nibd: self.nibd.clone(),
            },
        })
    }

    /// "ssh", "wsl" or "local".
    pub fn kind(&self) -> &'static str {
        match (&self.ssh, &self.wsl) {
            (Some(_), _) => "ssh",
            (None, Some(_)) => "wsl",
            (None, None) => "local",
        }
    }

    /// Everything a profile has to be before nib runs anything with it. Profiles also come from
    /// agents (`nib ctl ssh add`), so nothing in here may become an extra ssh option.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.ssh.is_some() && self.wsl.is_some() {
            anyhow::bail!("a profile sets `ssh` or `wsl`, not both");
        }
        if self.ssh.is_none() {
            for (k, set) in [("user", self.user.is_some()), ("port", self.port.is_some()), ("identity", self.identity.is_some()), ("jump", self.jump.is_some())] {
                if set {
                    anyhow::bail!("`{k}` only applies to ssh profiles");
                }
            }
        }
        if let Some(h) = &self.ssh {
            check_word("host", h)?;
        }
        if let Some(d) = &self.wsl {
            check_word("wsl distro", d)?;
        }
        if let Some(u) = &self.user {
            check_word("user", u)?;
        }
        if let Some(j) = &self.jump {
            check_word("jump", j)?;
        }
        if self.port == Some(0) {
            anyhow::bail!("port 0 is not a port");
        }
        if let Some(i) = &self.identity {
            check_path("identity", i)?;
        }
        if let Some(n) = &self.nibd {
            check_path("nibd", n)?;
        }
        for r in &self.roots {
            check_path("roots", r)?;
        }
        check_path("root", &self.root)
    }

    /// Write this profile into `t`. Keys this profile doesn't set are removed; a key whose value
    /// changes keeps its trailing comment.
    fn write_into(&self, t: &mut toml_edit::Table) {
        use toml_edit::Value;
        let fields: [(&str, Option<Value>); 9] = [
            ("ssh", self.ssh.as_deref().map(Value::from)),
            ("wsl", self.wsl.as_deref().map(Value::from)),
            ("user", self.user.as_deref().map(Value::from)),
            ("port", self.port.map(|p| Value::from(i64::from(p)))),
            ("identity", self.identity.as_deref().map(Value::from)),
            ("jump", self.jump.as_deref().map(Value::from)),
            ("root", Some(Value::from(self.root.as_str()))),
            ("nibd", self.nibd.as_deref().map(Value::from)),
            ("roots", (self.roots.len() > 1 || self.roots.first().is_some_and(|r| *r != self.root)).then(|| Value::from_iter(self.roots.iter().map(String::as_str)))),
        ];
        for (k, v) in fields {
            match (v, t.get_mut(k).and_then(|i| i.as_value_mut())) {
                (None, _) => {
                    t.remove(k);
                }
                (Some(v), Some(old)) => {
                    if old.to_string().trim() != v.to_string().trim() {
                        let decor = old.decor().clone();
                        *old = v;
                        *old.decor_mut() = decor;
                    }
                }
                (Some(v), None) => t[k] = toml_edit::value(v),
            }
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("NIB_CONFIG") {
            return Some(PathBuf::from(p));
        }
        if cfg!(windows) {
            return std::env::var_os("APPDATA")
                .map(|a| PathBuf::from(a).join("nib").join("config.toml"));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("nib").join("config.toml"))
    }

    /// Missing file = defaults. Parse errors are returned (don't silently ignore a typo).
    pub fn load() -> anyhow::Result<Self> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }
}

/// Everything the settings UI edits. Written back into config.toml with `Config::save_settings`.
#[derive(Debug, Clone)]
pub struct UserSettings {
    pub font_size: f32,
    pub font: Option<String>,
    pub line_height: f32,
    pub beginner_mode: bool,
    pub window: WindowConfig,
    pub theme: ThemeConfig,
    /// `[keys]` entries: chord -> command name or "none".
    pub keys: Vec<(String, String)>,
}

/// Set `doc[table][key]`, or remove it when `v` is None (the built-in default applies).
/// The table is only created when there is something to put in it.
fn set_or_default(doc: &mut toml_edit::DocumentMut, tbl: &str, key: &str, v: Option<toml_edit::Item>) {
    match v {
        Some(v) => table(doc, tbl)[key] = v,
        None => {
            if let Some(t) = doc.get_mut(tbl).and_then(|t| t.as_table_mut()) {
                t.remove(key);
            }
        }
    }
}

fn num(f: f32) -> toml_edit::Item {
    let r = (f as f64 * 100.0).round() / 100.0;
    if r.fract() == 0.0 {
        toml_edit::value(r as i64)
    } else {
        toml_edit::value(r)
    }
}

/// `Some(num(v))` unless `v` equals the default (compared at the precision written).
fn non_default(v: f32, default: f32) -> Option<toml_edit::Item> {
    ((v * 100.0).round() != (default * 100.0).round()).then(|| num(v))
}

impl UserSettings {
    /// Update `doc` in place. Only the keys the settings UI owns are touched, so comments,
    /// profiles and unrelated settings survive. Values equal to nibble's defaults are removed
    /// rather than written, so changing a default in code reaches users who never touched it.
    pub fn apply_to(&self, doc: &mut toml_edit::DocumentMut, ui_colors: &[&str]) {
        use toml_edit::{value, Item, Table};

        set_or_default(doc, "editor", "font_size", non_default(self.font_size, default_font_size()));
        set_or_default(doc, "editor", "line_height", non_default(self.line_height, default_line_height()));
        set_or_default(doc, "editor", "font", self.font.as_deref().map(value));
        set_or_default(doc, "editor", "beginner_mode", self.beginner_mode.then(|| value(true)));
        set_or_default(doc, "window", "width", non_default(self.window.width, default_window_width()));
        set_or_default(doc, "window", "height", non_default(self.window.height, default_window_height()));
        set_or_default(doc, "window", "fullscreen", self.window.fullscreen.then(|| value(true)));

        for k in ui_colors {
            set_or_default(doc, "theme", k, self.theme.colors.get(*k).map(|v| value(v.as_str())));
        }
        if self.theme.syntax.is_empty() {
            if let Some(t) = doc.get_mut("theme").and_then(|t| t.as_table_mut()) {
                t.remove("syntax");
            }
        } else {
            let theme = table(doc, "theme");
            let mut syn = Table::new();
            for (k, v) in &self.theme.syntax {
                syn[k.as_str()] = value(v.as_str());
            }
            // Keep the comment block above an existing [theme.syntax] header.
            if let Some(old) = theme.get("syntax").and_then(Item::as_table) {
                *syn.decor_mut() = old.decor().clone();
            }
            theme["syntax"] = Item::Table(syn);
        }

        if !self.keys.is_empty() || doc.contains_key("keys") {
            let keys = table(doc, "keys");
            keys.clear();
            for (k, c) in &self.keys {
                keys[k.as_str()] = value(c.as_str());
            }
        }
    }
}

/// The table at `doc[name]`, created if missing.
fn table<'a>(doc: &'a mut toml_edit::DocumentMut, name: &str) -> &'a mut toml_edit::Table {
    if !doc.as_table().get(name).is_some_and(|i| i.is_table()) {
        doc[name] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc[name].as_table_mut().expect("just ensured")
}

/// Add or replace `[profiles.<name>]` in `doc`. `previous`: the name it had (a rename keeps the
/// old table's place and comments). Refuses to overwrite a different profile already called `name`.
pub fn put_profile(doc: &mut toml_edit::DocumentMut, name: &str, p: &Profile, previous: Option<&str>) -> anyhow::Result<()> {
    check_profile_name(name)?;
    p.check()?;
    let profiles = table(doc, "profiles");
    if !profiles.contains_key("ssh") && profiles.iter().all(|(_, v)| v.is_table()) {
        // Only `[profiles.x]` headers, never a bare `[profiles]` one.
        profiles.set_implicit(true);
    }
    let from = previous.unwrap_or(name);
    if from != name && profiles.contains_key(name) {
        anyhow::bail!("a profile called {name:?} already exists");
    }
    let mut t = match profiles.remove(from) {
        Some(toml_edit::Item::Table(t)) => t,
        Some(_) => anyhow::bail!("[profiles] {from:?} is not a table"),
        None if previous.is_some() => anyhow::bail!("no profile called {from:?}"),
        None => {
            let mut t = toml_edit::Table::new();
            // New profiles go after the existing ones rather than at the end of the file.
            let last = profiles.iter().filter_map(|(_, v)| v.as_table()?.position()).max();
            if let Some(pos) = last {
                t.set_position(pos);
            }
            t
        }
    };
    p.write_into(&mut t);
    profiles.insert(name, toml_edit::Item::Table(t));
    Ok(())
}

/// Remove `[profiles.<name>]` from `doc`.
pub fn delete_profile(doc: &mut toml_edit::DocumentMut, name: &str) -> anyhow::Result<()> {
    let removed = doc.get_mut("profiles").and_then(|p| p.as_table_mut()).and_then(|p| p.remove(name));
    if removed.is_none() {
        anyhow::bail!("no profile called {name:?}");
    }
    Ok(())
}

impl Config {
    /// Read config.toml, let `f` change it, and write it back (created if missing), preserving
    /// everything `f` didn't touch. Atomic: a crash mid-write never leaves a truncated config.
    fn edit(f: impl FnOnce(&mut toml_edit::DocumentMut) -> anyhow::Result<()>) -> anyhow::Result<PathBuf> {
        // One read-modify-write at a time: the settings, the AI panel's choices and profiles are
        // saved from threads of their own, and two at once would lose one's change.
        static EDITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = EDITING.lock().unwrap_or_else(|e| e.into_inner());
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config location (HOME unset)"))?;
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => anyhow::bail!("{}: {e}", path.display()),
        };
        let mut doc: toml_edit::DocumentMut =
            src.parse().map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        f(&mut doc)?;
        let out = doc.to_string();
        // Never write a file nib itself couldn't load.
        toml::from_str::<Config>(&out).map_err(|e| anyhow::anyhow!("refusing to write an invalid config: {e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Add or replace `[languages.<name>]` (`nib grammar add`).
    pub fn save_language(name: &str, l: &LanguageEntry) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| {
            let langs = table(doc, "languages");
            langs.set_implicit(true);
            let mut t = toml_edit::Table::new();
            if let Some(g) = l.grammar.as_deref().filter(|g| *g != name) {
                t["grammar"] = toml_edit::value(g);
            }
            for (k, v) in [
                ("extensions", &l.extensions),
                ("filenames", &l.filenames),
                ("shebangs", &l.shebangs),
                ("aliases", &l.aliases),
            ] {
                if !v.is_empty() {
                    t[k] = toml_edit::value(v.iter().collect::<toml_edit::Array>());
                }
            }
            langs.insert(name, toml_edit::Item::Table(t));
            Ok(())
        })
    }

    /// Remove `[languages.<name>]` (`nib grammar remove`).
    pub fn remove_language(name: &str) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| {
            let removed = doc.get_mut("languages").and_then(|p| p.as_table_mut()).and_then(|p| p.remove(name));
            if removed.is_none() {
                anyhow::bail!("no [languages.{name}] in the config");
            }
            Ok(())
        })
    }

    /// Write the settings UI's state into config.toml.
    pub fn save_settings(s: &UserSettings, ui_colors: &[&str]) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| {
            s.apply_to(doc, ui_colors);
            Ok(())
        })
    }

    /// Add or replace a profile in config.toml (see `put_profile`).
    pub fn save_profile(name: &str, p: &Profile, previous: Option<&str>) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| put_profile(doc, name, p, previous))
    }

    /// Remove a profile from config.toml.
    pub fn remove_profile(name: &str) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| delete_profile(doc, name))
    }

    /// Write the AI panel's remembered choice into `[ai]`: the assistant and model last picked,
    /// and whether a new agent asks for one (only written when off, the default being on).
    /// `pick`: `None` leaves the assistant and model as they are.
    pub fn save_ai_choice(pick: Option<(&str, Option<&str>)>, pick_model: bool) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| {
            if let Some((agent, model)) = pick {
                set_or_default(doc, "ai", "agent", Some(toml_edit::value(agent)));
                set_or_default(doc, "ai", "model", model.map(toml_edit::value));
            }
            set_or_default(doc, "ai", "pick_model", (!pick_model).then(|| toml_edit::value(false)));
            Ok(())
        })
    }

    /// Write `[ai] worktree_per_feature` (only when off, the default being on).
    pub fn save_ai_wpf(on: bool) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| {
            set_or_default(doc, "ai", "worktree_per_feature", (!on).then(|| toml_edit::value(false)));
            Ok(())
        })
    }

    /// Write the thinking level and system prompt an agent last started with into `[ai]`
    /// (`None`: removed).
    pub fn save_ai_tuning(effort: Option<&str>, prompt: Option<&str>) -> anyhow::Result<PathBuf> {
        Self::edit(|doc| {
            set_or_default(doc, "ai", "effort", effort.map(toml_edit::value));
            set_or_default(doc, "ai", "prompt", prompt.map(toml_edit::value));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config.toml as a user's grows: comments, saved connections, then the rest. (The shipped
    /// example has every line commented out, so it can't stand in for one.)
    const LIVED_IN: &str = r##"# my nib config
[editor]
tab_width = 4
font_size = 14

[window]
# width = 1200

# `nib @name [path]` opens a profile. Paths are relative to `root` on the target machine.
[profiles.gpu]
ssh = "gpu-box"          # Host alias from ~/.ssh/config (ProxyJump etc. are honored)
root = "~/projects/seg"

[profiles.ubuntu]
wsl = "Ubuntu-24.04"     # Windows only
root = "~/code"

[theme]
# background = "#000000"

[ai]
# cloud_models = true

[keys]
# "mod+y" = "none"
"##;

    #[test]
    fn parse_example() {
        // The shipped example parses, and copying it as is changes nothing: no connections.
        let shipped: Config = toml::from_str(include_str!("../../../config/config.example.toml")).unwrap();
        assert!(shipped.profiles.is_empty(), "the example adds no connections");
        assert!(shipped.ai.worktree_per_feature && shipped.ai.pick_model, "defaults, untouched");
        let c: Config = toml::from_str(LIVED_IN).unwrap();
        assert_eq!(
            c.profiles["gpu"].target().unwrap(),
            Target::Ssh {
                ssh: SshHost { host: "gpu-box".into(), ..Default::default() },
                nibd: None
            }
        );
        assert!(matches!(
            c.profiles["ubuntu"].target().unwrap(),
            Target::Wsl { .. }
        ));
        assert!(!c.ai.cloud_models, "only downloaded models unless asked");
        let on: Config = toml::from_str("[ai]\ncloud_models = true").unwrap();
        assert!(on.ai.cloud_models);
        assert!(c.ai.pick_model, "a new agent asks for its model unless told not to");
        let last: Config = toml::from_str("[ai]\nagent = \"codex\"\nmodel = \"gpt-6\"\npick_model = false").unwrap();
        assert_eq!((last.ai.agent.as_deref(), last.ai.model.as_deref(), last.ai.pick_model), (Some("codex"), Some("gpt-6"), false));
        assert_eq!(c.ai.image_zoom_delay, 1.0);
        let quick: Config = toml::from_str("[ai]\nimage_zoom_delay = 0.3").unwrap();
        assert_eq!(quick.ai.image_zoom_delay, 0.3);
    }

    #[test]
    fn settings_round_trip_keeps_comments() {
        let mut doc: toml_edit::DocumentMut = LIVED_IN.parse().unwrap();
        let mut theme = ThemeConfig::default();
        theme.colors.insert("background".into(), "#101010".into());
        theme.syntax.insert("string.regexp".into(), "#ff0000".into());
        let s = UserSettings {
            font_size: 15.5,
            font: Some("Menlo".into()),
            line_height: 1.5,
            beginner_mode: true,
            window: WindowConfig { width: 1440.0, ..Default::default() },
            theme,
            keys: vec![("cmd+d".into(), "duplicate_line".into()), ("cmd+y".into(), "none".into())],
        };
        s.apply_to(&mut doc, &["background", "foreground"]);
        let out = doc.to_string();
        assert!(out.contains("# `nib @name [path]` opens a profile"), "comments kept");
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(c.editor.font_size, 15.5);
        assert_eq!(c.editor.font.as_deref(), Some("Menlo"));
        assert!(c.editor.beginner_mode);
        assert_eq!(c.theme.colors["background"], "#101010");
        assert_eq!(c.theme.syntax["string.regexp"], "#ff0000");
        assert_eq!(c.keys["cmd+y"], "none");
        assert!(c.profiles.contains_key("gpu"));

        // Clearing everything removes the keys again.
        let empty = UserSettings { theme: ThemeConfig::default(), keys: vec![], font: None, ..s };
        empty.apply_to(&mut doc, &["background", "foreground"]);
        let c: Config = toml::from_str(&doc.to_string()).unwrap();
        assert!(c.theme.colors.is_empty() && c.theme.syntax.is_empty() && c.keys.is_empty());
        assert!(c.editor.font.is_none());
    }

    fn ssh(host: &str) -> Profile {
        Profile { ssh: Some(host.into()), root: "~/code".into(), ..Default::default() }
    }

    #[test]
    fn profiles_are_written_beside_the_others_keeping_comments() {
        let mut doc: toml_edit::DocumentMut = LIVED_IN.parse().unwrap();
        let lab = Profile { user: Some("ada".into()), port: Some(2222), ..ssh("10.0.0.7") };
        put_profile(&mut doc, "lab", &lab, None).unwrap();
        // Changing a value keeps the comment after it.
        let gpu = Profile { root: "~/projects/other".into(), ..ssh("gpu-box") };
        put_profile(&mut doc, "gpu", &gpu, None).unwrap();
        let out = doc.to_string();
        assert!(out.contains("ssh = \"gpu-box\"          # Host alias"), "comment kept:\n{out}");
        assert!(!out.contains("[profiles]\n"), "no bare [profiles] header:\n{out}");
        assert!(out.find("\n[profiles.lab]").unwrap() < out.find("\n[theme]").unwrap(), "next to the other profiles:\n{out}");
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(c.profiles["lab"], lab);
        assert_eq!(c.profiles["gpu"].root, "~/projects/other");

        // Rename, then delete.
        put_profile(&mut doc, "lab2", &lab, Some("lab")).unwrap();
        assert!(put_profile(&mut doc, "gpu", &lab, Some("lab2")).is_err(), "won't overwrite another profile");
        delete_profile(&mut doc, "lab2").unwrap();
        let c: Config = toml::from_str(&doc.to_string()).unwrap();
        assert!(!c.profiles.contains_key("lab") && !c.profiles.contains_key("lab2"));
        assert!(delete_profile(&mut doc, "nope").is_err());

        // Into an empty file.
        let mut empty = toml_edit::DocumentMut::new();
        put_profile(&mut empty, "a", &ssh("h"), None).unwrap();
        assert_eq!(empty.to_string(), "[profiles.a]\nssh = \"h\"\nroot = \"~/code\"\n");
    }

    #[test]
    fn nothing_from_a_profile_can_become_an_ssh_option() {
        for bad in [
            ssh("-oProxyCommand=touch /tmp/x"),
            ssh("host name"),
            Profile { user: Some("-oX=y".into()), ..ssh("h") },
            Profile { jump: Some("-J".into()), ..ssh("h") },
            Profile { identity: Some("-i".into()), ..ssh("h") },
            Profile { port: Some(0), ..ssh("h") },
            Profile { root: "".into(), ..ssh("h") },
            Profile { user: Some("me".into()), ..Default::default() },
            Profile { wsl: Some("Ubuntu".into()), ..ssh("h") },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        for name in ["", "-x", ".x", "a b", "a/b"] {
            assert!(check_profile_name(name).is_err(), "{name:?}");
        }
        let ok = Profile { identity: Some("~/.ssh/my key".into()), jump: Some("me@bastion:22".into()), ..ssh("ada@gpu") };
        assert!(ok.check().is_ok());
        let Target::Ssh { ssh, .. } = Profile { port: Some(2222), ..ok.clone() }.target().unwrap() else { panic!() };
        assert_eq!(ssh.args(), ["-p", "2222", "-i", "~/.ssh/my key", "-J", "me@bastion:22"]);
    }

    #[test]
    fn roots_are_remembered_most_recent_first() {
        let mut p = ssh("h"); // root ~/code, written by hand
        p.use_root("~/a");
        p.use_root("~/b");
        p.use_root("~/a");
        assert_eq!((p.root.as_str(), p.all_roots()), ("~/a", vec!["~/a".to_owned(), "~/b".into(), "~/code".into()]));
        // Home is never remembered, but a host with nothing else still offers it.
        p.use_root("~");
        assert_eq!((p.root.as_str(), p.all_roots()), ("~", vec!["~/a".to_owned(), "~/b".into(), "~/code".into()]));
        assert!(!p.roots.contains(&"~".to_owned()));
        let mut home = ssh("h");
        home.root = "~".into();
        home.use_root("~");
        assert_eq!(home.all_roots(), ["~"]);
        let mut doc = toml_edit::DocumentMut::new();
        put_profile(&mut doc, "h", &p, None).unwrap();
        let c: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(c.profiles["h"], p);
        // A single root needs no list.
        let mut q = ssh("h");
        q.use_root("~/code");
        put_profile(&mut doc, "q", &q, None).unwrap();
        assert!(!doc.to_string().split("[profiles.q]").nth(1).unwrap().contains("roots"));
    }

    #[test]
    fn defaults_are_not_written() {
        let s = UserSettings {
            font_size: default_font_size(),
            font: None,
            line_height: default_line_height(),
            beginner_mode: false,
            window: WindowConfig::default(),
            theme: ThemeConfig::default(),
            keys: vec![],
        };
        let mut doc = toml_edit::DocumentMut::new();
        s.apply_to(&mut doc, &["background"]);
        assert_eq!(doc.to_string().trim(), "", "nothing but defaults -> empty file");

        // A user value is written; going back to the default removes it again.
        let mut big = s.clone();
        big.font_size = 18.0;
        big.window.fullscreen = true;
        big.apply_to(&mut doc, &["background"]);
        let c: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(c.editor.font_size, 18.0);
        assert!(c.window.fullscreen);
        assert_eq!(c.window.width, default_window_width());
        s.apply_to(&mut doc, &["background"]);
        assert!(!doc.to_string().contains("font_size") && !doc.to_string().contains("fullscreen"));
    }
}
