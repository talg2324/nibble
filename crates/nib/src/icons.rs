//! Control-strip and title-bar icons (inline SVG, drawn with `currentColor` replaced at load time).

use iced::widget::svg;

fn handle(src: &str, color: &str) -> svg::Handle {
    svg::Handle::from_memory(src.replace("currentColor", color).into_bytes())
}

const FOLDER: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M3 6.5a1.5 1.5 0 0 1 1.5-1.5h4.2l2 2h8.8A1.5 1.5 0 0 1 21 8.5v9a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 17.5z"/></svg>"#;

/// A small fully-connected network (3 inputs, 4 hidden, 2 outputs): hollow nodes, with the
/// edges stopping short of each ring so the layers stay legible at toolbar size.
const NETWORK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor"><path stroke-width="0.55" stroke-linecap="round" opacity="0.7" d="M5.27 5.62L9.73 4.08M5.3 7.09L9.7 8.41M4.74 8.05L10.26 13.25M4.28 8.43L10.72 18.67M4.73 10.33L10.27 4.97M5.28 11.26L9.72 9.84M5.28 12.74L9.72 14.16M4.73 13.67L10.27 19.03M4.28 15.57L10.72 5.33M4.74 15.95L10.26 10.75M5.3 16.91L9.7 15.59M5.27 18.38L9.73 19.92M14.02 4.6L18.98 7.8M13.47 5.2L19.53 13M14.4 9.1L18.6 9.1M14.02 10.4L18.98 13.6M14.02 13.6L18.98 10.4M14.4 14.9L18.6 14.9M13.47 18.8L19.53 11M14.02 19.4L18.98 16.2"/><g stroke-width="1.0"><circle cx="3" cy="6.4" r="1.4"/><circle cx="3" cy="12" r="1.4"/><circle cx="3" cy="17.6" r="1.4"/><circle cx="12" cy="3.3" r="1.4"/><circle cx="12" cy="9.1" r="1.4"/><circle cx="12" cy="14.9" r="1.4"/><circle cx="12" cy="20.7" r="1.4"/><circle cx="21" cy="9.1" r="1.4"/><circle cx="21" cy="14.9" r="1.4"/></g></svg>"#;

const TERMINAL: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4.5" width="18" height="15" rx="1.5"/><path d="M7 9.5l3 2.5-3 2.5M12.5 15h4.5"/></svg>"#;

const GEAR: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/></svg>"#;

/// A branch leaving the trunk.
const GIT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3.5v11.5"/><circle cx="18" cy="6.5" r="2.7"/><circle cx="6" cy="18" r="2.7"/><path d="M18 9.2a9 9 0 0 1-9 8.8"/></svg>"#;

/// A page of text under a magnifying glass: the Review tab (what changed, read like a pull request).
const REVIEW: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M10.5 21H5.5A1.5 1.5 0 0 1 4 19.5v-15A1.5 1.5 0 0 1 5.5 3h10A1.5 1.5 0 0 1 17 4.5V8.5"/><path d="M7.5 7.5h6M7.5 11h3M7.5 14.5h2"/><circle cx="15.5" cy="15" r="4"/><path d="M18.5 18l2.5 2.5"/></svg>"#;

/// The letters SSH, drawn in strokes like the other icons: the SSH panel.
const REMOTE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M6.6000000000000005 8.6C6.1000000000000005 7.7 5.2 7.2 4.1 7.2C2.5999999999999996 7.2 1.6 8.1 1.6 9.3C1.6 12.1 6.8 11 6.8 14.3C6.8 15.8 5.6000000000000005 16.8 4.0 16.8C2.8 16.8 1.7999999999999998 16.2 1.2 15.3 M14.4 8.6C13.9 7.7 13.0 7.2 11.9 7.2C10.4 7.2 9.4 8.1 9.4 9.3C9.4 12.1 14.6 11 14.6 14.3C14.6 15.8 13.4 16.8 11.8 16.8C10.6 16.8 9.6 16.2 9.0 15.3 M17.4 7.2V16.8M22.8 7.2V16.8M17.4 12H22.8"/></svg>"#;

/// A window with a play triangle in it: the Apps panel (the project's apps, run here).
const APPS: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4.5" width="18" height="15" rx="1.5"/><path d="M3 8.5h18"/><path d="M10.2 11.2v5.6l4.6-2.8z"/></svg>"#;

/// The logo mark without its app-icon tile: three blocks and a bitten fourth.
/// `currentColor` paints the blocks; `ACCENT` the bitten one.
const LOGO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="104 394 798 186"><defs><mask id="bite"><rect x="0" y="0" width="1024" height="1024" fill="#fff"/><circle cx="902" cy="402" r="74" fill="#000"/><circle cx="826" cy="372" r="40" fill="#000"/><circle cx="930" cy="480" r="40" fill="#000"/></mask></defs><g fill="currentColor"><rect x="112" y="402" width="170" height="170" rx="34"/><rect x="316" y="402" width="170" height="170" rx="34"/><rect x="520" y="402" width="170" height="170" rx="34"/></g><rect x="724" y="402" width="170" height="170" rx="34" fill="ACCENT" mask="url(#bite)"/></svg>"##;

/// The logo for the status bar, drawn on the pixel grid: shown at exactly LOGO_SMALL_SIZE, every
/// block and gap covers whole pixels (at 1x and 2x), so the gaps come out even. (Scaling LOGO
/// down makes 2.2px gaps that land on different pixel fractions.) One round bite: LOGO's three
/// small circles blur into a slanted corner at this size.
const LOGO_SMALL: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 53 11"><defs><mask id="bite"><rect width="53" height="11" fill="#fff"/><circle cx="53.4" cy="-0.4" r="6" fill="#000"/></mask></defs><g fill="currentColor"><rect width="11" height="11" rx="2.5"/><rect x="14" width="11" height="11" rx="2.5"/><rect x="28" width="11" height="11" rx="2.5"/></g><rect x="42" width="11" height="11" rx="2.5" fill="ACCENT" mask="url(#bite)"/></svg>"##;

/// Title-bar buttons (`app/titlebar.rs`): each SVG is the whole 46x30 button, glyph in the
/// middle, so the icon's hover state is the button's. Lines sit on half pixels to stay sharp.
/// Drawn white; the title bar tints them.
const WIN_MINIMIZE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 46 30" fill="none" stroke="#fff" stroke-width="1"><path d="M18 15.5h10"/></svg>"##;
const WIN_MAXIMIZE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 46 30" fill="none" stroke="#fff" stroke-width="1"><rect x="18.5" y="10.5" width="9" height="9"/></svg>"##;
const WIN_RESTORE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 46 30" fill="none" stroke="#fff" stroke-width="1"><rect x="18.5" y="12.5" width="7" height="7"/><path d="M20.5 12.5v-2h7v7h-2"/></svg>"##;
const WIN_CLOSE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 46 30" fill="none" stroke="#fff" stroke-width="1.1"><path d="M18 10l10 10M28 10l-10 10"/></svg>"##;

/// LOGO_SMALL's width and height in logical pixels: its viewBox, one unit per pixel.
pub const LOGO_SMALL_SIZE: (f32, f32) = (53.0, 11.0);

/// Width / height of the logo's viewBox, for sizing it without distortion.
pub const LOGO_ASPECT: f32 = 798.0 / 186.0;

pub struct Icons {
    pub folder: svg::Handle,
    pub network: svg::Handle,
    pub terminal: svg::Handle,
    pub folder_on: svg::Handle,
    pub network_on: svg::Handle,
    pub terminal_on: svg::Handle,
    pub git: svg::Handle,
    pub git_on: svg::Handle,
    pub diff: svg::Handle,
    pub diff_on: svg::Handle,
    pub remote: svg::Handle,
    pub remote_on: svg::Handle,
    pub apps: svg::Handle,
    pub apps_on: svg::Handle,
    pub gear: svg::Handle,
    pub gear_on: svg::Handle,
    pub logo: svg::Handle,
    pub logo_small: svg::Handle,
    pub win_minimize: svg::Handle,
    pub win_maximize: svg::Handle,
    pub win_restore: svg::Handle,
    pub win_close: svg::Handle,
}

impl Icons {
    pub fn new(off: &str, on: &str) -> Self {
        Self {
            folder: handle(FOLDER, off),
            network: handle(NETWORK, off),
            terminal: handle(TERMINAL, off),
            folder_on: handle(FOLDER, on),
            network_on: handle(NETWORK, on),
            terminal_on: handle(TERMINAL, on),
            git: handle(GIT, off),
            git_on: handle(GIT, on),
            diff: handle(REVIEW, off),
            diff_on: handle(REVIEW, on),
            remote: handle(REMOTE, off),
            remote_on: handle(REMOTE, on),
            apps: handle(APPS, off),
            apps_on: handle(APPS, on),
            gear: handle(GEAR, off),
            gear_on: handle(GEAR, on),
            logo: handle(&LOGO.replace("ACCENT", on), off),
            logo_small: handle(&LOGO_SMALL.replace("ACCENT", on), off),
            win_minimize: svg::Handle::from_memory(WIN_MINIMIZE.as_bytes()),
            win_maximize: svg::Handle::from_memory(WIN_MAXIMIZE.as_bytes()),
            win_restore: svg::Handle::from_memory(WIN_RESTORE.as_bytes()),
            win_close: svg::Handle::from_memory(WIN_CLOSE.as_bytes()),
        }
    }
}

pub fn hex(c: iced::Color) -> String {
    let [r, g, b, _] = c.into_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
}
