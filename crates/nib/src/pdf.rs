//! PDF files (D-049): parsing, links and the outline, page rendering with hayro (off the UI
//! thread), and the viewer pane. hayro draws a page as a picture; the links and the outline are
//! read from the file's objects here.

use crate::media::human_size;
use crate::theme::Theme;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::object::{Array, Dict, MaybeRef, Name, Object, ObjectIdentifier, String as PdfString};
use hayro::hayro_syntax::page::Page;
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::kurbo::{Affine, Point as KPoint, Rect as KRect};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, PixmapSettings, RenderCache, RenderSettings};
use iced::widget::image::Handle;
use iced::widget::scrollable::{Direction, Scrollbar, Viewport};
use iced::widget::{button, column, container, image as image_widget, mouse_area, row, scrollable, sensor, space, text};
use iced::{mouse, Alignment, Background, Color, ContentFit, Element, Length, Padding, Point, Size, Vector};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

/// Space around the pages and between them, in logical pixels.
const PAD: f32 = 16.0;
const GAP: f32 = 12.0;
/// Room left for the vertical scrollbar when fitting the width.
const BAR: f32 = 14.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
/// A rendered page is at most this many pixels on a side, and this many in all.
const MAX_SIDE: f32 = 8192.0;
const MAX_PIXELS: f32 = 48e6;
/// Rendered pages kept beyond the ones on screen, on each side.
const KEEP: usize = 3;
const OUTLINE_WIDTH: f32 = 260.0;

/// Where a link or an outline entry goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// `y`: points from the top of the page, if the destination names a position.
    Page { page: usize, y: Option<f32> },
    Uri(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// `[x0, y0, x1, y1]` in points from the page's top-left corner, as the page is shown.
    pub rect: [f32; 4],
    pub target: Target,
}

#[derive(Debug, Clone)]
pub struct PageInfo {
    /// In points, as shown (rotation applied).
    pub width: f32,
    pub height: f32,
    pub links: Vec<Link>,
}

#[derive(Debug, Clone)]
pub struct OutlineItem {
    pub title: String,
    pub depth: usize,
    pub target: Option<Target>,
}

/// A parsed PDF, shared between the UI and the render tasks.
pub struct PdfDoc {
    pdf: Pdf,
    pub bytes: usize,
    pub pages: Vec<PageInfo>,
    pub outline: Vec<OutlineItem>,
}

impl std::fmt::Debug for PdfDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PdfDoc({} pages, {} outline entries)", self.pages.len(), self.outline.len())
    }
}

/// Parse a PDF and read its links and outline. Run it off the UI thread.
pub fn load(data: Vec<u8>) -> Result<PdfDoc, String> {
    catch_unwind(AssertUnwindSafe(|| load_inner(data))).unwrap_or_else(|_| Err("the PDF reader crashed on this file".into()))
}

fn load_inner(data: Vec<u8>) -> Result<PdfDoc, String> {
    let bytes = data.len();
    let pdf = Pdf::new(data).map_err(|e| match e {
        LoadPdfError::Decryption(_) => "this PDF is password-protected, which nib can't open yet".to_string(),
        LoadPdfError::Invalid => "not a valid PDF".to_string(),
    })?;
    let (pages, outline) = {
        let r = Resolver::new(&pdf);
        if r.pages.is_empty() {
            return Err("the PDF has no pages".into());
        }
        let pages = r
            .pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (width, height) = p.render_dimensions();
                PageInfo { width, height, links: r.links(i, p) }
            })
            .collect();
        (pages, r.outline())
    };
    Ok(PdfDoc { pdf, bytes, pages, outline })
}

impl PdfDoc {
    /// Draw page `page` at `scale` pixels per point (capped to keep the bitmap a sane size).
    /// Returns width, height and RGBA bytes. CPU-heavy: run it off the UI thread.
    pub fn rasterize(&self, page: usize, scale: f32) -> Result<(u32, u32, Vec<u8>), String> {
        let info = self.pages.get(page).ok_or("no such page")?;
        let scale = scale
            .min(MAX_SIDE / info.width.max(1.0))
            .min(MAX_SIDE / info.height.max(1.0))
            .min((MAX_PIXELS / (info.width * info.height).max(1.0)).sqrt())
            .max(0.01);
        catch_unwind(AssertUnwindSafe(|| {
            let p = &self.pdf.pages()[page];
            let cache = RenderCache::new();
            let settings = PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE };
            let px = render(p, &cache, &InterpreterSettings::default(), &RenderSettings::default(), &settings);
            (px.width() as u32, px.height() as u32, px.data_as_u8_slice().to_vec())
        }))
        .map_err(|_| format!("the PDF renderer crashed on page {}", page + 1))
        .and_then(|(w, h, data)| if w == 0 || h == 0 { Err("empty page".into()) } else { Ok((w, h, data)) })
    }

    /// `rasterize`, ready to draw. (The background is opaque, so premultiplied RGBA is plain RGBA.)
    pub fn render(&self, page: usize, scale: f32) -> Result<Handle, String> {
        self.rasterize(page, scale).map(|(w, h, data)| Handle::from_rgba(w, h, data))
    }
}

/// Reads destinations: explicit (`[page /XYZ left top zoom]`), named (the catalog's `/Dests` and
/// the `/Names` → `/Dests` name tree), and the `GoTo` / `URI` actions that carry them.
struct Resolver<'a> {
    pages: &'a [Page<'a>],
    by_id: HashMap<ObjectIdentifier, usize>,
    /// PDF user space → points from the top-left of the page as shown.
    transforms: Vec<Affine>,
    named: HashMap<Vec<u8>, Target>,
    root: Option<Dict<'a>>,
}

impl<'a> Resolver<'a> {
    fn new(pdf: &'a Pdf) -> Self {
        let pages: &'a [Page<'a>] = pdf.pages();
        let by_id = pages.iter().enumerate().filter_map(|(i, p)| Some((p.raw().obj_id()?, i))).collect();
        let transforms = pages.iter().map(|p| p.initial_transform(true).to_kurbo()).collect();
        let root: Option<Dict<'a>> = pdf.xref().get(pdf.xref().root_id());
        let mut r = Resolver { pages, by_id, transforms, named: HashMap::new(), root: root.clone() };
        if let Some(root) = root {
            if let Some(dests) = root.get::<Dict>(b"Dests") {
                for key in dests.keys() {
                    if let Some(t) = dests.get::<Object>(key.as_ref()).and_then(|o| r.direct(o)) {
                        r.named.insert(key.as_ref().to_vec(), t);
                    }
                }
            }
            if let Some(tree) = root.get::<Dict>(b"Names").and_then(|n| n.get::<Dict>(b"Dests")) {
                let mut named = HashMap::new();
                r.name_tree(&tree, 0, &mut named);
                r.named.extend(named);
            }
        }
        r
    }

    fn name_tree(&self, node: &Dict<'a>, depth: usize, out: &mut HashMap<Vec<u8>, Target>) {
        if depth > 32 {
            return;
        }
        if let Some(names) = node.get::<Array>(b"Names") {
            let mut it = names.flex_iter();
            while let (Some(key), Some(value)) = (it.next::<PdfString>(), it.next::<Object>()) {
                if let Some(t) = self.direct(value) {
                    out.insert(key.as_bytes().to_vec(), t);
                }
            }
        }
        if let Some(kids) = node.get::<Array>(b"Kids") {
            for kid in kids.iter::<Dict>() {
                self.name_tree(&kid, depth + 1, out);
            }
        }
    }

    /// A destination that isn't a name: an array, or a dictionary holding one under `/D`.
    fn direct(&self, obj: Object<'a>) -> Option<Target> {
        match obj {
            Object::Array(a) => self.explicit(&a),
            Object::Dict(d) => d.get::<Array>(b"D").and_then(|a| self.explicit(&a)),
            _ => None,
        }
    }

    fn dest(&self, obj: Object<'a>) -> Option<Target> {
        match obj {
            Object::Name(n) => self.named.get(n.as_ref()).cloned(),
            Object::String(s) => self.named.get(s.as_bytes()).cloned(),
            o => self.direct(o),
        }
    }

    fn explicit(&self, a: &Array<'a>) -> Option<Target> {
        let page = match a.raw_iter().next()? {
            MaybeRef::Ref(r) => *self.by_id.get(&r.into())?,
            // Page numbers instead of references are for remote destinations, but appear locally too.
            MaybeRef::NotRef(Object::Number(n)) => (n.as_i64().max(0) as usize).min(self.pages.len() - 1),
            _ => return None,
        };
        let mut it = a.flex_iter();
        it.next::<Object>();
        let kind = it.next::<Name>();
        let args: Vec<Option<f32>> = std::iter::from_fn(|| it.next::<Object>())
            .take(4)
            .map(|o| match o {
                Object::Number(n) => Some(n.as_f32()),
                _ => None,
            })
            .collect();
        let arg = |i: usize| args.get(i).copied().flatten();
        let (x, top) = match kind.as_ref().map(|k| k.as_ref()) {
            Some(b"XYZ") => (arg(0), arg(1)),
            Some(b"FitH") | Some(b"FitBH") => (None, arg(0)),
            Some(b"FitR") => (arg(0), arg(3)),
            _ => (None, None),
        };
        let y = top.map(|top| {
            let p = self.transforms[page] * KPoint::new(x.unwrap_or(0.0) as f64, top as f64);
            (p.y as f32).clamp(0.0, self.pages[page].render_dimensions().1)
        });
        Some(Target::Page { page, y })
    }

    fn action(&self, a: &Dict<'a>) -> Option<Target> {
        let kind = a.get::<Name>(b"S")?;
        match kind.as_ref() {
            b"GoTo" => self.dest(a.get::<Object>(b"D")?),
            b"URI" => {
                let uri = String::from_utf8_lossy(a.get::<PdfString>(b"URI")?.as_bytes()).trim().to_string();
                (!uri.is_empty()).then_some(Target::Uri(uri))
            }
            _ => None,
        }
    }

    fn target_of(&self, d: &Dict<'a>) -> Option<Target> {
        d.get::<Object>(b"Dest").and_then(|o| self.dest(o)).or_else(|| d.get::<Dict>(b"A").and_then(|a| self.action(&a)))
    }

    fn links(&self, index: usize, page: &Page<'a>) -> Vec<Link> {
        let Some(annots) = page.raw().get::<Array>(b"Annots") else { return Vec::new() };
        let mut out = Vec::new();
        for a in annots.iter::<Dict>() {
            if a.get::<Name>(b"Subtype").is_none_or(|s| s.as_ref() != b"Link") {
                continue;
            }
            let Some(r) = a.get::<[f32; 4]>(b"Rect") else { continue };
            let Some(target) = self.target_of(&a) else { continue };
            let b = self.transforms[index].transform_rect_bbox(KRect::new(r[0] as f64, r[1] as f64, r[2] as f64, r[3] as f64));
            out.push(Link { rect: [b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32], target });
        }
        out
    }

    fn outline(&self) -> Vec<OutlineItem> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        if let Some(top) = self.root.as_ref().and_then(|r| r.get::<Dict>(b"Outlines")) {
            self.outline_level(&top, 0, &mut out, &mut seen);
        }
        out
    }

    fn outline_level(&self, parent: &Dict<'a>, depth: usize, out: &mut Vec<OutlineItem>, seen: &mut HashSet<ObjectIdentifier>) {
        let mut cur = parent.get::<Dict>(b"First");
        while let Some(item) = cur {
            // Broken files can loop.
            if depth > 32 || out.len() >= 20_000 || item.obj_id().is_some_and(|id| !seen.insert(id)) {
                return;
            }
            let title = item.get::<PdfString>(b"Title").map(|s| text_string(s.as_bytes())).unwrap_or_default();
            out.push(OutlineItem { title, depth, target: self.target_of(&item) });
            self.outline_level(&item, depth + 1, out, seen);
            cur = item.get::<Dict>(b"Next");
        }
    }
}

/// A PDF "text string": UTF-16 with a byte-order mark, UTF-8 with one, else PDFDocEncoding (read
/// as Latin-1, which it matches for printable text). Whitespace runs become one space.
pub fn text_string(b: &[u8]) -> String {
    let s = if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>())
    } else if let Some(rest) = b.strip_prefix(&[0xFF, 0xFE]) {
        String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>())
    } else if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(rest).into_owned()
    } else {
        b.iter().map(|&c| c as char).collect()
    };
    s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Only these open outside nib: a PDF can name any URL, and `open` would run an app for `file:`.
pub fn safe_uri(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    ["http://", "https://", "mailto:"].iter().any(|p| lower.starts_with(p))
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(u64, Result<Arc<PdfDoc>, String>),
    Rendered { tab: u64, page: usize, scale: f32, result: Result<Handle, String> },
    /// The window's scale factor (pixels per logical pixel).
    Dpi(f32),
    Resized(Size),
    Scrolled(Viewport),
    Hover(usize, Point),
    Unhover,
    Click,
    Outline(usize),
    /// Back to fitting the page width.
    Fit,
}

pub fn scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("pdf")
}

/// An open PDF: zoom, scroll position, and the pages rendered so far.
pub struct PdfView {
    pub doc: Arc<PdfDoc>,
    /// `None`: fit the widest page to the pane.
    zoom: Option<f32>,
    /// The scrolling pane, once laid out.
    size: Size,
    pub offset: Vector,
    pub dpi: f32,
    rendered: HashMap<usize, (f32, Handle)>,
    pending: HashSet<usize>,
    failed: HashMap<usize, String>,
    /// The pointer, over page `.0` at `.1` (logical pixels from the page's corner).
    hover: Option<(usize, Point)>,
    pub outline_open: bool,
}

impl PdfView {
    pub fn new(doc: Arc<PdfDoc>, dpi: f32) -> Self {
        let outline_open = !doc.outline.is_empty();
        PdfView {
            doc,
            zoom: None,
            size: Size::ZERO,
            offset: Vector::ZERO,
            dpi,
            rendered: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            hover: None,
            outline_open,
        }
    }

    fn widest(&self) -> f32 {
        self.doc.pages.iter().map(|p| p.width).fold(1.0, f32::max)
    }

    /// Logical pixels per point.
    pub fn zoom(&self) -> f32 {
        match self.zoom {
            Some(z) => z,
            None if self.size.width > 0.0 => ((self.size.width - 2.0 * PAD - BAR) / self.widest()).clamp(MIN_ZOOM, MAX_ZOOM),
            None => 1.0,
        }
    }

    pub fn fits(&self) -> bool {
        self.zoom.is_none()
    }

    /// Top of each page in the scrolled content, at zoom `z`.
    fn tops(&self, z: f32) -> Vec<f32> {
        let mut y = PAD;
        self.doc
            .pages
            .iter()
            .map(|p| {
                let top = y;
                y += p.height * z + GAP;
                top
            })
            .collect()
    }

    fn content_height(&self) -> f32 {
        let z = self.zoom();
        PAD * 2.0 + self.doc.pages.iter().map(|p| p.height * z).sum::<f32>() + GAP * (self.doc.pages.len() - 1) as f32
    }

    fn content_width(&self) -> f32 {
        (self.widest() * self.zoom() + 2.0 * PAD).max(self.size.width - BAR)
    }

    /// The page at content height `y`.
    fn page_at(&self, tops: &[f32], y: f32) -> usize {
        tops.partition_point(|&t| t <= y).saturating_sub(1)
    }

    /// First and last page on screen.
    pub fn visible(&self) -> (usize, usize) {
        let tops = self.tops(self.zoom());
        let last = self.page_at(&tops, self.offset.y + self.size.height.max(1.0));
        let first = self.page_at(&tops, self.offset.y);
        // A gap between pages counts as the page above it.
        (first, last.max(first))
    }

    /// The page being read: the one a third of the way down the pane.
    pub fn current(&self) -> usize {
        self.page_at(&self.tops(self.zoom()), self.offset.y + self.size.height / 3.0)
    }

    /// Pages to render now (marked pending; the caller starts the work), at what scale.
    /// Also forgets rendered pages far from the screen.
    pub fn wants(&mut self) -> Vec<(usize, f32)> {
        if self.size.height <= 0.0 {
            return Vec::new();
        }
        let scale = self.zoom() * self.dpi;
        let (first, last) = self.visible();
        let (lo, hi) = (first.saturating_sub(1), (last + 1).min(self.doc.pages.len() - 1));
        let mut out = Vec::new();
        // Pages on screen first.
        for page in (first..=last).chain(lo..first).chain(last + 1..=hi) {
            let fresh = self.rendered.get(&page).is_some_and(|(s, _)| (s - scale).abs() / scale < 0.05);
            if fresh || self.pending.contains(&page) || self.failed.contains_key(&page) {
                continue;
            }
            self.pending.insert(page);
            out.push((page, scale));
        }
        let (keep_lo, keep_hi) = (first.saturating_sub(KEEP), last + KEEP);
        self.rendered.retain(|p, _| (keep_lo..=keep_hi).contains(p));
        out
    }

    pub fn on_rendered(&mut self, page: usize, scale: f32, result: Result<Handle, String>) {
        self.pending.remove(&page);
        match result {
            Ok(h) => {
                self.rendered.insert(page, (scale, h));
            }
            Err(e) => {
                self.failed.insert(page, e);
            }
        }
    }

    /// Keep the same spot on the same page in view while `f` changes the zoom or the pane.
    /// Returns the new scroll offset if it moved.
    fn keep_place(&mut self, f: impl FnOnce(&mut Self)) -> Option<Vector> {
        let (z0, tops0) = (self.zoom(), self.tops(self.zoom()));
        let page = self.page_at(&tops0, self.offset.y);
        let within = (self.offset.y - tops0[page]) / z0;
        let x = (self.offset.x + self.size.width / 2.0) / self.content_width();
        f(self);
        let z1 = self.zoom();
        if (z1 - z0).abs() < 1e-4 {
            return None;
        }
        let y = self.tops(z1)[page] + within * z1;
        let x = (x * self.content_width() - self.size.width / 2.0).max(0.0);
        self.offset = Vector::new(if self.fits() { 0.0 } else { x }, self.clamp_y(y));
        Some(self.offset)
    }

    pub fn set_zoom(&mut self, zoom: Option<f32>) -> Option<Vector> {
        self.keep_place(|v| v.zoom = zoom.map(|z| z.clamp(MIN_ZOOM, MAX_ZOOM)))
    }

    pub fn zoom_by(&mut self, factor: f32) -> Option<Vector> {
        let z = self.zoom() * factor;
        self.set_zoom(Some(z))
    }

    pub fn resize(&mut self, size: Size) -> Option<Vector> {
        self.keep_place(|v| v.size = size)
    }

    fn clamp_y(&self, y: f32) -> f32 {
        y.clamp(0.0, (self.content_height() - self.size.height).max(0.0))
    }

    /// Scroll by `(dx, dy)` logical pixels; returns where to.
    pub fn scroll_by(&mut self, dx: f32, dy: f32) -> Vector {
        let max_x = (self.content_width() - self.size.width).max(0.0);
        self.offset = Vector::new((self.offset.x + dx).clamp(0.0, max_x), self.clamp_y(self.offset.y + dy));
        self.offset
    }

    /// Scroll so `target`'s page (and position on it) is at the top; returns where to.
    pub fn go_to(&mut self, page: usize, y: Option<f32>) -> Vector {
        let page = page.min(self.doc.pages.len() - 1);
        let z = self.zoom();
        let top = self.tops(z)[page];
        let y = match y {
            Some(y) => top + y * z - GAP,
            None => top - GAP / 2.0,
        };
        self.offset.y = self.clamp_y(y);
        self.offset
    }

    pub fn page_height(&self) -> f32 {
        self.size.height
    }

    pub fn on_scrolled(&mut self, v: Viewport) {
        let o = v.absolute_offset();
        self.offset = Vector::new(o.x, o.y);
        self.size = v.bounds().size();
    }

    pub fn set_hover(&mut self, hover: Option<(usize, Point)>) {
        self.hover = hover;
    }

    /// The link under the pointer.
    pub fn hovered_link(&self) -> Option<&Link> {
        let (page, p) = self.hover?;
        let z = self.zoom();
        let (x, y) = (p.x / z, p.y / z);
        self.doc.pages.get(page)?.links.iter().find(|l| x >= l.rect[0] && x <= l.rect[2] && y >= l.rect[1] && y <= l.rect[3])
    }

    pub fn status(&self) -> String {
        let n = self.doc.pages.len();
        let mut s = format!("page {} / {n} · {:.0}%", self.current() + 1, self.zoom() * 100.0);
        if self.fits() {
            s.push_str(" (fit width)");
        }
        s.push_str(&format!(" · {}", human_size(self.doc.bytes)));
        if let Some((page, e)) = self.failed.iter().min_by_key(|(p, _)| **p) {
            s.push_str(&format!(" · page {} failed: {e}", page + 1));
        }
        match self.hovered_link().map(|l| &l.target) {
            Some(Target::Page { page, .. }) => s.push_str(&format!(" · → page {}", page + 1)),
            Some(Target::Uri(u)) => s.push_str(&format!(" · → {u}")),
            None => {}
        }
        s
    }

    pub fn state_json(&self) -> Value {
        let (first, last) = self.visible();
        json!({
            "page": self.current() + 1,
            "pages": self.doc.pages.len(),
            "visible": [first + 1, last + 1],
            "zoom": self.zoom(),
            "fit_width": self.fits(),
            "rendered": self.rendered.len(),
            "failed": self.failed.iter().map(|(p, e)| json!({ "page": p + 1, "error": e })).collect::<Vec<_>>(),
            "links": self.doc.pages.iter().map(|p| p.links.len()).sum::<usize>(),
            "outline": self.doc.outline.iter().map(|o| json!({
                "title": o.title,
                "depth": o.depth,
                "page": match &o.target { Some(Target::Page { page, .. }) => json!(page + 1), _ => Value::Null },
            })).collect::<Vec<_>>(),
            "outline_open": self.outline_open,
        })
    }

    pub fn view(&self, th: &Theme) -> Element<'_, Msg> {
        let z = self.zoom();
        let hover_link = self.hovered_link().is_some();
        let pages = self.doc.pages.iter().enumerate().map(|(i, info)| {
            let (w, h) = (info.width * z, info.height * z);
            let body: Element<'_, Msg> = match self.rendered.get(&i) {
                Some((_, handle)) => image_widget(handle.clone())
                    .width(Length::Fixed(w))
                    .height(Length::Fixed(h))
                    .content_fit(ContentFit::Fill)
                    .into(),
                // Not rendered yet: a blank sheet of the right size keeps the scroll position honest.
                None => container(space())
                    .width(Length::Fixed(w))
                    .height(Length::Fixed(h))
                    .style(|_| container::Style::default().background(Color::WHITE))
                    .into(),
            };
            let mut area = mouse_area(body).on_move(move |p| Msg::Hover(i, p)).on_exit(Msg::Unhover).on_press(Msg::Click);
            if hover_link && self.hover.is_some_and(|(p, _)| p == i) {
                area = area.interaction(mouse::Interaction::Pointer);
            }
            area.into()
        });
        let content = column(pages)
            .spacing(GAP)
            .padding(PAD)
            .align_x(Alignment::Center)
            .width(Length::Fixed(self.content_width()));
        let bg = th.terminal_bg;
        let pane = scrollable(content)
            .direction(Direction::Both { vertical: Scrollbar::default(), horizontal: Scrollbar::default() })
            .id(scroll_id())
            .on_scroll(Msg::Scrolled)
            .style(crate::theme::scrollbar)
            .width(Length::Fill)
            .height(Length::Fill);
        let pane = container(sensor(pane).on_show(Msg::Resized).on_resize(Msg::Resized))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style::default().background(bg));
        if !self.outline_open || self.doc.outline.is_empty() {
            return pane.into();
        }
        let (fg, dim, hl, current) = (th.foreground, th.dim, th.current_line, self.current());
        // The entry for the page being read: the last one that starts at or before it.
        let active = self
            .doc
            .outline
            .iter()
            .enumerate()
            .filter_map(|(i, o)| match o.target {
                Some(Target::Page { page, .. }) if page <= current => Some((page, i)),
                _ => None,
            })
            .max_by_key(|&(page, i)| (page, i))
            .map(|(_, i)| i);
        let items = self.doc.outline.iter().enumerate().map(|(i, o)| {
            let color = if o.target.is_some() { fg } else { dim };
            let on = active == Some(i);
            button(text(o.title.clone()).size(12).color(color))
                .on_press_maybe(o.target.as_ref().map(|_| Msg::Outline(i)))
                .width(Length::Fill)
                .padding(Padding { top: 3.0, bottom: 3.0, left: 10.0 + 14.0 * o.depth.min(8) as f32, right: 8.0 })
                .style(move |_, status| button::Style {
                    background: (on || matches!(status, button::Status::Hovered)).then_some(Background::Color(hl)),
                    text_color: color,
                    ..Default::default()
                })
                .into()
        });
        let panel = th.panel;
        let side = container(scrollable(column(items).padding([6, 0])).style(crate::theme::scrollbar).height(Length::Fill))
            .width(Length::Fixed(OUTLINE_WIDTH))
            .height(Length::Fill)
            .style(move |_| container::Style::default().background(panel));
        let rule = container(space()).width(Length::Fixed(1.0)).height(Length::Fill).style({
            let c = th.divider;
            move |_| container::Style::default().background(c)
        });
        row![side, rule, pane].into()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small PDF, written by hand: two 200×300 pt pages; page 1 has a blue square, a link to a
    /// spot on page 2 (by name), and a web link; the outline has two entries, one titled in UTF-16.
    pub(crate) fn sample() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R /Outlines 7 0 R /Names << /Dests << /Names [(sec2) [4 0 R /XYZ 0 250 null]] >> >> >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Contents 5 0 R /Annots [6 0 R 10 0 R] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] >>",
            "<< /Length 26 >>\nstream\n0 0 1 rg 20 200 60 60 re f\nendstream",
            "<< /Type /Annot /Subtype /Link /Rect [20 200 80 260] /Dest (sec2) >>",
            "<< /Type /Outlines /First 8 0 R /Last 9 0 R /Count 2 >>",
            "<< /Title (Intro) /Parent 7 0 R /Next 9 0 R /Dest [3 0 R /Fit] >>",
            "<< /Title <FEFF00C9007400650020> /Parent 7 0 R /Prev 8 0 R /A << /S /GoTo /D [4 0 R /FitH 100] >> >>",
            "<< /Type /Annot /Subtype /Link /Rect [100 10 190 30] /A << /S /URI /URI (https://example.com/x) >> >>",
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for o in offsets {
            out.extend(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
        out
    }

    #[test]
    fn reads_pages_links_and_outline() {
        let doc = load(sample()).unwrap();
        assert_eq!(doc.pages.len(), 2);
        assert_eq!((doc.pages[0].width, doc.pages[0].height), (200.0, 300.0));
        // PDF space is y-up from the bottom; links come out y-down from the top.
        assert_eq!(
            doc.pages[0].links,
            vec![
                Link { rect: [20.0, 40.0, 80.0, 100.0], target: Target::Page { page: 1, y: Some(50.0) } },
                Link { rect: [100.0, 270.0, 190.0, 290.0], target: Target::Uri("https://example.com/x".into()) },
            ]
        );
        let titles: Vec<_> = doc.outline.iter().map(|o| (o.title.as_str(), o.depth, o.target.clone())).collect();
        assert_eq!(
            titles,
            vec![
                ("Intro", 0, Some(Target::Page { page: 0, y: None })),
                ("Éte", 0, Some(Target::Page { page: 1, y: Some(200.0) })),
            ]
        );
        assert!(load(b"not a pdf".to_vec()).is_err());
    }

    #[test]
    fn renders_pages() {
        let doc = load(sample()).unwrap();
        let (w, h, data) = doc.rasterize(0, 2.0).unwrap();
        assert_eq!((w, h), (400, 600));
        let px = |x: u32, y: u32| &data[((y * w + x) * 4) as usize..][..4];
        // Inside the blue square (x 20–80, y 40–100 from the top, doubled), and the white paper.
        assert_eq!(px(100, 140), [0, 0, 255, 255]);
        assert_eq!(px(10, 10), [255, 255, 255, 255]);
        // Absurd scales are capped.
        let (w, h, _) = doc.rasterize(1, 1000.0).unwrap();
        assert!(w as f32 <= MAX_SIDE && h as f32 <= MAX_SIDE && (w * h) as f32 <= MAX_PIXELS * 1.01);
    }

    #[test]
    fn text_strings() {
        assert_eq!(text_string(b"\xFE\xFF\x00A\x00\n\x00B"), "A B");
        assert_eq!(text_string(b"Caf\xE9  au\tlait"), "Café au lait");
        assert_eq!(text_string(b"\xEF\xBB\xBF\xE2\x9C\x93 done"), "✓ done");
        assert!(safe_uri("HTTPS://x.org") && safe_uri("mailto:a@b.c"));
        assert!(!safe_uri("file:///Applications/Calculator.app") && !safe_uri("javascript:alert(1)"));
    }

    #[test]
    fn layout_scrolling_and_zoom() {
        let mut v = PdfView::new(Arc::new(load(sample()).unwrap()), 2.0);
        assert!(v.outline_open);
        // Not laid out yet: nothing to render.
        assert!(v.wants().is_empty());
        // 200 pt wide page in a pane 230 + scrollbar wide: fits at 1.0.
        v.resize(Size::new(200.0 + 2.0 * PAD + BAR, 200.0));
        assert_eq!(v.zoom(), 1.0);
        assert_eq!(v.visible(), (0, 0));
        assert_eq!(v.wants(), vec![(0, 2.0), (1, 2.0)]);
        assert!(v.wants().is_empty(), "pending pages aren't asked for twice");
        v.on_rendered(0, 2.0, Ok(Handle::from_rgba(1, 1, vec![0; 4])));

        // Links are hit in page coordinates.
        v.set_hover(Some((0, Point::new(50.0, 70.0))));
        assert_eq!(v.hovered_link().unwrap().target, Target::Page { page: 1, y: Some(50.0) });
        assert!(v.status().ends_with("→ page 2"));
        v.set_hover(Some((0, Point::new(150.0, 150.0))));
        assert!(v.hovered_link().is_none());

        // Page 2 starts at PAD + 300 + GAP; a position on it lands just below the top of the pane.
        let o = v.go_to(1, Some(50.0));
        assert_eq!(o.y, PAD + 300.0 + GAP + 50.0 - GAP);
        assert_eq!(v.current(), 1);

        // Zooming keeps the same spot of the same page at the top.
        let before = v.offset.y;
        let o = v.zoom_by(2.0).unwrap();
        assert_eq!(v.zoom(), 2.0);
        assert!(!v.fits());
        let tops = v.tops(2.0);
        assert!((o.y - (tops[1] + (before - (PAD + 300.0 + GAP)) * 2.0)).abs() < 0.01);
        v.set_zoom(None);
        assert!(v.fits() && v.zoom() == 1.0);
        // Scrolling stops at the ends.
        assert_eq!(v.scroll_by(0.0, -1e6).y, 0.0);
        assert_eq!(v.scroll_by(0.0, 1e6).y, v.content_height() - 200.0);
    }
}
