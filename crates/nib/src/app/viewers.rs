//! Non-text views of a tab: images, the CSV table, and the Markdown / HTML preview.

use super::{App, Focus, Message, Pending, Tab};
use crate::editor_view::EditorView;
use crate::math;
use crate::media::{self, Kind, Media};
use crate::preview::{self, block_key, Place, SourceMap, Target, WIDTH_TITLE};
use crate::table::Table;
use crate::table_view::{total_cols, TableEvent, TableView};
use iced::widget::image::Handle;
use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{tree, Tree, Widget};
use iced::advanced::{renderer, Clipboard, Shell};
use iced::widget::operation::{scroll_to, AbsoluteOffset};
use iced::widget::{
    button, column, container, image as image_widget, markdown, mouse_area, row, scrollable, space, svg, text,
};
use iced::{mouse, Border, ContentFit, Element, Event, Length, Rectangle, Size, Task};
use nib_editor::{Command, Document};
use nib_proto::Request;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The rendered side of a Markdown / HTML tab.
pub struct Preview {
    content: markdown::Content,
    /// The typeset formulas `content` refers to.
    math: math::Formulas,
    /// `Document::version` the content was parsed from.
    pub(super) version: u64,
    /// Images the document refers to, by the URL written in it. `None`: requested, not here (yet).
    images: HashMap<String, Option<PreviewImage>>,
    /// Which source position each rendered block comes from.
    map: SourceMap,
    /// Top of each top-level block in the rendered body, as last laid out (written by `Probe`).
    tops: RefCell<Vec<f32>>,
    pub(super) scroll: PreviewScroll,
}

/// Keeps the editor and the preview scrolled to the same place.
#[derive(Default, Clone, Copy)]
pub(super) struct PreviewScroll {
    /// The preview's scroll offset, as last reported or asked for.
    y: f32,
    /// Its visible height and the height of everything in it.
    view_h: f32,
    content_h: f32,
    /// `Tab::top` the two were last lined up at; the preview follows when the editor moves off it.
    synced_top: Option<usize>,
}

/// The rendered body sits this far inside the scrollable.
const PREVIEW_PAD: f32 = 18.0;

fn preview_id() -> iced::widget::Id {
    iced::widget::Id::new("nib-preview")
}

/// Piecewise-linear lookup in `points` (sorted by both coordinates): the second coordinate at `x`.
fn interpolate(points: &[(f32, f32)], x: f32) -> f32 {
    let i = points.partition_point(|p| p.0 <= x);
    match (i.checked_sub(1).map(|j| points[j]), points.get(i)) {
        (Some(a), Some(b)) if b.0 > a.0 => a.1 + (x - a.0) / (b.0 - a.0) * (b.1 - a.1),
        (Some(a), _) => a.1,
        (None, Some(b)) => b.1,
        (None, None) => 0.0,
    }
}

impl Preview {
    /// (source line, preview offset) pairs that should be at the top of their panes together.
    fn anchors(&self, lines: usize) -> Vec<(f32, f32)> {
        let tops = self.tops.borrow();
        let max_y = (self.scroll.content_h - self.scroll.view_h).max(0.0);
        let mut pts = vec![(0.0, 0.0)];
        if tops.len() == self.map.top.len() {
            for (&line, &y) in self.map.top.iter().zip(tops.iter()) {
                let last = *pts.last().unwrap();
                let (line, y) = (line as f32, (y + PREVIEW_PAD).min(max_y));
                // Keep both coordinates increasing (a block can't be above its predecessor).
                if line >= last.0 && y >= last.1 {
                    pts.push((line, y));
                }
            }
        }
        // The editor's last possible top line goes with the preview scrolled to the bottom.
        let last = *pts.last().unwrap();
        let end = lines.saturating_sub(1) as f32;
        if end > last.0 && max_y >= last.1 {
            pts.push((end, max_y));
        }
        pts
    }
}

/// Lays out its content unchanged and reports where the content's children ended up.
struct Probe<'a, Message> {
    content: Element<'a, Message>,
    tops: &'a RefCell<Vec<f32>>,
    /// What each child of `content` holds; empty: one top-level block each.
    shapes: Vec<Shape>,
}

/// What one child of the preview's body is made of.
#[derive(Clone, Copy)]
enum Shape {
    /// A column of top-level blocks, one child each.
    Run,
    /// One element for this many top-level blocks (centered, or a row of columns).
    One(usize),
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Probe<'_, Message> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }
    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }
    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        let node = self.content.as_widget_mut().layout(tree, renderer, limits);
        let mut tops = Vec::new();
        if self.shapes.is_empty() {
            tops.extend(node.children().iter().map(|c| c.bounds().y));
        }
        for (c, shape) in node.children().iter().zip(&self.shapes) {
            match *shape {
                Shape::Run => tops.extend(c.children().iter().map(|b| c.bounds().y + b.bounds().y)),
                Shape::One(n) => tops.extend(std::iter::repeat_n(c.bounds().y, n)),
            }
        }
        *self.tops.borrow_mut() = tops;
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<iced::advanced::overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message: 'a> From<Probe<'a, Message>> for Element<'a, Message> {
    fn from(p: Probe<'a, Message>) -> Self {
        Element::new(p)
    }
}

/// The picture next to an SVG's source.
pub struct SvgPreview {
    /// The last text that parsed: kept while an edit leaves the file invalid.
    pub(super) handle: Option<svg::Handle>,
    /// Why the current text doesn't parse.
    pub(super) error: Option<String>,
    /// `Document::version` the preview was made from.
    pub(super) version: u64,
}

/// A decoded image in a preview: raster formats go through `media::decode`, SVG is drawn as vectors.
#[derive(Clone)]
pub(super) enum PreviewImage {
    Raster(Handle),
    Svg(svg::Handle),
}

impl PreviewImage {
    /// Decoded here (not lazily by the renderer) so the image has its size from the first frame.
    pub(super) fn decode(url: &str, data: Vec<u8>) -> Option<PreviewImage> {
        if is_svg(url) {
            Some(PreviewImage::Svg(svg::Handle::from_memory(data)))
        } else {
            media::decode(&data).ok().and_then(|d| d.frames.into_iter().next()).map(|f| PreviewImage::Raster(f.handle))
        }
    }

    /// Whether a file can be shown as one: a raster format nib views, or SVG.
    pub(super) fn wanted(path: &str) -> bool {
        Kind::of(Path::new(path)) == Kind::Image || is_svg(path)
    }

    /// Its natural size, shrunk to fit the space it gets.
    pub(super) fn view<'a>(&self) -> Element<'a, Message> {
        self.view_at(None)
    }

    /// As `view`, or this wide (less if the space is narrower).
    fn view_at<'a>(&self, width: Option<f32>) -> Element<'a, Message> {
        if let Some(w) = width {
            let fill: Element<'a, Message> = match self {
                PreviewImage::Raster(h) => image_widget(h.clone()).width(Length::Fill).content_fit(ContentFit::Contain).into(),
                PreviewImage::Svg(h) => svg(h.clone()).width(Length::Fill).height(Length::Shrink).content_fit(ContentFit::Contain).into(),
            };
            // Fixed, not Fill, so a centered block can shrink to it; `max_width` shrinks it with the pane.
            return container(fill).width(Length::Fixed(w)).max_width(w).into();
        }
        match self {
            PreviewImage::Raster(handle) => image_widget(handle.clone()).content_fit(ContentFit::ScaleDown).into(),
            PreviewImage::Svg(handle) => {
                svg(handle.clone()).width(Length::Shrink).height(Length::Shrink).content_fit(ContentFit::ScaleDown).into()
            }
        }
    }
}

fn is_svg(path: &str) -> bool {
    Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("svg"))
}

/// Draws preview images that have arrived; everything else is Iced's default Markdown rendering.
struct Viewer<'a> {
    images: &'a HashMap<String, Option<PreviewImage>>,
    /// The editor's code font and size, for code blocks.
    code: (iced::Font, f32),
    code_color: iced::Color,
    /// Source position of each block, by `preview::block_key`.
    targets: &'a HashMap<usize, usize>,
    math: &'a math::Formulas,
    /// The text's color, for formulas.
    color: iced::Color,
}

impl<'a> Viewer<'a> {
    /// A click on the block puts the editor's cursor where it comes from.
    fn clickable(&self, key: usize, el: Element<'a, Message>) -> Element<'a, Message> {
        match self.targets.get(&key) {
            Some(&pos) => mouse_area(el).on_press(Message::PreviewClick(pos)).into(),
            None => el,
        }
    }
}

/// The text size `markdown::heading` uses for `level`.
fn heading_size(settings: markdown::Settings, level: &markdown::HeadingLevel) -> f32 {
    use markdown::HeadingLevel as H;
    match level {
        H::H1 => settings.h1_size,
        H::H2 => settings.h2_size,
        H::H3 => settings.h3_size,
        H::H4 => settings.h4_size,
        H::H5 => settings.h5_size,
        H::H6 => settings.h6_size,
    }
    .0
}

/// The preview body: Iced's rendering, with the blocks HTML centered or put side by side
/// (`places`) taken out of the flow and set where it asked.
fn laid_out<'a>(
    items: &'a [markdown::Item],
    places: &[Place],
    settings: markdown::Settings,
    viewer: &Viewer<'a>,
) -> (Element<'a, Message>, Vec<Shape>) {
    if places.len() != items.len() {
        return (markdown::view_with(items, settings, viewer), Vec::new());
    }
    let (mut kids, mut shapes, mut i) = (Vec::new(), Vec::new(), 0);
    while i < items.len() {
        let end = |from: usize, same: &dyn Fn(Place) -> bool| (from..items.len()).find(|&j| !same(places[j])).unwrap_or(items.len());
        match places[i] {
            Place::Flow => {
                let j = end(i, &|p| p == Place::Flow);
                kids.push(markdown::view_with(&items[i..j], settings, viewer));
                shapes.push(Shape::Run);
                i = j;
            }
            Place::Center => {
                kids.push(container(markdown::view_with(&items[i..=i], settings, viewer)).center_x(Length::Fill).into());
                shapes.push(Shape::One(1));
                i += 1;
            }
            Place::Cell { row: r, .. } => {
                let j = end(i, &|p| matches!(p, Place::Cell { row, .. } if row == r));
                let mut cols: Vec<Element<'a, Message>> = Vec::new();
                let mut from = i;
                while from < j {
                    let to = (from..j).find(|&k| places[k] != places[from]).unwrap_or(j);
                    cols.push(container(markdown::view_with(&items[from..to], settings, viewer)).width(Length::FillPortion(1)).into());
                    from = to;
                }
                kids.push(row(cols).spacing(settings.spacing.0 * 2.0).into());
                shapes.push(Shape::One(j - i));
                i = j;
            }
        }
    }
    (column(kids).spacing(settings.spacing.0).into(), shapes)
}

impl<'a> markdown::Viewer<'a, Message> for Viewer<'a> {
    fn on_link_click(url: markdown::Uri) -> Message {
        Message::PreviewLink(url)
    }

    fn image(
        &self,
        settings: markdown::Settings,
        url: &'a markdown::Uri,
        title: &'a str,
        alt: &markdown::Text,
    ) -> Element<'a, Message> {
        if let Some(f) = math::get(Some(self.math), url) {
            let pad = settings.spacing.0 / 2.0;
            return self.clickable(block_key(url), container(math::display(f, settings.text_size.0, self.color)).padding([pad, 0.0]).into());
        }
        let el = match self.images.get(url) {
            // Intrinsic size (an SVG's width/height attributes), shrunk to fit the pane.
            Some(Some(img)) => img.view_at(title.strip_prefix(WIDTH_TITLE).and_then(|w| w.parse().ok())),
            _ => {
                container(iced::widget::rich_text(alt.spans(settings.style)).on_link_click(Self::on_link_click))
                    .padding(settings.spacing.0)
                    .style(container::dark)
                    .into()
            }
        };
        self.clickable(block_key(url), el)
    }

    fn heading(
        &self,
        settings: markdown::Settings,
        level: &'a markdown::HeadingLevel,
        text: &'a markdown::Text,
        index: usize,
    ) -> Element<'a, Message> {
        let size = heading_size(settings, level);
        let el = match math::rich(&text.spans(settings.style), Some(self.math), size, self.color, Self::on_link_click) {
            Some(el) => container(el).padding(iced::padding::top(if index > 0 { settings.text_size.0 / 2.0 } else { 0.0 })).into(),
            None => markdown::heading(settings, level, text, index, Self::on_link_click),
        };
        self.clickable(block_key(text), el)
    }

    fn paragraph(&self, settings: markdown::Settings, text: &markdown::Text) -> Element<'a, Message> {
        let el = math::rich(&text.spans(settings.style), Some(self.math), settings.text_size.0, self.color, Self::on_link_click)
            .unwrap_or_else(|| markdown::paragraph(settings, text, Self::on_link_click));
        self.clickable(block_key(text), el)
    }

    fn table(
        &self,
        settings: markdown::Settings,
        columns: &'a [markdown::Column],
        rows: &'a [markdown::Row],
    ) -> Element<'a, Message> {
        self.clickable(block_key(columns), markdown::table(self, settings, columns, rows))
    }

    /// Code blocks in the editor's own font, scrolling sideways instead of wrapping.
    fn code_block(
        &self,
        settings: markdown::Settings,
        _language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Message> {
        let (font, size) = self.code;
        let body = text(code.trim_end_matches('\n')).font(font).size(size).color(self.code_color);
        let el = container(
            scrollable(container(body).padding(settings.spacing.0)).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::default().width(4).scroller_width(4),
            ))
            .style(crate::theme::scrollbar),
        )
        .width(Length::Fill)
        .style(container::dark)
        .into();
        self.clickable(block_key(code), el)
    }
}

impl Tab {
    pub(super) fn new(id: u64, doc: Document) -> Self {
        let kind = doc.path.as_deref().map_or(Kind::Text, Kind::of);
        Tab {
            id,
            doc,
            top: 0,
            left: 0,
            local: false,
            kind,
            // A CSV opens as its table, an SVG next to its picture; other previews on request.
            rendered: matches!(kind, Kind::Csv | Kind::Svg),
            media: None,
            table: None,
            table_version: 0,
            preview: None,
            svg: None,
            lsp_synced: None,
        }
    }

    /// Shows something other than the editable text, so typing and cursor keys don't apply.
    pub(super) fn passive(&self) -> bool {
        self.media.is_some() || (self.kind == Kind::Csv && self.rendered)
    }

    fn csv_delimiter(&self) -> Option<u8> {
        let ext = self.doc.path.as_ref().and_then(|p| p.extension()).and_then(|e| e.to_str()).unwrap_or("");
        matches!(ext.to_ascii_lowercase().as_str(), "tsv" | "tab").then_some(b'\t')
    }
}

pub(super) fn open_external(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = cmd.arg(url).spawn();
}

impl App {
    /// Open `path` from the daemon, or switch to its tab if it is already open.
    pub(super) fn open_path(&mut self, path: String, new_tab: bool) -> Task<Message> {
        self.open_path_at(path, new_tab, None)
    }

    /// `open_path`, then put the cursor at `goto`.
    pub(super) fn open_path_at(&mut self, path: String, new_tab: bool, goto: Option<nib_proto::Pos>) -> Task<Message> {
        if let Some(i) = self.tabs.iter().position(|t| t.doc.path.as_deref() == Some(Path::new(&path))) {
            self.active = i;
            if let Some(pos) = goto {
                self.goto_in_active(pos);
            }
            return Task::none();
        }
        if Kind::of(Path::new(&path)) == Kind::Video {
            // Not read: videos are large and nothing here can decode them.
            let abs = nib_proto::path::join(&self.root, &path);
            self.put_media(PathBuf::from(abs), Media::Unsupported("Video"), new_tab);
            return Task::none();
        }
        self.request(Request::ReadFile { path: path.clone() }, Pending::Open { path, new_tab, goto })
    }

    /// A tab that shows `media` for `path` (its document is an empty read-only placeholder).
    pub(super) fn put_media(&mut self, path: PathBuf, media: Media, new_tab: bool) -> u64 {
        let mut doc = Document::new_plain(Default::default(), Some(path));
        doc.read_only = true;
        self.put_doc(doc, new_tab);
        let tab = &mut self.tabs[self.active];
        tab.media = Some(media);
        tab.id
    }

    /// An image file arrived: show the tab now, decode in the background.
    pub(super) fn open_image(&mut self, path: String, data: Vec<u8>, new_tab: bool) -> Task<Message> {
        let id = self.put_media(PathBuf::from(path), Media::Loading, new_tab);
        Task::perform(async move { media::decode(&data).map(Arc::new) }, move |r| Message::MediaDecoded(id, r))
    }

    pub(super) fn on_media_decoded(&mut self, id: u64, r: Result<Arc<media::Decoded>, String>) -> Task<Message> {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.media = Some(match r {
                Ok(img) => Media::image(img),
                Err(e) => Media::Failed(e),
            });
        }
        Task::none()
    }

    /// The active tab is an animation that is on screen and running.
    pub(super) fn animating(&self) -> bool {
        self.tab().and_then(|t| t.media.as_ref()).is_some_and(Media::playing)
    }

    /// A redraw while an animation plays: step it, and pin the frames it needs next.
    pub(super) fn media_tick(&mut self) -> Task<Message> {
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        let id = tab.id;
        let Some(m) = tab.media.as_mut() else { return Task::none() };
        Task::batch(m.tick().into_iter().map(|(i, h)| iced_runtime::image::allocate(h).map(move |r| Message::MediaPinned(id, i, r))))
    }

    pub(super) fn toggle_preview(&mut self) -> Task<Message> {
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        if tab.media.is_some() || !tab.kind.has_preview() {
            return self.flash_info("no preview for this file type");
        }
        tab.rendered = !tab.rendered;
        tab.top = 0;
        tab.left = 0;
        if !tab.rendered && tab.kind == Kind::Csv {
            self.follow_cursor();
        }
        Task::none()
    }

    /// Bring the active tab's table / preview up to date with its text. Runs after every update,
    /// so the rendered view follows edits as they happen.
    pub(super) fn refresh_derived(&mut self) -> Task<Message> {
        let rows = self.rows;
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        if !tab.rendered || tab.media.is_some() {
            return Task::none();
        }
        let version = tab.doc.version();
        match tab.kind {
            Kind::Csv => {
                if tab.table.is_none() || tab.table_version != version {
                    let sort = tab.table.as_ref().and_then(|t| t.sort);
                    let mut table = Table::parse(&tab.doc.text.to_string(), tab.csv_delimiter());
                    table.set_sort(sort);
                    tab.top = tab.top.min(table.len().saturating_sub(rows.min(table.len())));
                    tab.table = Some(table);
                    tab.table_version = version;
                }
                Task::none()
            }
            Kind::Markdown | Kind::Html => {
                if tab.preview.as_ref().is_some_and(|p| p.version == version) {
                    return Task::none();
                }
                let (content, map, math) = preview::render(tab.kind, &tab.doc.text.to_string());
                let (mut images, scroll, tops) = match tab.preview.take() {
                    Some(p) => (p.images, p.scroll, p.tops),
                    None => Default::default(),
                };
                let wanted: Vec<String> = content.images().iter().filter(|u| !images.contains_key(*u)).cloned().collect();
                let doc_path = tab.doc.path.clone().unwrap_or_default();
                let (id, local) = (tab.id, tab.local);
                for url in &wanted {
                    images.insert(url.clone(), None);
                }
                tab.preview = Some(Preview { content, math, version, images, map, tops, scroll });
                let mut tasks = Vec::new();
                for url in wanted {
                    let Target::File(path) = preview::resolve(&doc_path, &url) else { continue };
                    if !PreviewImage::wanted(&path) {
                        continue;
                    }
                    if local {
                        if let Ok(data) = std::fs::read(&path) {
                            self.preview_image(id, url, data);
                        }
                    } else {
                        tasks.push(self.request(Request::ReadFile { path }, Pending::PreviewImage { tab: id, url }));
                    }
                }
                Task::batch(tasks)
            }
            Kind::Svg => {
                if tab.svg.as_ref().is_some_and(|p| p.version == version) {
                    return Task::none();
                }
                let source = tab.doc.text.to_string();
                let prev = tab.svg.take().and_then(|p| p.handle);
                let (handle, error) = match usvg::Tree::from_str(&source, &usvg::Options::default()) {
                    Ok(_) => (Some(svg::Handle::from_memory(source.into_bytes())), None),
                    Err(e) => (prev, Some(e.to_string())),
                };
                tab.svg = Some(SvgPreview { handle, error, version });
                Task::none()
            }
            _ => Task::none(),
        }
    }

    pub(super) fn preview_image(&mut self, tab: u64, url: String, data: Vec<u8>) {
        let Some(p) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(|t| t.preview.as_mut()) else { return };
        let image = PreviewImage::decode(&url, data);
        p.images.insert(url, image);
    }

    /// A block of the preview was clicked: cursor to where it comes from.
    pub(super) fn on_preview_click(&mut self, pos: usize) {
        self.focus = Focus::Main;
        let rows = self.rows.max(1);
        let Some(t) = self.tabs.get_mut(self.active) else { return };
        let pos = pos.min(t.doc.text.len_chars());
        t.doc.set_cursor(pos, false);
        let line = t.doc.line_col_of(pos).0;
        if line < t.top || line >= t.top + rows {
            t.top = line.saturating_sub(rows / 3);
        }
    }

    /// The preview scrolled (or was resized): the editor follows if the user did it.
    pub(super) fn on_preview_scrolled(&mut self, v: scrollable::Viewport) {
        let Some(t) = self.tabs.get_mut(self.active) else { return };
        let lines = t.doc.text.len_lines();
        let Some(p) = t.preview.as_mut() else { return };
        p.scroll.view_h = v.bounds().height;
        p.scroll.content_h = v.content_bounds().height;
        // Still the previous tab's position: `sync_preview` is about to replace it.
        if self.preview_tab != Some(t.id) {
            return;
        }
        let y = v.absolute_offset().y;
        let moved = (y - p.scroll.y).abs() >= 1.0;
        p.scroll.y = y;
        // Our own `scroll_to` comes back here too, at the offset we asked for.
        if !moved {
            return;
        }
        let back: Vec<(f32, f32)> = p.anchors(lines).into_iter().map(|(l, y)| (y, l)).collect();
        let top = (interpolate(&back, y).round().max(0.0) as usize).min(lines.saturating_sub(1));
        t.top = top;
        p.scroll.synced_top = Some(top);
    }

    /// The editor moved: scroll the preview to the same place. Runs after every update.
    pub(super) fn sync_preview(&mut self) -> Task<Message> {
        let Some(t) = self.tabs.get_mut(self.active) else { return Task::none() };
        let (lines, top, id) = (t.doc.text.len_lines(), t.top, t.id);
        let Some(p) = t.preview.as_mut().filter(|_| t.rendered) else { return Task::none() };
        // The pane just switched to this tab: put it where this tab was, whatever it shows now.
        let switched = self.preview_tab != Some(id);
        // Nothing to line up with until the preview has been laid out and reported its size.
        if (!switched && p.scroll.synced_top == Some(top)) || p.scroll.content_h <= 0.0 {
            return Task::none();
        }
        self.preview_tab = Some(id);
        let y = interpolate(&p.anchors(lines), top as f32);
        p.scroll.synced_top = Some(top);
        if !switched && (y - p.scroll.y).abs() < 1.0 {
            return Task::none();
        }
        p.scroll.y = y;
        scroll_to(preview_id(), AbsoluteOffset { x: None, y: Some(y) })
    }

    pub(super) fn on_preview_link(&mut self, url: String) -> Task<Message> {
        // External links (also from the AI chat) open with or without a file tab.
        if let Target::External(u) = preview::resolve(std::path::Path::new(""), &url) {
            open_external(&u);
            return Task::none();
        }
        let Some(doc_path) = self.tab().and_then(|t| t.doc.path.clone()) else { return Task::none() };
        match preview::resolve(&doc_path, &url) {
            Target::External(u) => {
                open_external(&u);
                Task::none()
            }
            Target::File(path) if !self.tab().is_some_and(|t| t.local) => self.open_path(path, true),
            _ => Task::none(),
        }
    }

    pub(super) fn on_table(&mut self, ev: TableEvent) -> Task<Message> {
        self.focus = Focus::Main;
        match ev {
            TableEvent::Resized { rows } => self.rows = rows,
            TableEvent::Scroll { lines, cols } => self.scroll_table(lines.round() as isize, cols.round() as isize),
            TableEvent::Sort(col) => {
                if let Some(table) = self.tab_mut().and_then(|t| t.table.as_mut()) {
                    table.cycle_sort(col);
                }
            }
        }
        Task::none()
    }

    fn scroll_table(&mut self, lines: isize, cols: isize) {
        let rows = self.rows.max(1);
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let Some(table) = &tab.table else { return };
        let max_top = table.len().saturating_sub(rows);
        let max_left = total_cols(table).saturating_sub(8);
        tab.top = tab.top.saturating_add_signed(lines).min(max_top);
        tab.left = tab.left.saturating_add_signed(cols).min(max_left);
    }

    /// Keys on a tab that isn't showing editable text: navigation scrolls, Space plays / pauses.
    /// Returns false if the command isn't one of those.
    pub(super) fn passive_command(&mut self, c: Command) -> bool {
        use Command::*;
        let page = self.rows.max(2) as isize - 1;
        let far = isize::MAX / 2;
        let (lines, cols) = match c {
            MoveUp => (-1, 0),
            MoveDown => (1, 0),
            MoveLeft => (0, -4),
            MoveRight => (0, 4),
            PageUp => (-page, 0),
            PageDown => (page, 0),
            MoveDocStart => (-far, 0),
            MoveDocEnd => (far, 0),
            MoveLineStart => (0, -far),
            MoveLineEnd => (0, far),
            _ => return false,
        };
        self.scroll_table(lines, cols);
        true
    }

    /// What to say about a passive tab in the status strip.
    pub(super) fn passive_status(&self) -> Option<String> {
        let tab = self.tab()?;
        if let Some(m) = &tab.media {
            return m.status();
        }
        let table = tab.table.as_ref().filter(|_| tab.passive())?;
        let mut s = format!("{} rows × {} columns", table.len(), table.columns());
        if let Some(sort) = table.sort {
            let name = &table.header[sort.col];
            let name = if name.is_empty() { format!("column {}", sort.col + 1) } else { name.clone() };
            s.push_str(&format!(" · sorted by {name} {}", if sort.descending { "↓" } else { "↑" }));
        }
        Some(s)
    }

    /// The main pane for a tab: its media, its table, its text, or text next to a live preview.
    pub(super) fn tab_view<'a>(&'a self, tab: &'a Tab) -> Element<'a, Message> {
        let th = &self.theme;
        if let Some(m) = &tab.media {
            if let Some(v) = m.pdf() {
                return v.view(th).map(Message::Pdf);
            }
            return m.view(th, Message::MediaClick);
        }
        let editor = || -> Element<'a, Message> {
            let (mut marks, tag) = self.ai_follow_view(tab.id);
            marks.extend(self.search_marks(tab));
            EditorView::new(&tab.doc, th, tab.top, tab.left, self.face, Message::View).marks(marks).tag(tag).into()
        };
        if !tab.rendered {
            return editor();
        }
        if let (Kind::Svg, Some(p)) = (tab.kind, &tab.svg) {
            let pic: Element<'a, Message> = match &p.handle {
                Some(h) => svg(h.clone()).width(Length::Fill).height(Length::Fill).content_fit(ContentFit::Contain).into(),
                None => space().into(),
            };
            let mut pane = column![container(pic).center(Length::Fill).padding(12)];
            if let Some(e) = &p.error {
                let what = if p.handle.is_some() { "Not valid SVG (showing the last valid version)" } else { "Not valid SVG" };
                pane = pane.push(container(text(format!("{what}: {e}")).size(12).color(th.dim)).padding([6, 12]));
            }
            return row![
                container(editor()).width(Length::FillPortion(1)),
                self.vrule(),
                container(pane).width(Length::FillPortion(1)),
            ]
            .into();
        }
        match (&tab.table, &tab.preview) {
            (Some(table), _) if tab.kind == Kind::Csv => {
                TableView::new(table, th, tab.top, tab.left, self.face, Message::Table).into()
            }
            (_, Some(p)) => {
                let mut style = markdown::Style::from_palette(iced::Theme::Dark.palette());
                style.link_color = th.accent;
                style.inline_code_color = th.scope("string");
                style.inline_code_font = self.face.font;
                style.code_block_font = self.face.font;
                let settings = markdown::Settings::with_text_size(self.face.size + 1.0, style);
                let viewer = Viewer {
                    images: &p.images,
                    code: (self.face.font, self.face.size),
                    code_color: th.foreground,
                    targets: &p.map.targets,
                    math: &p.math,
                    color: th.foreground,
                };
                let (content, shapes) = laid_out(p.content.items(), &p.map.places, settings, &viewer);
                let body: Element<'a, Message> = Probe { content, tops: &p.tops, shapes }.into();
                let fg = th.foreground;
                let pane = scrollable(
                    container(body)
                        .padding(PREVIEW_PAD)
                        .width(Length::Fill)
                        .style(move |_| container::Style::default().color(fg)),
                )
                .id(preview_id())
                .on_scroll(Message::PreviewScrolled)
                .style(crate::theme::scrollbar)
                .width(Length::Fill)
                .height(Length::Fill);
                row![
                    container(editor()).width(Length::FillPortion(1)),
                    self.vrule(),
                    container(pane).width(Length::FillPortion(1)),
                ]
                .into()
            }
            _ => editor(),
        }
    }

    /// The button at the right of the tab bar that switches the active tab's rendered view.
    pub(super) fn preview_button(&self) -> Option<Element<'_, Message>> {
        let th = &self.theme;
        let pill = |label: &'static str, on: bool, msg: Message| -> Element<'_, Message> {
            let (fg, bd) = if on { (th.foreground, th.accent) } else { (th.dim, th.divider) };
            button(text(label).size(11).color(fg))
                .on_press(msg)
                .padding([3, 10])
                .style(move |_, _| button::Style {
                    border: Border { color: bd, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                })
                .into()
        };
        if let Some(v) = self.tab().and_then(|t| t.media.as_ref()).and_then(Media::pdf) {
            let mut pills = row![].spacing(6);
            if !v.fits() {
                pills = pills.push(pill("Fit width", false, Message::Pdf(crate::pdf::Msg::Fit)));
            }
            if !v.doc.outline.is_empty() {
                pills = pills.push(pill("Outline", v.outline_open, Message::Control(Command::TogglePreview)));
            }
            return Some(container(pills).padding([0, 8]).into());
        }
        let tab = self.tab().filter(|t| t.media.is_none() && t.kind.has_preview())?;
        let label = match (tab.kind, tab.rendered) {
            (Kind::Csv, true) => "Text",
            (Kind::Csv, false) => "Table",
            _ => "Preview",
        };
        let on = tab.rendered && tab.kind != Kind::Csv;
        Some(container(pill(label, on, Message::Control(Command::TogglePreview))).padding([0, 8]).into())
    }
}
