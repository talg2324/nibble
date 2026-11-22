//! Chord -> Command table. Defaults follow VSCode (D-024): `mod` = Cmd on macOS, Ctrl elsewhere.
//! User overrides come from `[keys]` in config.toml, written by hand or by the shortcut editor
//! (Settings > Keyboard Shortcuts, D-009). The keymap keeps defaults and overrides apart so the UI
//! can show which bindings are nibble's and which are the user's.

use crate::Command;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    Enter,
    Backspace,
    Delete,
    Insert,
    Esc,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS, Win/Super elsewhere.
    pub logo: bool,
    pub key: Key,
}

impl KeyChord {
    pub const fn new(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
            key,
        }
    }

    /// Parse `ctrl+shift+z`, `cmd+s`, `mod+s`, `alt+left`, `f5`, `ctrl+/`. Case-insensitive.
    /// `mod` = Cmd on macOS, Ctrl elsewhere.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        let mut parts: Vec<&str> = s.split('+').collect();
        // "ctrl++" -> the key is '+'
        if s.ends_with("++") {
            parts.pop();
            *parts.last_mut().unwrap() = "+";
        }
        let key_s = parts.pop().ok_or("empty chord")?;
        let mut c = KeyChord::new(parse_key(key_s)?);
        for m in parts {
            match m {
                "ctrl" | "control" | "c" => c.ctrl = true,
                "alt" | "option" | "opt" | "a" => c.alt = true,
                "shift" | "s" => c.shift = true,
                "cmd" | "command" | "super" | "win" | "logo" | "meta" => c.logo = true,
                "mod" => {
                    if cfg!(target_os = "macos") {
                        c.logo = true
                    } else {
                        c.ctrl = true
                    }
                }
                other => return Err(format!("unknown modifier {other:?} in {s:?}")),
            }
        }
        Ok(c.normalized())
    }

    /// Canonical form: letters lowercase, shift kept as a modifier.
    pub fn normalized(mut self) -> Self {
        if let Key::Char(ch) = self.key {
            if ch.is_ascii_uppercase() {
                self.shift = true;
                self.key = Key::Char(ch.to_ascii_lowercase());
            }
        }
        self
    }

    /// Canonical `[keys]` spelling, e.g. `cmd+shift+k`; `KeyChord::parse` round-trips it.
    pub fn to_config(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("ctrl+");
        }
        if self.alt {
            s.push_str("alt+");
        }
        if self.shift {
            s.push_str("shift+");
        }
        if self.logo {
            s.push_str(if cfg!(target_os = "macos") { "cmd+" } else { "super+" });
        }
        let key = match self.key {
            Key::Char(' ') => "space".into(),
            Key::Char('+') => "plus".into(),
            Key::Char('-') => "minus".into(),
            Key::Char(c) => c.to_string(),
            Key::F(n) => format!("f{n}"),
            k => format!("{k:?}").to_ascii_lowercase(),
        };
        s.push_str(&key);
        s
    }

    /// One label per physical key, for drawing keycaps: `["⌘", "⇧", "K"]` on macOS,
    /// `["Ctrl", "Shift", "K"]` elsewhere. Modifiers in platform order.
    pub fn caps(&self) -> Vec<String> {
        let mac = cfg!(target_os = "macos");
        let mut v: Vec<String> = Vec::new();
        let m = |on: bool, mac_s: &str, other: &str, v: &mut Vec<String>| {
            if on {
                v.push(if mac { mac_s } else { other }.into());
            }
        };
        if mac {
            m(self.ctrl, "⌃", "Ctrl", &mut v);
            m(self.alt, "⌥", "Alt", &mut v);
            m(self.shift, "⇧", "Shift", &mut v);
            m(self.logo, "⌘", "Super", &mut v);
        } else {
            m(self.logo, "⌘", "Super", &mut v);
            m(self.ctrl, "⌃", "Ctrl", &mut v);
            m(self.alt, "⌥", "Alt", &mut v);
            m(self.shift, "⇧", "Shift", &mut v);
        }
        v.push(match self.key {
            Key::Char(' ') => "Space".into(),
            Key::Char(c) => c.to_ascii_uppercase().to_string(),
            Key::F(n) => format!("F{n}"),
            Key::Up => "↑".into(),
            Key::Down => "↓".into(),
            Key::Left => "←".into(),
            Key::Right => "→".into(),
            Key::Enter => if mac { "↩" } else { "Enter" }.into(),
            Key::Tab => if mac { "⇥" } else { "Tab" }.into(),
            Key::Backspace => if mac { "⌫" } else { "Backspace" }.into(),
            Key::Delete => if mac { "⌦" } else { "Del" }.into(),
            Key::Esc => "Esc".into(),
            Key::PageUp => "PgUp".into(),
            Key::PageDown => "PgDn".into(),
            k => format!("{k:?}"),
        });
        v
    }
}

fn parse_key(s: &str) -> Result<Key, String> {
    Ok(match s {
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" | "pgdown" => Key::PageDown,
        "tab" => Key::Tab,
        "enter" | "return" | "ret" => Key::Enter,
        "backspace" | "bs" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "esc" | "escape" => Key::Esc,
        "space" => Key::Char(' '),
        "minus" => Key::Char('-'),
        "plus" => Key::Char('+'),
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            Key::F(f[1..].parse().unwrap())
        }
        c if c.chars().count() == 1 => Key::Char(c.chars().next().unwrap()),
        other => return Err(format!("unknown key {other:?}")),
    })
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.logo {
            f.write_str(if cfg!(target_os = "macos") {
                "Cmd+"
            } else {
                "Super+"
            })?;
        }
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        match self.key {
            Key::Char(' ') => f.write_str("Space"),
            Key::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            Key::F(n) => write!(f, "F{n}"),
            k => write!(f, "{k:?}"),
        }
    }
}

/// Where an active binding comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Default,
    User,
}

pub struct Keymap {
    /// nibble's built-in bindings.
    defaults: HashMap<KeyChord, Command>,
    /// User bindings in file order: `None` unbinds a default chord.
    overrides: Vec<(KeyChord, Option<Command>)>,
    /// Effective table (defaults overlaid with overrides).
    map: HashMap<KeyChord, Command>,
}

/// (chord, command). Several chords may map to one command.
pub fn default_bindings() -> Vec<(&'static str, Command)> {
    use Command::*;
    let mut v = vec![
        ("mod+s", Save),
        ("mod+q", Quit),
        ("mod+z", Undo),
        ("mod+shift+z", Redo),
        ("mod+y", Redo),
        ("mod+c", Copy),
        ("mod+x", Cut),
        ("mod+v", Paste),
        ("mod+a", SelectAll),
        ("mod+b", ToggleExplorer),
        ("mod+o", OpenFolder),
        ("mod+shift+o", ToggleExplorer),
        ("mod+enter", OpenInNewTab),
        ("ctrl+tab", NextTab),
        ("ctrl+shift+tab", PrevTab),
        ("f6", NextPane),
        ("shift+f6", PrevPane),
        ("mod+w", CloseTab),
        ("mod+shift+t", ReopenClosedTab),
        ("mod+1", GoToTab1),
        ("mod+2", GoToTab2),
        ("mod+3", GoToTab3),
        ("mod+4", GoToTab4),
        ("mod+5", GoToTab5),
        ("mod+6", GoToTab6),
        ("mod+7", GoToTab7),
        ("mod+8", GoToTab8),
        ("mod+9", GoToTab9),
        ("mod+=", FontBigger),
        ("mod+shift+=", FontBigger), // Mod+"+" on US keyboards
        ("mod++", FontBigger),       // numpad +
        ("mod+-", FontSmaller),
        ("f12", GoToDefinition),
        ("shift+f12", FindReferences),
        ("f2", RenameSymbol),
        ("mod+f2", ChangeAllOccurrences),
        ("ctrl+`", NewTerminal),
        ("ctrl+shift+`", NewTerminal), // "Ctrl+~" on US keyboards
        ("mod+/", ToggleComment),
        ("mod+p", TogglePreview), // ⌘⇧V is commonly taken by clipboard managers
        ("mod+shift+k", DeleteLine),
        ("alt+shift+down", DuplicateLine),
        ("tab", Indent),
        ("shift+tab", Dedent),
        ("mod+]", Indent),
        ("mod+[", Dedent),
        ("enter", Newline),
        ("shift+enter", Newline),
        ("backspace", DeleteBackward),
        ("shift+backspace", DeleteBackward),
        ("delete", DeleteForward),
        ("left", MoveLeft),
        ("right", MoveRight),
        ("up", MoveUp),
        ("down", MoveDown),
        ("home", MoveLineStart),
        ("end", MoveLineEnd),
        ("pageup", PageUp),
        ("pagedown", PageDown),
        ("shift+left", SelectLeft),
        ("shift+right", SelectRight),
        ("shift+up", SelectUp),
        ("shift+down", SelectDown),
        ("shift+home", SelectLineStart),
        ("shift+end", SelectLineEnd),
        ("shift+pageup", SelectPageUp),
        ("shift+pagedown", SelectPageDown),
        ("esc", CollapseSelection),
        ("mod+,", OpenSettings),
        ("mod+shift+g", ToggleGit),
        ("mod+shift+d", ToggleDiff),
        ("mod+shift+i", ToggleAi),
        ("mod+shift+]", NextAgent),
        ("mod+shift+[", PrevAgent),
        ("mod+shift+n", NewAgent),
        ("mod+shift+h", AgentHistory),
        ("mod+shift+backspace", KillAgent),
        ("ctrl+.", Panic),
        ("mod+shift+r", ToggleSsh), // R for remote
        ("mod+f", Find),
    ];
    if cfg!(target_os = "macos") {
        v.extend([
            ("alt+left", MoveWordLeft),
            ("alt+right", MoveWordRight),
            ("alt+shift+left", SelectWordLeft),
            ("alt+shift+right", SelectWordRight),
            ("cmd+left", MoveLineStart),
            ("cmd+right", MoveLineEnd),
            ("cmd+shift+left", SelectLineStart),
            ("cmd+shift+right", SelectLineEnd),
            ("cmd+up", MoveDocStart),
            ("cmd+down", MoveDocEnd),
            ("cmd+shift+up", SelectDocStart),
            ("cmd+shift+down", SelectDocEnd),
            ("alt+backspace", DeleteWordBackward),
            ("ctrl+cmd+f", ToggleFullscreen),
            // Elsewhere Ctrl+B is the file browser (mod+b): Ctrl+` then B does this on every OS.
            ("ctrl+b", FocusToolbar),
            // Alt+Tab belongs to the window manager elsewhere.
            ("alt+tab", NextPane),
            ("alt+shift+tab", PrevPane),
            ("ctrl+-", NavigateBack),
            ("ctrl+shift+-", NavigateForward),
        ]);
    } else {
        v.extend([
            ("ctrl+left", MoveWordLeft),
            ("ctrl+right", MoveWordRight),
            ("ctrl+shift+left", SelectWordLeft),
            ("ctrl+shift+right", SelectWordRight),
            ("ctrl+home", MoveDocStart),
            ("ctrl+end", MoveDocEnd),
            ("ctrl+shift+home", SelectDocStart),
            ("ctrl+shift+end", SelectDocEnd),
            ("ctrl+backspace", DeleteWordBackward),
            ("f11", ToggleFullscreen),
            ("alt+left", NavigateBack),
            // Ctrl+W belongs to the shell inside a terminal (delete word); this works everywhere.
            ("ctrl+shift+w", CloseTab),
            ("alt+right", NavigateForward),
        ]);
    }
    v
}

impl Default for Keymap {
    fn default() -> Self {
        let defaults: HashMap<_, _> = default_bindings()
            .into_iter()
            .map(|(k, c)| (KeyChord::parse(k).expect("default chord parses"), c))
            .collect();
        Self { map: defaults.clone(), defaults, overrides: Vec::new() }
    }
}

impl Keymap {
    /// Defaults overlaid with user bindings (`"none"` unbinds). Returns warnings for bad entries.
    pub fn with_overrides<'a>(
        overrides: impl IntoIterator<Item = (&'a String, &'a String)>,
    ) -> (Self, Vec<String>) {
        let mut km = Self::default();
        let mut warnings = Vec::new();
        for (chord, cmd) in overrides {
            let chord = match KeyChord::parse(chord) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!("[keys] {e}"));
                    continue;
                }
            };
            if cmd == "none" {
                km.set_override(chord, None);
            } else if let Some(c) = Command::from_name(cmd) {
                km.set_override(chord, Some(c));
            } else {
                warnings.push(format!("[keys] unknown command {cmd:?}"));
            }
        }
        km.rebuild();
        (km, warnings)
    }

    fn set_override(&mut self, chord: KeyChord, cmd: Option<Command>) {
        self.overrides.retain(|(k, _)| *k != chord);
        // An override that restates the default is no override at all.
        if self.defaults.get(&chord).copied() != cmd {
            self.overrides.push((chord, cmd));
        }
    }

    fn rebuild(&mut self) {
        self.map = self.defaults.clone();
        for (k, c) in &self.overrides {
            match c {
                Some(c) => self.map.insert(*k, *c),
                None => self.map.remove(k),
            };
        }
    }

    pub fn lookup(&self, chord: &KeyChord) -> Option<Command> {
        self.map.get(chord).copied()
    }

    pub fn chords_for(&self, cmd: Command) -> Vec<KeyChord> {
        self.bindings_for(cmd).into_iter().map(|(k, _)| k).collect()
    }

    /// Active chords for `cmd` with their origin, defaults first.
    pub fn bindings_for(&self, cmd: Command) -> Vec<(KeyChord, Origin)> {
        let mut v: Vec<_> = self
            .map
            .iter()
            .filter(|(_, c)| **c == cmd)
            .map(|(k, _)| {
                let o = if self.defaults.get(k) == Some(&cmd) { Origin::Default } else { Origin::User };
                (*k, o)
            })
            .collect();
        v.sort_by_key(|(k, o)| (*o == Origin::User, k.to_string().len(), k.to_string()));
        v
    }

    /// Default chords of `cmd` that the user unbound or gave to another command.
    pub fn removed_defaults(&self, cmd: Command) -> Vec<KeyChord> {
        let mut v: Vec<_> = self
            .defaults
            .iter()
            .filter(|(k, c)| **c == cmd && self.map.get(k) != Some(&cmd))
            .map(|(k, _)| *k)
            .collect();
        v.sort_by_key(|k| k.to_string());
        v
    }

    /// Whether the user changed any binding of `cmd`.
    pub fn is_customized(&self, cmd: Command) -> bool {
        self.overrides.iter().any(|(k, c)| *c == Some(cmd) || self.defaults.get(k) == Some(&cmd))
    }

    /// Bind `chord` to `cmd`. Returns the command that previously owned the chord, if another.
    pub fn bind(&mut self, chord: KeyChord, cmd: Command) -> Option<Command> {
        let prev = self.lookup(&chord).filter(|c| *c != cmd);
        self.set_override(chord, Some(cmd));
        self.rebuild();
        prev
    }

    pub fn unbind(&mut self, chord: KeyChord) {
        self.set_override(chord, None);
        self.rebuild();
    }

    /// Restore `cmd` to exactly its default chords.
    pub fn reset(&mut self, cmd: Command) {
        let defaults = &self.defaults;
        self.overrides.retain(|(k, c)| *c != Some(cmd) && defaults.get(k) != Some(&cmd));
        self.rebuild();
    }

    pub fn reset_all(&mut self) {
        self.overrides.clear();
        self.rebuild();
    }

    /// User bindings as `[keys]` entries (chord, command name or "none").
    pub fn overrides_for_config(&self) -> Vec<(String, String)> {
        self.overrides
            .iter()
            .map(|(k, c)| (k.to_config(), c.map_or("none", |c| c.name()).to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display() {
        let c = KeyChord::parse("Ctrl+Shift+Z").unwrap();
        assert!(c.ctrl && c.shift && c.key == Key::Char('z'));
        assert_eq!(c.to_string(), "Ctrl+Shift+Z");
        assert_eq!(KeyChord::parse("ctrl++").unwrap().key, Key::Char('+'));
        assert!(KeyChord::parse("cmd+s").unwrap().logo);
        assert!(KeyChord::parse("bogus+s").is_err());
    }

    #[test]
    fn overrides() {
        let o = [
            ("ctrl+y".to_string(), "none".to_string()),
            ("ctrl+shift+z".to_string(), "redo".to_string()),
        ];
        let (km, w) = Keymap::with_overrides(o.iter().map(|(a, b)| (a, b)));
        assert!(w.is_empty());
        assert_eq!(km.lookup(&KeyChord::parse("ctrl+y").unwrap()), None);
        assert_eq!(
            km.lookup(&KeyChord::parse("ctrl+shift+z").unwrap()),
            Some(Command::Redo)
        );
    }

    #[test]
    fn config_round_trip_and_caps() {
        for (k, _) in default_bindings() {
            let c = KeyChord::parse(k).unwrap();
            assert_eq!(KeyChord::parse(&c.to_config()).unwrap(), c, "{k}");
        }
        let c = KeyChord::parse("ctrl+shift+k").unwrap();
        assert_eq!(c.caps().len(), 3);
    }

    #[test]
    fn bind_unbind_reset() {
        let mut km = Keymap::default();
        let dl = KeyChord::parse("mod+shift+k").unwrap();
        let new = KeyChord::parse("mod+d").unwrap();
        assert!(!km.is_customized(Command::DeleteLine));
        // Stealing Save's chord reports the previous owner.
        assert_eq!(km.bind(KeyChord::parse("mod+s").unwrap(), Command::DeleteLine), Some(Command::Save));
        assert!(km.is_customized(Command::Save));
        assert_eq!(km.removed_defaults(Command::Save).len(), 1);
        km.reset(Command::Save);
        assert_eq!(km.lookup(&KeyChord::parse("mod+s").unwrap()), Some(Command::Save));

        km.bind(new, Command::DeleteLine);
        km.unbind(dl);
        assert_eq!(km.bindings_for(Command::DeleteLine), vec![(new, Origin::User)]);
        assert_eq!(km.overrides_for_config().len(), 2);
        // Re-binding a default chord to its default command is not an override.
        km.bind(dl, Command::DeleteLine);
        assert_eq!(km.overrides_for_config().len(), 1);
        km.reset(Command::DeleteLine);
        assert!(!km.is_customized(Command::DeleteLine));
        assert!(km.overrides_for_config().is_empty());
    }
}
