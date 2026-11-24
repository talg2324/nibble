//! PDF tabs: opening, rendering pages in the background, links, zoom and keys. The model
//! and the pane are in `crate::pdf`.

use super::viewers::open_external;
use super::{App, Focus, Message, Tab};
use crate::media::Media;
use crate::pdf::{self, Msg, PdfView, Target};
use iced::widget::operation::{scroll_to, AbsoluteOffset};
use iced::{Task, Vector};
use nib_editor::Command;
use std::path::PathBuf;
use std::sync::Arc;

/// Until the window says otherwise: Retina on macOS, 1:1 elsewhere.
const DEFAULT_DPI: f32 = if cfg!(target_os = "macos") { 2.0 } else { 1.0 };
/// One arrow-key step, in logical pixels.
const STEP: f32 = 48.0;
const ZOOM_STEP: f32 = 1.25;

fn scroll(to: Vector) -> Task<Message> {
    scroll_to(pdf::scroll_id(), AbsoluteOffset { x: Some(to.x), y: Some(to.y) })
}

impl App {
    /// The PDF the active tab shows: a PDF file's, or the one a source file compiles to, when
    /// its preview is shown.
    fn pdf_view(&mut self) -> Option<&mut PdfView> {
        pane_pdf(self.tabs.get_mut(self.active)?)
    }

    pub(super) fn pdf_active(&self) -> bool {
        self.tab().and_then(|t| t.media.as_ref()).and_then(Media::pdf).is_some()
    }

    /// A PDF file arrived: show the tab now, parse in the background.
    pub(super) fn open_pdf(&mut self, path: String, data: Vec<u8>, new_tab: bool) -> Task<Message> {
        let id = self.put_media(PathBuf::from(path), Media::Loading, new_tab);
        Task::perform(async move { pdf::load(data).map(Arc::new) }, move |r| Message::Pdf(Msg::Loaded(id, r)))
    }

    /// The PDF of tab `tab` changed on disk: parse the new one; the old pages stay until it's ready.
    pub(super) fn reload_pdf(&mut self, tab: u64, data: Vec<u8>) -> Task<Message> {
        if !self.tabs.iter().any(|t| t.id == tab) {
            return Task::none();
        }
        Task::perform(async move { pdf::load(data).map(Arc::new) }, move |r| Message::Pdf(Msg::Loaded(tab, r)))
    }

    pub(super) fn on_pdf(&mut self, msg: Msg) -> Task<Message> {
        match msg {
            Msg::Loaded(id, r) => {
                let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return Task::none() };
                // A source file's compiled PDF.
                if let Some(o) = t.out.as_mut().filter(|_| t.media.is_none()) {
                    let Ok(doc) = r else { return Task::none() };
                    if let Some(v) = o.view.as_mut() {
                        return scroll(v.replace_doc(doc));
                    }
                    let mut v = PdfView::new(doc, DEFAULT_DPI);
                    // Half the window already: no outline beside it (open the PDF in a tab for that).
                    v.outline_open = false;
                    o.view = Some(Box::new(v));
                    if self.headless {
                        return Task::none();
                    }
                    return iced::window::latest().and_then(iced::window::scale_factor).map(|s| Message::Pdf(Msg::Dpi(s)));
                }
                // Read again (it was rebuilt): where it was being read, at the same zoom.
                if let Some(v) = t.media.as_mut().and_then(Media::pdf_mut) {
                    return match r {
                        Ok(doc) => scroll(v.replace_doc(doc)),
                        // Caught half-written: keep showing the last good one; the next change reads it again.
                        Err(_) => Task::none(),
                    };
                }
                t.media = Some(match r {
                    Ok(doc) => Media::Pdf(Box::new(PdfView::new(doc, DEFAULT_DPI))),
                    Err(e) => Media::Failed(e),
                });
                if self.headless {
                    return Task::none();
                }
                iced::window::latest()
                    .and_then(iced::window::scale_factor)
                    .map(|s| Message::Pdf(Msg::Dpi(s)))
            }
            Msg::Rendered { tab, page, scale, result } => {
                if let Some(v) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(any_pdf) {
                    v.on_rendered(page, scale, result);
                }
                Task::none()
            }
            Msg::Dpi(dpi) => {
                for v in self.tabs.iter_mut().filter_map(any_pdf) {
                    v.dpi = dpi;
                }
                Task::none()
            }
            Msg::Resized(size) => self.pdf_view().and_then(|v| v.resize(size)).map_or_else(Task::none, scroll),
            Msg::Scrolled(viewport) => {
                if let Some(v) = self.pdf_view() {
                    v.on_scrolled(viewport);
                }
                Task::none()
            }
            Msg::Hover(page, p) => {
                if let Some(v) = self.pdf_view() {
                    v.set_hover(Some((page, p)));
                }
                Task::none()
            }
            Msg::Unhover => {
                if let Some(v) = self.pdf_view() {
                    v.set_hover(None);
                }
                Task::none()
            }
            Msg::Click => {
                self.focus = Focus::Main;
                let target = self.pdf_view().and_then(|v| v.hovered_link()).map(|l| l.target.clone());
                self.follow(target)
            }
            Msg::Outline(i) => {
                let target = self.pdf_view().and_then(|v| v.doc.outline.get(i)).and_then(|o| o.target.clone());
                self.follow(target)
            }
            Msg::Fit => self.pdf_view().and_then(|v| v.set_zoom(None)).map_or_else(Task::none, scroll),
        }
    }

    fn follow(&mut self, target: Option<Target>) -> Task<Message> {
        match target {
            Some(Target::Page { page, y }) => self.pdf_view().map(|v| v.go_to(page, y)).map_or_else(Task::none, scroll),
            Some(Target::Uri(uri)) if pdf::safe_uri(&uri) => {
                open_external(&uri);
                Task::none()
            }
            Some(Target::Uri(uri)) => self.flash_warn(&format!("not opening {uri}: only web and mail links open from a PDF")),
            None => Task::none(),
        }
    }

    /// Start rendering the pages the active PDF tab needs. Runs after every update.
    pub(super) fn pdf_tick(&mut self) -> Task<Message> {
        let Some(tab) = self.tabs.get_mut(self.active) else { return Task::none() };
        let id = tab.id;
        let Some(v) = pane_pdf(tab) else { return Task::none() };
        let doc = v.doc.clone();
        Task::batch(v.wants().into_iter().map(|(page, scale)| {
            let doc = doc.clone();
            Task::perform(async move { doc.render(page, scale) }, move |result| {
                Message::Pdf(Msg::Rendered { tab: id, page, scale, result })
            })
        }))
    }

    /// Keys on a PDF tab: arrows / page keys scroll, the font-size keys zoom, the preview toggle
    /// shows / hides the outline. `None` if `c` isn't one of those.
    pub(super) fn pdf_command(&mut self, c: Command) -> Option<Task<Message>> {
        use Command::*;
        let v = self.pdf_view()?;
        let page = (v.page_height() - STEP).max(STEP);
        let far = f32::MAX / 4.0;
        let (dx, dy) = match c {
            FontBigger => return Some(v.zoom_by(ZOOM_STEP).map_or_else(Task::none, scroll)),
            FontSmaller => return Some(v.zoom_by(1.0 / ZOOM_STEP).map_or_else(Task::none, scroll)),
            // Back to fitting the pane.
            FontReset => return Some(v.set_zoom(None).map_or_else(Task::none, scroll)),
            TogglePreview => {
                if v.doc.outline.is_empty() {
                    return Some(self.flash_info("this PDF has no outline"));
                }
                v.outline_open = !v.outline_open;
                return Some(Task::none());
            }
            MoveUp => (0.0, -STEP),
            MoveDown => (0.0, STEP),
            MoveLeft => (-STEP, 0.0),
            MoveRight => (STEP, 0.0),
            PageUp => (0.0, -page),
            PageDown => (0.0, page),
            MoveDocStart => (0.0, -far),
            MoveDocEnd => (0.0, far),
            MoveLineStart => (-far, 0.0),
            MoveLineEnd => (far, 0.0),
            _ => return None,
        };
        Some(scroll(v.scroll_by(dx, dy)))
    }
}

/// A PDF file's view, or a source file's compiled PDF whatever is shown.
fn any_pdf(t: &mut Tab) -> Option<&mut PdfView> {
    match t.media.as_mut() {
        Some(m) => m.pdf_mut(),
        None => t.out.as_mut()?.view.as_deref_mut(),
    }
}

/// The PDF tab `t` shows now: its own, or its compiled PDF while the preview is open.
fn pane_pdf(t: &mut Tab) -> Option<&mut PdfView> {
    if t.media.is_none() && !t.rendered {
        return None;
    }
    any_pdf(t)
}
