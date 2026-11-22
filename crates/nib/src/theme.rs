//! Colours. Built-in defaults (black background; code text Dark+-like; blue file browser),
//! overridable per key in `[theme]` of the config and live from Settings > Appearance.

use iced::widget::scrollable;
use iced::{Color, border};
use nib_editor::config::ThemeConfig;
use std::collections::HashMap;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub gutter: Color,
    pub gutter_current: Color,
    pub current_line: Color,
    pub selection: Color,
    pub cursor: Color,
    pub dim: Color,
    pub accent: Color,
    pub warn: Color,
    pub browser_fg: Color,
    pub browser_sel_bg: Color,
    pub browser_sel_fg: Color,
    pub terminal_fg: Color,
    pub terminal_bg: Color,
    pub divider: Color,
    pub panel: Color,
    pub skill_provider: Color,
    pub skill_nib: Color,
    pub skill_user: Color,
    pub skill_repo: Color,
    pub skill_command: Color,
    pub skill_tool: Color,
    pub diff_add: Color,
    pub diff_del: Color,
    syntax: HashMap<String, Color>,
}

/// A user-settable UI colour: `[theme]` key, label in Settings > Appearance, group, default.
pub struct UiColor {
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub default: Color,
}

const BLUE: Color = rgb(0x3b, 0x8e, 0xea);

macro_rules! ui_colors {
    ($( $field:ident: $key:literal, $label:literal, $group:literal, $default:expr; )*) => {
        pub const UI_COLORS: &[UiColor] = &[
            $( UiColor { key: $key, label: $label, group: $group, default: $default }, )*
        ];
        impl Theme {
            pub fn get(&self, key: &str) -> Option<Color> {
                match key { $( $key => Some(self.$field), )* _ => None }
            }
            fn slot(&mut self, key: &str) -> Option<&mut Color> {
                match key { $( $key => Some(&mut self.$field), )* _ => None }
            }
        }
    };
}

ui_colors! {
    background:     "background",     "Background",             "Editor", rgb(0, 0, 0);
    foreground:     "foreground",     "Text",                   "Editor", rgb(0xd4, 0xd4, 0xd4);
    current_line:   "current_line",   "Current line",           "Editor", rgb(0x12, 0x12, 0x12);
    selection:      "selection",      "Selection",              "Editor", rgb(0x26, 0x4f, 0x78);
    cursor:         "cursor",         "Cursor",                 "Editor", rgb(0xae, 0xaf, 0xad);
    gutter:         "gutter",         "Line numbers",           "Editor", rgb(0x55, 0x5b, 0x63);
    gutter_current: "gutter_current", "Current line number",    "Editor", rgb(0xc6, 0xc6, 0xc6);
    accent:         "accent",         "Accent",                 "Interface", BLUE;
    dim:            "dim",            "Secondary text",         "Interface", rgb(0x6e, 0x76, 0x81);
    divider:        "divider",        "Dividers",               "Interface", rgb(0x26, 0x26, 0x26);
    panel:          "panel",          "Panels & menus",         "Interface", rgb(0, 0, 0);
    warn:           "warning",        "Warnings",               "Interface", rgb(0xe5, 0xc0, 0x7b);
    browser_fg:     "browser_foreground", "Text",               "File browser", BLUE;
    browser_sel_bg: "browser_selected_background", "Selected row", "File browser", BLUE;
    browser_sel_fg: "browser_selected_foreground", "Selected text", "File browser", rgb(0, 0, 0);
    terminal_fg:    "terminal_foreground", "Text",              "Terminal", rgb(0xd4, 0xd4, 0xd4);
    terminal_bg:    "terminal_background", "Background",        "Terminal", rgb(0, 0, 0);
    skill_provider: "skill_provider", "Model provider's skills", "Agents", rgb(0xc6, 0x78, 0xdd);
    skill_nib:      "skill_nib",      "nib's skills",          "Agents", BLUE;
    skill_user:     "skill_user",     "Your skills",            "Agents", rgb(0xe8, 0x8f, 0xb4);
    skill_repo:     "skill_repo",     "The project's skills",  "Agents", rgb(0x98, 0xc3, 0x79);
    skill_command:  "skill_command",  "Commands",               "Agents", rgb(0x56, 0xb6, 0xc2);
    skill_tool:     "skill_tool",     "Tools",                  "Agents", rgb(0xd1, 0x9a, 0x66);
    diff_add:       "diff_added",     "Added lines",            "Diff", rgb(0x3f, 0xb9, 0x50);
    diff_del:       "diff_removed",   "Removed lines",          "Diff", rgb(0xf8, 0x51, 0x49);
}

pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        6 => Some(rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)),
        _ => None,
    }
}

impl Theme {
    /// Returns the theme and warnings for unknown keys and unparsable values.
    pub fn from_config(c: &ThemeConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let black = rgb(0, 0, 0);
        let mut t = Theme {
            background: black,
            foreground: black,
            gutter: black,
            gutter_current: black,
            current_line: black,
            selection: black,
            cursor: black,
            dim: black,
            accent: black,
            warn: black,
            browser_fg: black,
            browser_sel_bg: black,
            browser_sel_fg: black,
            terminal_fg: black,
            terminal_bg: black,
            divider: black,
            panel: black,
            skill_provider: black,
            skill_nib: black,
            skill_user: black,
            skill_repo: black,
            skill_command: black,
            skill_tool: black,
            diff_add: black,
            diff_del: black,
            syntax: HashMap::new(),
        };
        for u in UI_COLORS {
            *t.slot(u.key).expect("table key") = u.default;
        }
        for (k, v) in &c.colors {
            match (t.slot(k), parse_hex(v)) {
                (Some(slot), Some(col)) => *slot = col,
                (Some(_), None) => warnings.push(format!("[theme] {k} = {v:?}: expected \"#rrggbb\"")),
                (None, _) => warnings.push(format!("[theme] unknown colour {k:?}")),
            }
        }
        for (k, v) in &c.syntax {
            match parse_hex(v) {
                Some(col) => {
                    t.syntax.insert(k.clone(), col);
                }
                None => warnings.push(format!("[theme.syntax] {k} = {v:?}: expected \"#rrggbb\"")),
            }
        }
        (t, warnings)
    }

    /// The colour of a skill group, wherever its name or one of its skills is shown.
    pub fn skill_group(&self, g: nib_proto::SkillGroup) -> Color {
        match g {
            nib_proto::SkillGroup::Provider => self.skill_provider,
            nib_proto::SkillGroup::Nib => self.skill_nib,
            nib_proto::SkillGroup::User => self.skill_user,
            nib_proto::SkillGroup::Repo => self.skill_repo,
        }
    }

    /// What `scope` would be without any `[theme.syntax]` overrides.
    pub fn default_scope(&self, scope: &str) -> Color {
        builtin_scope(scope, self.foreground)
    }

    /// Colour for a highlight scope: user override (longest dotted prefix), else built-in.
    pub fn scope(&self, scope: &str) -> Color {
        let mut s = scope;
        loop {
            if let Some(c) = self.syntax.get(s) {
                return *c;
            }
            match s.rfind('.') {
                Some(i) => s = &s[..i],
                None => break,
            }
        }
        builtin_scope(scope, self.foreground)
    }
}

fn builtin_scope(scope: &str, fg: Color) -> Color {
    let top = |s: &str| scope == s || scope.strip_prefix(s).is_some_and(|r| r.starts_with('.'));
    let (r, g, b) = if scope == "comment.block.documentation" {
        (0xce, 0x91, 0x78)
    } else if top("comment") {
        (0x6a, 0x99, 0x55)
    } else if top("keyword.control") || scope == "keyword.directive" {
        (0xc5, 0x86, 0xc0)
    } else if top("keyword") {
        (0x56, 0x9c, 0xd6)
    } else if top("string.regexp") {
        (0xd1, 0x69, 0x69)
    } else if top("string") {
        (0xce, 0x91, 0x78)
    } else if top("constant.numeric") {
        (0xb5, 0xce, 0xa8)
    } else if top("constant.character") {
        (0xd7, 0xba, 0x7d)
    } else if top("constant") || top("variable.builtin") {
        (0x56, 0x9c, 0xd6)
    } else if top("function") || top("attribute") {
        (0xdc, 0xdc, 0xaa)
    } else if top("type") || top("constructor") || top("namespace") {
        (0x4e, 0xc9, 0xb0)
    } else if top("variable") || top("label") {
        (0x9c, 0xdc, 0xfe)
    } else if top("markup.heading") || top("markup.bold") {
        (0x56, 0x9c, 0xd6)
    } else if top("markup.italic") {
        (0xc5, 0x86, 0xc0)
    } else if top("markup.raw") || top("markup.link.text") {
        (0xce, 0x91, 0x78)
    } else if top("markup.link") {
        (0x4e, 0xc9, 0xb0)
    } else if top("markup.list") {
        (0xd7, 0xba, 0x7d)
    } else if top("markup.quote") {
        (0x6a, 0x99, 0x55)
    } else if top("markup.strikethrough") {
        (0x80, 0x80, 0x80)
    } else {
        return fg;
    };
    rgb(r, g, b)
}

/// xterm 256-colour palette.
pub fn xterm(i: u8) -> Color {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), (0xcd, 0x31, 0x31), (0x0d, 0xbc, 0x79), (0xe5, 0xe5, 0x10),
        (0x24, 0x72, 0xc8), (0xbc, 0x3f, 0xbc), (0x11, 0xa8, 0xcd), (0xe5, 0xe5, 0xe5),
        (0x66, 0x66, 0x66), (0xf1, 0x4c, 0x4c), (0x23, 0xd1, 0x8b), (0xf5, 0xf5, 0x43),
        (0x3b, 0x8e, 0xea), (0xd6, 0x70, 0xd6), (0x29, 0xb8, 0xdb), (0xe5, 0xe5, 0xe5),
    ];
    match i {
        0..=15 => {
            let (r, g, b) = BASE[i as usize];
            rgb(r, g, b)
        }
        16..=231 => {
            let i = i - 16;
            let l = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            rgb(l(i / 36), l((i / 6) % 6), l(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            rgb(v, v, v)
        }
    }
}

/// Style for every `scrollable` in the app. iced's stock Dark style draws the thumb barely
/// lighter than its rail, so the scroll position is hard to read; this uses a faint rail and a
/// bright thumb that brightens on hover and turns accent-coloured while dragged.
pub fn scrollbar(theme: &iced::Theme, status: scrollable::Status) -> scrollable::Style {
    let palette = theme.extended_palette();
    let ink = palette.background.base.text;
    let rail = |thumb: Color| scrollable::Rail {
        background: Some(ink.scale_alpha(0.08).into()),
        border: border::rounded(3),
        scroller: scrollable::Scroller { background: thumb.into(), border: border::rounded(3) },
    };
    let idle = ink.scale_alpha(0.5);
    let pick = |hovered: bool, dragged: bool| {
        rail(if dragged {
            palette.primary.base.color
        } else if hovered {
            ink.scale_alpha(0.75)
        } else {
            idle
        })
    };
    let (vertical_rail, horizontal_rail) = match status {
        scrollable::Status::Active { .. } => (rail(idle), rail(idle)),
        scrollable::Status::Hovered { is_vertical_scrollbar_hovered: v, is_horizontal_scrollbar_hovered: h, .. } => {
            (pick(v, false), pick(h, false))
        }
        scrollable::Status::Dragged { is_vertical_scrollbar_dragged: v, is_horizontal_scrollbar_dragged: h, .. } => {
            (pick(false, v), pick(false, h))
        }
    };
    scrollable::Style { vertical_rail, horizontal_rail, ..scrollable::default(theme, status) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_and_prefixes() {
        let mut c = ThemeConfig::default();
        c.colors.insert("background".into(), "#101010".into());
        c.syntax.insert("keyword".into(), "#ff0000".into());
        c.syntax.insert("bogus".into(), "red".into());
        let (t, w) = Theme::from_config(&c);
        assert_eq!(t.background, rgb(0x10, 0x10, 0x10));
        assert_eq!(t.scope("keyword.control.import"), rgb(0xff, 0, 0));
        assert_eq!(w.len(), 1);
        assert_eq!(xterm(196), rgb(255, 0, 0));
    }
}
