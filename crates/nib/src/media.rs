//! Image files: decoding (off the UI thread) and the viewer pane. Animated GIF / WebP / APNG
//! play in a loop; the frame due is a pure function of the time since playback started. The
//! renderer uploads a large image in the background and draws nothing until that is done, and it
//! drops an image the moment a redraw doesn't use it, so each frame change would flash. Playback
//! therefore pins the frames it is about to need (`Media::tick`) and moves to a frame only once
//! it is pinned.

use crate::pdf::PdfView;
use crate::theme::Theme;
use iced::widget::image::Handle;
use iced::widget::{column, container, image as image_widget, mouse_area, space, text};
use iced::{Alignment, ContentFit, Element, Length};
use iced_runtime::image::Allocation;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a path is shown as, judged by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Markdown,
    Html,
    Csv,
    /// Text, shown next to its picture.
    Svg,
    Image,
    Video,
    Pdf,
}

impl Kind {
    pub fn of(path: &Path) -> Kind {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" | "mdx" | "mkd" | "mkdn" | "mdwn" | "mdown" | "livemd" => Kind::Markdown,
            "html" | "htm" | "xhtml" | "shtml" => Kind::Html,
            "csv" | "tsv" | "tab" => Kind::Csv,
            "svg" => Kind::Svg,
            "png" | "apng" | "jpg" | "jpeg" | "jpe" | "jfif" | "gif" | "tif" | "tiff" | "webp" | "bmp" => Kind::Image,
            "pdf" => Kind::Pdf,
            "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "wmv" | "flv" | "mpg" | "mpeg" | "ogv" | "3gp" => Kind::Video,
            _ => Kind::Text,
        }
    }

    /// Opens with a rendered view next to (or instead of) the source text.
    pub fn has_preview(self) -> bool {
        matches!(self, Kind::Markdown | Kind::Html | Kind::Csv | Kind::Svg)
    }
}

/// Frames are kept decoded; stop adding them past this many bytes of RGBA.
const FRAME_BUDGET: usize = 512 << 20;
/// Larger images are downscaled to fit GPU texture limits.
const MAX_SIDE: u32 = 8192;
/// Browsers treat very short GIF delays as "unspecified"; so do we.
const MIN_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_DELAY: Duration = Duration::from_millis(100);
/// Frames due within this long are pinned ahead of time. One upload takes 30-70 ms.
const LOOKAHEAD: Duration = Duration::from_millis(150);
/// A pin not answered after this many redraws never will be (no renderer): play unpinned.
const PIN_TIMEOUT: u32 = 240;

pub struct Frame {
    pub handle: Handle,
    /// Time from the start of the loop until this frame ends.
    pub ends: Duration,
}

pub struct Decoded {
    pub frames: Vec<Frame>,
    pub width: u32,
    pub height: u32,
    pub format: &'static str,
    pub bytes: usize,
    /// The animation was cut short to stay inside `FRAME_BUDGET`.
    pub truncated: bool,
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Decoded({}x{} {}, {} frames)", self.width, self.height, self.format, self.frames.len())
    }
}

impl Decoded {
    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }

    fn duration(&self) -> Duration {
        self.frames.last().map_or(Duration::ZERO, |f| f.ends)
    }

    /// Index of the frame due `elapsed` after playback started (looping).
    #[cfg(test)]
    pub fn frame_at(&self, elapsed: Duration) -> usize {
        self.upcoming(elapsed)[0]
    }

    /// The frame due `elapsed` after playback started, then the ones to have ready after it:
    /// the next two, and every frame that starts within `LOOKAHEAD`.
    fn upcoming(&self, elapsed: Duration) -> Vec<usize> {
        let (n, total) = (self.frames.len(), self.duration());
        if n < 2 || total.is_zero() {
            return vec![0];
        }
        let t = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
        let due = self.frames.partition_point(|f| f.ends <= t).min(n - 1);
        let mut out = vec![due];
        // Time until the next frame not yet in `out` starts.
        let mut until = self.frames[due].ends.saturating_sub(t);
        for step in 1..n {
            if step > 2 && until >= LOOKAHEAD {
                break;
            }
            let i = (due + step) % n;
            out.push(i);
            until += self.frames[i].ends - if i == 0 { Duration::ZERO } else { self.frames[i - 1].ends };
        }
        out
    }

    /// One line for the status strip.
    pub fn describe(&self) -> String {
        let mut s = format!("{}×{} · {}", self.width, self.height, self.format);
        if self.animated() {
            s.push_str(&format!(" · {} frames · {:.1}s", self.frames.len(), self.duration().as_secs_f32()));
            if self.truncated {
                s.push_str(" (truncated)");
            }
        }
        s.push_str(&format!(" · {}", human_size(self.bytes)));
        s
    }
}

pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::WebP => "WebP",
        ImageFormat::Bmp => "BMP",
        _ => "image",
    }
}

fn still(img: DynamicImage) -> (Handle, u32, u32) {
    let (w, h) = (img.width(), img.height());
    let img = if w.max(h) > MAX_SIDE { img.thumbnail(MAX_SIDE, MAX_SIDE) } else { img };
    let rgba = img.into_rgba8();
    (Handle::from_rgba(rgba.width(), rgba.height(), rgba.into_raw()), w, h)
}

/// `first_only`: stop after the first frame (what a still picture of an animation shows).
fn animation<'a>(dec: impl AnimationDecoder<'a>, first_only: bool) -> Result<(Vec<Frame>, u32, u32, bool), String> {
    let mut frames = Vec::new();
    let (mut w, mut h) = (0, 0);
    let mut ends = Duration::ZERO;
    let mut used = 0usize;
    let mut truncated = false;
    for frame in dec.into_frames() {
        let frame = match frame {
            Ok(f) => f,
            // A damaged tail still leaves the frames before it playable.
            Err(_) if !frames.is_empty() => break,
            Err(e) => return Err(e.to_string()),
        };
        let delay = Duration::from(frame.delay());
        ends += if delay < MIN_DELAY { DEFAULT_DELAY } else { delay };
        let buf = frame.into_buffer();
        (w, h) = (buf.width(), buf.height());
        used += buf.as_raw().len();
        frames.push(Frame { handle: Handle::from_rgba(w, h, buf.into_raw()), ends });
        if first_only {
            break;
        }
        if used > FRAME_BUDGET {
            truncated = true;
            break;
        }
    }
    if frames.is_empty() {
        return Err("no frames".into());
    }
    Ok((frames, w, h, truncated))
}

/// Decode an image file. CPU-heavy: run it off the UI thread.
pub fn decode(data: &[u8]) -> Result<Decoded, String> {
    decode_frames(data, false)
}

/// As `decode`, but only an animation's first frame (the rest would be thrown away).
pub fn decode_first(data: &[u8]) -> Result<Decoded, String> {
    decode_frames(data, true)
}

fn decode_frames(data: &[u8], first_only: bool) -> Result<Decoded, String> {
    let err = |e: image::ImageError| e.to_string();
    let reader = ImageReader::new(Cursor::new(data)).with_guessed_format().map_err(|e| e.to_string())?;
    let format = reader.format().ok_or("not a supported image format")?;
    let animated = match format {
        ImageFormat::Gif => Some(animation(GifDecoder::new(Cursor::new(data)).map_err(err)?, first_only)),
        ImageFormat::WebP => {
            let dec = WebPDecoder::new(Cursor::new(data)).map_err(err)?;
            dec.has_animation().then(|| animation(dec, first_only))
        }
        ImageFormat::Png => {
            let dec = PngDecoder::new(Cursor::new(data)).map_err(err)?;
            match dec.is_apng() {
                Ok(true) => Some(dec.apng().map_err(err).and_then(|a| animation(a, first_only))),
                _ => None,
            }
        }
        _ => None,
    };
    let (frames, width, height, truncated) = match animated {
        Some(a) => a?,
        None => {
            let mut dec = reader.into_decoder().map_err(err)?;
            let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
            let mut img = DynamicImage::from_decoder(dec).map_err(err)?;
            img.apply_orientation(orientation);
            let (handle, w, h) = still(img);
            (vec![Frame { handle, ends: Duration::ZERO }], w, h, false)
        }
    };
    Ok(Decoded { frames, width, height, format: format_name(format), bytes: data.len(), truncated })
}

/// A tab that shows a file instead of editing it.
pub enum Media {
    Loading,
    Image {
        img: Arc<Decoded>,
        /// Playback clock origin (shifted on resume so the animation continues where it paused).
        started: Instant,
        paused_at: Option<Duration>,
        /// The frame on screen: the newest due frame that was pinned in time.
        shown: usize,
        pins: Pins,
    },
    Failed(String),
    /// A file type nib recognises but can't show yet.
    Unsupported(&'static str),
    /// Drawn by `PdfView::view`, not `Media::view`.
    Pdf(Box<PdfView>),
}

/// The frames of an animation held in the renderer, so drawing one shows it at once.
#[derive(Default)]
pub struct Pins {
    held: HashMap<usize, Allocation>,
    /// Asked for and not answered yet, with the redraws since.
    asked: HashMap<usize, u32>,
    /// The renderer can't pin: show each frame when it is due, as a still is shown.
    off: bool,
}

impl Media {
    pub fn image(img: Arc<Decoded>) -> Self {
        Media::Image { img, started: Instant::now(), paused_at: None, shown: 0, pins: Pins::default() }
    }

    /// One redraw of a playing animation: move to the frame that is due if it is pinned, and
    /// return the frames to pin now (answer each with `pinned`).
    pub fn tick(&mut self) -> Vec<(usize, Handle)> {
        let Media::Image { img, started, paused_at: None, shown, pins } = self else { return Vec::new() };
        if !img.animated() {
            return Vec::new();
        }
        let want = img.upcoming(started.elapsed());
        let due = want[0];
        pins.asked.values_mut().for_each(|waited| *waited += 1);
        if pins.off || pins.asked.values().any(|&waited| waited > PIN_TIMEOUT) {
            *pins = Pins { off: true, ..Pins::default() };
            *shown = due;
            return Vec::new();
        }
        if pins.held.contains_key(&due) {
            *shown = due;
        }
        // The frame on screen stays pinned until the due one takes over.
        pins.held.retain(|i, _| i == shown || want.contains(i));
        pins.asked.retain(|i, _| want.contains(i));
        let mut ask = Vec::new();
        for i in want {
            if !pins.held.contains_key(&i) && !pins.asked.contains_key(&i) {
                pins.asked.insert(i, 0);
                ask.push((i, img.frames[i].handle.clone()));
            }
        }
        ask
    }

    /// The renderer's answer to a pin `tick` asked for.
    pub fn pinned(&mut self, i: usize, r: Result<Allocation, iced_runtime::image::Error>) {
        let Media::Image { img, pins, .. } = self else { return };
        match r {
            // An answer for a frame no longer wanted (or of the file before a reload) is dropped.
            Ok(a) => {
                if img.frames.get(i).is_some_and(|f| f.handle.id() == a.handle().id()) && pins.asked.remove(&i).is_some() {
                    pins.held.insert(i, a);
                }
            }
            Err(_) => *pins = Pins { off: true, ..Pins::default() },
        }
    }

    /// Needs continuous redraws.
    pub fn playing(&self) -> bool {
        matches!(self, Media::Image { img, paused_at: None, .. } if img.animated())
    }

    pub fn toggle_play(&mut self) {
        if let Media::Image { img, started, paused_at, .. } = self {
            if !img.animated() {
                return;
            }
            match paused_at.take() {
                Some(at) => *started = Instant::now() - at,
                None => *paused_at = Some(started.elapsed()),
            }
        }
    }

    pub fn pdf(&self) -> Option<&PdfView> {
        match self {
            Media::Pdf(v) => Some(v),
            _ => None,
        }
    }

    pub fn pdf_mut(&mut self) -> Option<&mut PdfView> {
        match self {
            Media::Pdf(v) => Some(v),
            _ => None,
        }
    }

    pub fn status(&self) -> Option<String> {
        match self {
            Media::Pdf(v) => Some(v.status()),
            Media::Image { img, paused_at, .. } => {
                let mut s = img.describe();
                if paused_at.is_some() {
                    s.push_str(" · paused");
                }
                Some(s)
            }
            _ => None,
        }
    }

    pub fn view<'a, Message: Clone + 'a>(&'a self, th: &Theme, on_click: Message) -> Element<'a, Message> {
        let note = |title: String, detail: String| -> Element<'a, Message> {
            column![text(title).size(14).color(th.foreground), text(detail).size(12).color(th.dim)]
                .spacing(6)
                .align_x(Alignment::Center)
                .into()
        };
        let body: Element<'a, Message> = match self {
            Media::Loading => note("Loading…".into(), String::new()),
            Media::Failed(e) => note("Can't show this file".into(), e.clone()),
            Media::Pdf(_) => space().into(),
            Media::Unsupported(what) => note(format!("{what} playback isn't supported yet"), "nib can't decode this file type.".into()),
            Media::Image { img, shown, .. } => {
                let pic = image_widget(img.frames[*shown].handle.clone()).content_fit(ContentFit::ScaleDown);
                mouse_area(pic).on_press(on_click).into()
            }
        };
        container(body).center(Length::Fill).padding(12).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Rgba, RgbaImage};

    fn encode(img: &RgbaImage, f: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let img = DynamicImage::ImageRgba8(img.clone());
        // JPEG has no alpha channel.
        let img = if f == ImageFormat::Jpeg { DynamicImage::ImageRgb8(img.to_rgb8()) } else { img };
        img.write_to(&mut out, f).unwrap();
        out.into_inner()
    }

    #[test]
    fn kinds_by_extension() {
        assert_eq!(Kind::of(Path::new("a/B.PNG")), Kind::Image);
        assert_eq!(Kind::of(Path::new("x.tif")), Kind::Image);
        assert_eq!(Kind::of(Path::new("x.mp4")), Kind::Video);
        assert_eq!(Kind::of(Path::new("Paper.PDF")), Kind::Pdf);
        assert_eq!(Kind::of(Path::new("logo.SVG")), Kind::Svg);
        assert_eq!(Kind::of(Path::new("x.avi")), Kind::Video);
        assert_eq!(Kind::of(Path::new("x.csv")), Kind::Csv);
        assert_eq!(Kind::of(Path::new("README.md")), Kind::Markdown);
        assert_eq!(Kind::of(Path::new("index.htm")), Kind::Html);
        assert_eq!(Kind::of(Path::new("main.rs")), Kind::Text);
    }

    #[test]
    fn decodes_still_formats() {
        let img = RgbaImage::from_pixel(7, 5, Rgba([10, 20, 30, 255]));
        for (f, name) in [
            (ImageFormat::Png, "PNG"),
            (ImageFormat::Jpeg, "JPEG"),
            (ImageFormat::Tiff, "TIFF"),
            (ImageFormat::Bmp, "BMP"),
            (ImageFormat::WebP, "WebP"),
        ] {
            let d = decode(&encode(&img, f)).unwrap();
            assert_eq!((d.width, d.height, d.format, d.frames.len()), (7, 5, name, 1), "{name}");
            assert!(!d.animated());
        }
        assert!(decode(b"definitely not an image").is_err());
    }

    #[test]
    fn animated_gif_plays_in_a_loop() {
        let mut data = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut data);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for (i, ms) in [100u32, 200, 100].into_iter().enumerate() {
                let img = RgbaImage::from_pixel(4, 4, Rgba([i as u8 * 80, 0, 0, 255]));
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
            }
        }
        let d = decode(&data).unwrap();
        assert_eq!((d.frames.len(), d.width, d.height), (3, 4, 4));
        assert!(d.animated());
        let at = |ms| d.frame_at(Duration::from_millis(ms));
        assert_eq!([at(0), at(99), at(100), at(299), at(300), at(399)], [0, 0, 1, 1, 2, 2]);
        // Loops.
        assert_eq!([at(400), at(550), at(700)], [0, 1, 2]);

        // What to have ready: the due frame, the next two, and all that start within LOOKAHEAD.
        assert_eq!(d.upcoming(Duration::from_millis(0)), [0, 1, 2]);
        assert_eq!(d.upcoming(Duration::from_millis(350)), [2, 0, 1], "wraps around the loop");

        let d = Arc::new(d);
        let mut m = Media::image(d.clone());
        assert!(m.playing());
        // A frame is shown only once the renderer holds it: until then the last one stays.
        let asked: Vec<usize> = m.tick().into_iter().map(|(i, _)| i).collect();
        assert_eq!(asked, [0, 1, 2]);
        assert!(m.tick().is_empty(), "each frame is asked for once");
        if let Media::Image { started, .. } = &mut m {
            *started = Instant::now() - Duration::from_millis(150);
        }
        let _ = m.tick();
        assert!(matches!(m, Media::Image { shown: 0, .. }), "frame 1 is due but not pinned yet");
        #[allow(unsafe_code)]
        let pin = |i: usize| unsafe { iced::advanced::image::allocate(&d.frames[i].handle, iced::Size::new(4, 4)) };
        m.pinned(1, Ok(pin(1)));
        let _ = m.tick();
        assert!(matches!(m, Media::Image { shown: 1, .. }));
        // An answer nobody is waiting for is dropped; a failed one turns pinning off.
        m.pinned(1, Ok(pin(1)));
        m.pinned(2, Err(iced_runtime::image::Error::OutOfMemory));
        if let Media::Image { started, .. } = &mut m {
            *started = Instant::now() - Duration::from_millis(350);
        }
        assert!(m.tick().is_empty());
        assert!(matches!(m, Media::Image { shown: 2, .. }), "unpinned: the due frame is shown as it comes");

        m.toggle_play();
        assert!(!m.playing() && m.status().unwrap().ends_with("paused"));
        m.toggle_play();
        assert!(m.playing());
    }
}
