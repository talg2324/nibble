//! Fonts named for what is installed, so laying out text does not search for them.
//!
//! cosmic-text names the generic families for fonts that are seldom there: "Noto Sans Mono",
//! "Open Sans", "DejaVu Serif" (on macOS and Windows, never). Text still draws, in the font its
//! fallback settles on, but it searches for that font again for every word. Monospace, nib's
//! font for nearly all text, is the worst: a word in the generic monospace family is checked
//! against every monospace font there is, even with the family named for one that is installed.
//! An agent's terminal, laid out anew whenever another agent is shown, took most of a second.
//!
//! So nib names its monospace font: [`mono`] is the font generic monospace settles on (the same
//! look), by name. [`pin`] names the generic sans-serif and serif for the fonts they settle on.

use std::sync::OnceLock;

use iced::advanced::graphics::text::{cosmic_text as ct, font_system};
use iced::Font;

/// The monospace font, by name: what text in [`Font::MONOSPACE`] is drawn in.
pub fn mono() -> Font {
    static MONO: OnceLock<Font> = OnceLock::new();
    *MONO.get_or_init(|| {
        let Ok(mut fs) = font_system().write() else { return Font::MONOSPACE };
        match settles_on(fs.raw(), ct::Family::Monospace) {
            Some(name) => Font::with_name(Box::leak(name.into_boxed_str())),
            None => Font::MONOSPACE,
        }
    })
}

/// Name the generic families for installed fonts. Run once before any text is laid out.
pub fn pin() {
    let _ = mono();
    let Ok(mut fs) = font_system().write() else { return };
    let raw = fs.raw();
    for generic in [ct::Family::Monospace, ct::Family::SansSerif, ct::Family::Serif] {
        let named = raw.db().family_name(&generic).to_string();
        if installed(raw, &named) {
            continue;
        }
        let Some(found) = settles_on(raw, generic) else { continue };
        let db = raw.db_mut();
        match generic {
            ct::Family::Monospace => db.set_monospace_family(found),
            ct::Family::SansSerif => db.set_sans_serif_family(found),
            _ => db.set_serif_family(found),
        }
    }
}

fn installed(raw: &ct::FontSystem, family: &str) -> bool {
    raw.db().faces().any(|f| f.families.iter().any(|(n, _)| n == family))
}

/// The family of the font plain text in `family` is drawn in.
fn settles_on(raw: &mut ct::FontSystem, family: ct::Family<'_>) -> Option<String> {
    let attrs = ct::Attrs::new().family(family);
    let mut buf = ct::Buffer::new(raw, ct::Metrics::new(14.0, 16.0));
    let mut b = buf.borrow_with(raw);
    b.set_text("abc", &attrs, ct::Shaping::Advanced, None);
    b.shape_until_scroll(false);
    let id = b.layout_runs().next()?.glyphs.first()?.font_id;
    raw.db().face(id)?.families.first().map(|(n, _)| n.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_are_named_for_installed_ones_and_look_the_same() {
        let (mono_before, sans_before) = {
            let mut fs = font_system().write().unwrap();
            (settles_on(fs.raw(), ct::Family::Monospace), settles_on(fs.raw(), ct::Family::SansSerif))
        };
        pin();
        let mut fs = font_system().write().unwrap();
        let raw = fs.raw();
        for generic in [ct::Family::Monospace, ct::Family::SansSerif, ct::Family::Serif] {
            let named = raw.db().family_name(&generic).to_string();
            assert!(installed(raw, &named), "{generic:?} names {named}, which is not installed");
        }
        // The same fonts as before.
        assert_eq!(settles_on(raw, ct::Family::SansSerif), sans_before);
        match mono().family {
            iced::font::Family::Name(n) => {
                assert!(installed(raw, n));
                assert_eq!(Some(n.to_string()), mono_before);
            }
            other => panic!("monospace not named: {other:?}"),
        }
    }
}
