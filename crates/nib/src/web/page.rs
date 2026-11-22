//! What runs inside a page (in nib's own sandbox, not among the page's globals) to describe it
//! and to find the things one can act on, plus the small pure helpers around that: a tab's
//! name, and key names as the browser wants them.

use super::local;
use anyhow::{bail, Context, Result};

/// The page as text with the things one can act on marked `[N name]`, and those things listed.
/// Called with `(max, base)`: how much text to give, and the number of this frame's first
/// element (frames of one page share one numbering). Goes through open shadow roots. Leaves the
/// elements in `nibRefs` for `NODE` to find again.
///
/// A control's name is the first of: `aria-label`, what `aria-labelledby` points at, its
/// `<label>`, its own text, placeholder, `title`, a tooltip attribute, its icon's name, `alt`,
/// the text beside it in the same label, row or small box, its `name` attribute.
pub const READ: &str = r##"(max, base) => {
  const ACT = 'a[href],button,input,select,textarea,summary,[role=button],[role=link],[role=tab],[role=checkbox],[role=radio],[role=switch],[role=menuitem],[role=option],[role=combobox],[role=textbox],[role=slider],[contenteditable=""],[contenteditable=true],[onclick]';
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'svg']);
  const BLOCK = new Set(['P', 'DIV', 'LI', 'TR', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'SECTION', 'ARTICLE', 'HEADER', 'FOOTER', 'NAV', 'PRE', 'TABLE', 'UL', 'OL', 'BR', 'FORM', 'LABEL']);
  const FIELD = new Set(['INPUT', 'SELECT', 'TEXTAREA']);
  const TIPS = ['mattooltip', 'data-tooltip', 'data-tooltip-content', 'data-tip', 'data-title', 'data-original-title', 'data-bs-original-title'];
  const refs = [], elements = [], out = [];
  let len = 0, more = 0, cut = false;
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const say = (s) => { if (len > max) { cut = true; return; } out.push(s); len += s.length; };
  const words = (s) => clean(s.replace(/[-_]+/g, ' '));
  const icon = (e) => {
    const t = e.querySelector('svg title');
    if (t && clean(t.textContent)) return clean(t.textContent);
    const marks = [e, ...e.querySelectorAll('[data-icon],[svgicon],mat-icon,i,svg,span,[data-testid]')].slice(0, 8);
    for (const m of marks) {
      const named = m.getAttribute('data-icon') || m.getAttribute('svgicon') || m.getAttribute('data-lucide');
      if (named) return words(named.replace(/^.*:/, ''));
      for (const c of String(m.getAttribute('class') || '').split(/\s+/)) {
        const k = /^(?:fa|lucide|icon|bi|mdi|codicon|tabler|ti|ri|ph|octicon|glyphicon)-([a-z0-9-]{2,})$/.exec(c);
        if (k && !/^(solid|regular|light|brands|lg|sm|xs|fw|spin|\dx)$/.test(k[1])) return words(k[1]);
      }
      const id = m.getAttribute('data-testid');
      if (id && /icon/i.test(id)) return words(id.replace(/icon/ig, ''));
    }
    return '';
  };
  const beside = (e) => {
    const l = e.closest('label');
    if (l && clean(l.textContent)) return clean(l.textContent);
    // The text of the small box it sits in: a table row or list item, or a box that holds
    // no other control (so the text is about this one).
    // (A row is a row by its tag, its role, or what its makers called it: TensorBoard's
    // run rows are `<tb-data-table-content-row>`.)
    const row = (p) => p.matches('tr,li,[role=row],[role=listitem]') || /ROW|ITEM/.test(p.tagName) || /(^|[\s_-])(row|item)([\s_-]|$)/i.test(String(p.getAttribute('class') || ''));
    let p = e.parentElement;
    for (let up = 0; p && up < 7; up++, p = p.parentElement) {
      const t = clean(p.textContent);
      if (t.length > 80 || (p.querySelectorAll(ACT).length > 1 && !row(p))) return '';
      if (t) return t;
    }
    return '';
  };
  const name = (e) => {
    const field = FIELD.has(e.tagName);
    let n = e.getAttribute('aria-label') || '';
    if (!n && e.getAttribute('aria-labelledby')) {
      const root = e.getRootNode();
      n = e.getAttribute('aria-labelledby').split(/\s+/).map(i => root.getElementById && root.getElementById(i)).filter(Boolean).map(x => x.textContent).join(' ');
    }
    if (!clean(n) && e.labels && e.labels.length) n = e.labels[0].textContent;
    if (!clean(n) && !field) n = e.textContent;
    if (!clean(n) && e.tagName === 'INPUT' && (e.type === 'submit' || e.type === 'button' || e.type === 'reset')) n = e.value;
    if (!clean(n)) n = e.getAttribute('placeholder') || e.getAttribute('title') || '';
    if (!clean(n)) for (const t of TIPS) { if (e.getAttribute(t)) { n = e.getAttribute(t); break; } }
    if (!clean(n)) n = icon(e);
    if (!clean(n)) { const i = e.querySelector('img[alt]'); n = (i && i.alt) || e.getAttribute('alt') || ''; }
    if (!clean(n)) n = beside(e);
    if (!clean(n)) n = e.getAttribute('name') || '';
    return clean(n).slice(0, 80);
  };
  const role = (e) => e.getAttribute('role') || (e.tagName === 'INPUT' ? (e.type || 'text') : e.isContentEditable ? 'editable' : e.tagName.toLowerCase());
  const walk = (node) => {
    for (const c of node.childNodes) {
      if (c.nodeType === 3) { const t = clean(c.nodeValue); if (t) say(t + ' '); continue; }
      if (c.nodeType !== 1 || SKIP.has(c.tagName)) continue;
      if (c.checkVisibility && !c.checkVisibility({ visibilityProperty: true }) && getComputedStyle(c).display !== 'contents') continue;
      const block = BLOCK.has(c.tagName);
      if (block) say('\n');
      if (/^H[1-6]$/.test(c.tagName)) say('#'.repeat(+c.tagName[1]) + ' ');
      if (c.tagName === 'IFRAME' || c.tagName === 'FRAME') { say('\n(frame) '); continue; }
      if (c.matches(ACT)) {
        const r = c.getBoundingClientRect();
        if (r.width < 1 || r.height < 1) continue;
        if (refs.length >= 500) { more++; continue; }
        const n = base + refs.length; refs.push(c);
        const el = { ref: n, role: role(c), name: name(c) };
        if (FIELD.has(c.tagName) && c.type !== 'password' && c.type !== 'file') el.value = String(c.value).slice(0, 200);
        if (c.type === 'checkbox' || c.type === 'radio') el.checked = c.checked;
        if (c.getAttribute('aria-checked')) el.checked = c.getAttribute('aria-checked') === 'true';
        if (c.getAttribute('aria-selected') === 'true') el.selected = true;
        if (c.getAttribute('aria-expanded')) el.expanded = c.getAttribute('aria-expanded') === 'true';
        if (c.disabled || c.getAttribute('aria-disabled') === 'true') el.disabled = true;
        if (c.tagName === 'A') el.href = c.getAttribute('href');
        if (c.tagName === 'SELECT') el.options = [...c.options].slice(0, 40).map(o => clean(o.text));
        if (r.bottom < 0 || r.top > innerHeight) el.offscreen = true;
        elements.push(el);
        const shown = c.tagName === 'SELECT' ? clean((c.selectedOptions[0] || {}).text) : el.value;
        say('[' + n + ' ' + (el.name || el.role) + (shown && c.type !== 'checkbox' && c.type !== 'radio' ? '=' + shown.slice(0, 40) : '') + (el.checked === true ? ' ✓' : '') + '] ');
        if (c.isContentEditable || c.tagName === 'SELECT' || c.tagName === 'TEXTAREA') continue;
        if (!c.querySelector(ACT) && !c.shadowRoot) continue;
      }
      if (c.shadowRoot) walk(c.shadowRoot);
      walk(c);
      if (block) say('\n');
    }
  };
  if (document.body) walk(document.body);
  globalThis.nibRefs = refs;
  const a = document.activeElement;
  return JSON.stringify({
    url: location.href, title: document.title, ready: document.readyState,
    scroll: { y: Math.round(scrollY), height: document.documentElement.scrollHeight, view: innerHeight },
    focus: a && refs.indexOf(a) >= 0 ? base + refs.indexOf(a) : null,
    text: out.join('').replace(/[ \t]*\n\s*/g, '\n').trim(), truncated: cut,
    elements, more, count: refs.length,
  });
}"##;

/// `(n)`: element `n` of this frame's last `READ`, scrolled into view. Throws `gone` when it is
/// no longer on the page or no longer shown.
pub const NODE: &str = r#"(n) => {
  const e = (globalThis.nibRefs || [])[n];
  if (!e || !e.isConnected || (e.checkVisibility && !e.checkVisibility({ visibilityProperty: true }))) throw new Error('gone');
  e.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  return e;
}"#;

/// `(n, option)`: choose an option of `<select>` number `n` by its text or value, as a person
/// would (the page hears `input` and `change`). Returns false when `n` is not a `<select>`.
pub const SELECT: &str = r#"(n, v) => {
  const e = (globalThis.nibRefs || [])[n];
  if (!e || !e.isConnected) throw new Error('gone');
  if (e.tagName !== 'SELECT') return false;
  const want = v.trim().toLowerCase();
  const o = [...e.options].find(o => o.text.trim().toLowerCase() === want || o.value.toLowerCase() === want);
  if (!o) throw new Error('no such option; it has: ' + [...e.options].map(o => o.text.trim()).join(' | '));
  e.value = o.value;
  e.dispatchEvent(new Event('input', { bubbles: true }));
  e.dispatchEvent(new Event('change', { bubbles: true }));
  return true;
}"#;

/// `(n)`: select everything in field `n` (it has just been clicked), so that typing replaces it.
pub const SELECT_ALL: &str = r#"(n) => {
  const e = (globalThis.nibRefs || [])[n];
  if (!e || !e.isConnected) throw new Error('gone');
  if (typeof e.select === 'function') { e.select(); return true; }
  const a = document.activeElement && document.activeElement.isContentEditable ? document.activeElement : e;
  const r = document.createRange();
  r.selectNodeContents(a);
  const s = getSelection();
  s.removeAllRanges();
  s.addRange(r);
  return true;
}"#;

/// The element at a point of the page, through open shadow roots (the start of a function).
macro_rules! at_point {
    () => {
        "(x, y) => { const at = (x, y) => { let e = document.elementFromPoint(x, y); while (e && e.shadowRoot) { const d = e.shadowRoot.elementFromPoint(x, y); if (!d || d === e) break; e = d; } return e; };"
    };
}

/// `(x, y)`: if a list to choose from (`<select>`) is there, what it offers and where it is, as
/// JSON; else ''. The list is kept for `CHOOSE`.
pub const PICK: &str = concat!(at_point!(), r#"
  const e = at(x, y);
  const s = e && e.closest && e.closest('select');
  if (!s || s.disabled || s.multiple || s.size > 1) return '';
  globalThis.nibPick = s;
  const r = s.getBoundingClientRect();
  return JSON.stringify({ options: [...s.options].map(o => o.text.trim()), selected: s.selectedIndex, x: r.left, y: r.top, w: r.width, h: r.height });
}"#);

/// `(i)`: choose option `i` of the list `PICK` found, as a person would.
pub const CHOOSE: &str = r#"(i) => {
  const s = globalThis.nibPick;
  if (!s || !s.isConnected || i >= s.options.length) return false;
  s.focus();
  if (s.selectedIndex === i) return true;
  s.selectedIndex = i;
  s.dispatchEvent(new Event('input', { bubbles: true }));
  s.dispatchEvent(new Event('change', { bubbles: true }));
  return true;
}"#;

/// `(x, y)`: the shape the pointer should have there: `pointer`, `text`, a CSS cursor name, or ''.
pub const SHAPE: &str = concat!(at_point!(), r#"
  const e = at(x, y);
  if (!e) return '';
  const c = getComputedStyle(e).cursor;
  if (c && c !== 'auto') return c;
  if (e.closest('a[href]')) return 'pointer';
  if (e.isContentEditable || e.tagName === 'TEXTAREA' || (e.tagName === 'INPUT' && !['button', 'submit', 'reset', 'checkbox', 'radio', 'range', 'file', 'color'].includes(e.type))) return 'text';
  // Over plain text a browser shows the text cursor.
  const s = document.caretPositionFromPoint ? document.caretPositionFromPoint(x, y) : null;
  if (s && s.offsetNode && s.offsetNode.nodeType === 3 && getComputedStyle(e).userSelect !== 'none') return 'text';
  return '';
}"#);

/// The text selected in the page (in a field, or on the page), for copying.
pub const SELECTED: &str = r#"() => {
  let a = document.activeElement;
  while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
  if (a && typeof a.selectionStart === 'number' && a.selectionEnd > a.selectionStart && a.type !== 'password') return a.value.slice(a.selectionStart, a.selectionEnd);
  return String(getSelection());
}"#;

/// `(text)`: put text where the caret is, as pasting does.
pub const INSERT: &str = "(text) => document.execCommand('insertText', false, text)";

/// `(text)`: whether the page shows `text` anywhere (open shadow roots included).
pub const HAS_TEXT: &str = r#"(text) => {
  const parts = [];
  const walk = (node) => {
    for (const c of node.childNodes) {
      if (c.nodeType === 3) { parts.push(c.nodeValue); continue; }
      if (c.nodeType !== 1 || c.tagName === 'SCRIPT' || c.tagName === 'STYLE' || c.tagName === 'TEMPLATE') continue;
      if (c.checkVisibility && !c.checkVisibility({ visibilityProperty: true }) && getComputedStyle(c).display !== 'contents') continue;
      if (c.tagName === 'INPUT' || c.tagName === 'TEXTAREA') parts.push(c.value);
      if (c.shadowRoot) walk(c.shadowRoot);
      walk(c);
    }
  };
  if (document.body) walk(document.body);
  const clean = (s) => s.replace(/\s+/g, ' ').trim().toLowerCase();
  return clean(parts.join(' ')).includes(clean(text));
}"#;

/// `(n)`: whether element `n` of the last `READ` is still on the page and shown.
pub const HAS_NODE: &str = r#"(n) => {
  const e = (globalThis.nibRefs || [])[n];
  return !!e && e.isConnected && (!e.checkVisibility || e.checkVisibility({ visibilityProperty: true }));
}"#;

/// Where the page is, once it has handled what was just done to it (one turn of its event loop).
pub const WHERE: &str = "() => new Promise(r => setTimeout(() => r(JSON.stringify({ url: location.href, title: document.title, ready: document.readyState })), 0))";

/// What a web tab is called: the page's title up to its first ` - `, ` | `, ` · ` or ` — `
/// ("MarketWatcher Backend - Swagger UI" is "MarketWatcher Backend"), at most 40 characters;
/// with no title, where it is served (`localhost:6006`).
pub fn tab_name(title: &str, url: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = [" - ", " | ", " · ", " — ", " – "].iter().filter_map(|sep| title.find(sep)).min().map_or(title.as_str(), |at| &title[..at]).trim();
    if !head.is_empty() {
        return head.chars().take(40).collect();
    }
    local::app_of(url).unwrap_or_else(|| "localhost".into())
}

/// `localhost:6006/path` for a tab's header: the address without scheme, query or fragment.
pub fn short_address(url: &str) -> String {
    let Some(app) = local::app_of(url) else { return String::new() };
    let path = url.split_once("://").map_or("", |(_, r)| r).split_once('/').map_or("", |(_, p)| p);
    let path = path.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
    if path.is_empty() { app } else { format!("{app}/{path}") }
}

/// Keys with a name, as WebDriver spells them.
pub mod key {
    pub const BACKSPACE: char = '\u{E003}';
    pub const TAB: char = '\u{E004}';
    pub const ENTER: char = '\u{E007}';
    pub const SHIFT: char = '\u{E008}';
    pub const CONTROL: char = '\u{E009}';
    pub const ALT: char = '\u{E00A}';
    pub const ESCAPE: char = '\u{E00C}';
    pub const PAGE_UP: char = '\u{E00E}';
    pub const PAGE_DOWN: char = '\u{E00F}';
    pub const END: char = '\u{E010}';
    pub const HOME: char = '\u{E011}';
    pub const LEFT: char = '\u{E012}';
    pub const UP: char = '\u{E013}';
    pub const RIGHT: char = '\u{E014}';
    pub const DOWN: char = '\u{E015}';
    pub const INSERT: char = '\u{E016}';
    pub const DELETE: char = '\u{E017}';
    pub const F1: char = '\u{E031}';
    pub const META: char = '\u{E03D}';
    /// Cmd on macOS, Ctrl elsewhere.
    pub const MOD: char = if cfg!(target_os = "macos") { META } else { CONTROL };
}

/// A press (`true`) or release of one key.
pub type Stroke = (bool, char);

/// Text as key strokes (`\n` is Enter, `\t` is Tab).
pub fn text_strokes(text: &str) -> Vec<Stroke> {
    text.chars()
        .map(|c| match c {
            '\n' => key::ENTER,
            '\t' => key::TAB,
            c => c,
        })
        .flat_map(|c| [(true, c), (false, c)])
        .collect()
}

/// `key` pressed with `mods` held.
pub fn press(mods: &[char], key: char) -> Vec<Stroke> {
    let mut out: Vec<Stroke> = mods.iter().map(|c| (true, *c)).collect();
    out.push((true, key));
    out.push((false, key));
    out.extend(mods.iter().rev().map(|c| (false, *c)));
    out
}

/// `shift+enter`, `mod+s`, `escape`, `a`: a chord as key strokes.
pub fn chord_strokes(chord: &str) -> Result<Vec<Stroke>> {
    let mut mods = Vec::new();
    let mut main = None;
    for part in chord.split('+') {
        let c = match part.to_ascii_lowercase().as_str() {
            "shift" => key::SHIFT,
            "ctrl" | "control" => key::CONTROL,
            "alt" | "option" => key::ALT,
            "cmd" | "meta" | "super" | "logo" => key::META,
            "mod" => key::MOD,
            name => {
                if main.is_some() {
                    bail!("a chord has one key besides its modifiers: {chord:?}");
                }
                main = Some(match name {
                    "enter" | "return" => key::ENTER,
                    "tab" => key::TAB,
                    "esc" | "escape" => key::ESCAPE,
                    "backspace" => key::BACKSPACE,
                    "delete" | "del" => key::DELETE,
                    "insert" => key::INSERT,
                    "space" => ' ',
                    "left" => key::LEFT,
                    "up" => key::UP,
                    "right" => key::RIGHT,
                    "down" => key::DOWN,
                    "pageup" => key::PAGE_UP,
                    "pagedown" => key::PAGE_DOWN,
                    "home" => key::HOME,
                    "end" => key::END,
                    f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => {
                        char::from_u32(key::F1 as u32 + f[1..].parse::<u32>().unwrap() - 1).context("a function key")?
                    }
                    _ => {
                        let mut it = part.chars();
                        match (it.next(), it.next()) {
                            (Some(c), None) => c,
                            _ => bail!("unknown key {part:?} in {chord:?}"),
                        }
                    }
                });
                continue;
            }
        };
        mods.push(c);
    }
    Ok(press(&mods, main.with_context(|| format!("{chord:?} names no key (for example shift+enter)"))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_is_named_after_its_app() {
        assert_eq!(tab_name("TensorBoard", "http://127.0.0.1:6006/"), "TensorBoard");
        assert_eq!(tab_name("MarketWatcher Backend - Swagger UI", "http://127.0.0.1:8765/docs"), "MarketWatcher Backend");
        assert_eq!(tab_name("Untitled.ipynb | JupyterLab", "http://localhost:8888/lab"), "Untitled.ipynb");
        assert_eq!(tab_name("Clarity · Results", "http://localhost:8766/"), "Clarity");
        assert_eq!(tab_name("  ", "http://127.0.0.1:6006/#x"), "localhost:6006");
        assert_eq!(tab_name("", "http://localhost/"), "localhost:80");
        assert_eq!(tab_name(&"x".repeat(90), "http://localhost/").chars().count(), 40);
        // A hyphen inside a word is not a separator.
        assert_eq!(tab_name("clarity-desktop", "http://localhost/"), "clarity-desktop");
    }

    #[test]
    fn the_header_shows_where_without_the_noise() {
        assert_eq!(short_address("http://127.0.0.1:6006/?darkMode=true#timeseries"), "localhost:6006");
        assert_eq!(short_address("http://localhost:8888/lab/tree/a.ipynb?token=abc"), "localhost:8888/lab/tree/a.ipynb");
        assert_eq!(short_address("http://127.0.0.1:8765/docs/"), "localhost:8765/docs");
        assert_eq!(short_address("https://example.com/"), "");
    }

    #[test]
    fn chords_become_strokes() {
        assert_eq!(chord_strokes("shift+enter").unwrap(), vec![(true, key::SHIFT), (true, key::ENTER), (false, key::ENTER), (false, key::SHIFT)]);
        assert_eq!(chord_strokes("a").unwrap(), vec![(true, 'a'), (false, 'a')]);
        assert_eq!(chord_strokes("mod+shift+P").unwrap().len(), 6);
        assert_eq!(chord_strokes("f5").unwrap()[0].1, '\u{E035}');
        assert_eq!(chord_strokes("+").ok(), None);
        assert!(chord_strokes("shift").is_err());
        assert!(chord_strokes("shift+nope").is_err());
        assert!(chord_strokes("a+b").is_err());
        assert_eq!(text_strokes("a\n"), vec![(true, 'a'), (false, 'a'), (true, key::ENTER), (false, key::ENTER)]);
    }
}
