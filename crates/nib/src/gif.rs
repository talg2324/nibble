//! `nib gif`: an animated GIF as something a model can read. Models take a GIF as one still (its
//! first frame), so a screen recording of a bug shows them nothing. This keeps the frames that
//! differ from the one before, crops them to the part of the picture that changes, and tiles them
//! into one contact sheet (left to right, then down), plus each frame on its own.

use image::codecs::gif::GifDecoder;
use image::imageops::{self, FilterType};
use image::{AnimationDecoder, Rgba, RgbaImage};
use serde_json::{json, Value};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
usage: nib gif FILE [--out DIR] [--max N] [--full]
  Splits an animated GIF into the frames that differ from the one before, cropped to the part
  that changes, and tiles them into sheet.png (left to right, then down). Writes into DIR
  (default: $NIB_SCRATCH/gif/NAME, else nib's private temp folder) and prints JSON: the sheet,
  the frames (with the time each appears), the crop, and full.png (the first frame, uncropped).
  --max N   at most N frames on the sheet, evenly spaced, first and last kept (default 24)
  --full    don't crop";

/// Margin kept around the region that changes, in pixels.
const PAD: u32 = 12;
/// Gap between cells on the sheet.
const GAP: u32 = 6;
/// Longest side of the sheet: about what a model looks at before it shrinks the picture.
const LONG_EDGE: u32 = 1568;

struct Frames {
    /// Distinct frames with the time each first appears, in ms.
    kept: Vec<(RgbaImage, u32)>,
    total: usize,
    duration_ms: u32,
    /// x, y, w, h of what changes, padded; `None` when nothing does or `--full`.
    crop: Option<(u32, u32, u32, u32)>,
}

/// The GIF's frames, decoded one at a time, each with its delay in ms.
fn each_frame(path: &Path) -> Result<impl Iterator<Item = Result<(RgbaImage, u32), String>>, String> {
    let err = |e: image::ImageError| format!("{}: {e}", path.display());
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let dec = GifDecoder::new(BufReader::new(file)).map_err(err)?;
    let shown = path.display().to_string();
    Ok(dec.into_frames().map(move |f| {
        let f = f.map_err(|e| format!("{shown}: {e}"))?;
        let (n, d) = f.delay().numer_denom_ms();
        Ok((f.into_buffer(), n / d.max(1)))
    }))
}

/// The distinct frames, at most `max` (evenly spaced, first and last kept). A long recording
/// isn't held in memory whole: frames are kept only while there are few enough, and if there
/// turn out to be more, a second pass picks out the ones wanted.
fn decode(path: &Path, max: usize, full: bool) -> Result<Frames, String> {
    let max = max.max(2);
    // Index and start time of each frame that differs from the one before.
    let mut distinct: Vec<(usize, u32)> = Vec::new();
    let mut kept: Vec<(RgbaImage, u32)> = Vec::new();
    let mut last: Option<RgbaImage> = None;
    let (mut at, mut total) = (0u32, 0usize);
    for (i, f) in each_frame(path)?.enumerate() {
        let (img, delay) = f?;
        if last.as_ref() != Some(&img) {
            distinct.push((i, at));
            if distinct.len() <= max {
                kept.push((img.clone(), at));
            } else {
                kept.clear();
            }
            last = Some(img);
        }
        at += delay;
        total += 1;
    }
    if distinct.is_empty() {
        return Err(format!("{}: no frames", path.display()));
    }
    if distinct.len() > max {
        let n = distinct.len();
        let mut want = (0..max).map(|k| distinct[k * (n - 1) / (max - 1)]).peekable();
        for (i, f) in each_frame(path)?.enumerate() {
            let Some(&(j, t)) = want.peek() else { break };
            if i == j {
                kept.push((f?.0, t));
                want.next();
            }
        }
    }
    let crop = if full { None } else { changed(&kept) };
    Ok(Frames { kept, total, duration_ms: at, crop })
}

/// The box around every pixel that differs from the first frame, padded.
fn changed(kept: &[(RgbaImage, u32)]) -> Option<(u32, u32, u32, u32)> {
    let first = &kept[0].0;
    let (w, h) = first.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (img, _) in &kept[1..] {
        for (x, y, p) in img.enumerate_pixels() {
            if p != first.get_pixel(x, y) {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    if x0 > x1 {
        return None;
    }
    let (x0, y0) = (x0.saturating_sub(PAD), y0.saturating_sub(PAD));
    let (x1, y1) = ((x1 + PAD).min(w - 1), (y1 + PAD).min(h - 1));
    Some((x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

fn cells(f: &Frames) -> Vec<RgbaImage> {
    f.kept
        .iter()
        .map(|(img, _)| match f.crop {
            Some((x, y, w, h)) => imageops::crop_imm(img, x, y, w, h).to_image(),
            None => img.clone(),
        })
        .collect()
}

/// Tile `cells` (all one size) into a grid about as wide as it is tall, scaled so the sheet's
/// long side is near `LONG_EDGE`: small crops are blown up, big frames shrunk.
fn tile(cells: &[RgbaImage]) -> RgbaImage {
    let (cw, ch) = cells[0].dimensions();
    let n = cells.len() as u32;
    let cols = (((n * ch) as f64 / cw as f64).sqrt().round() as u32).clamp(1, n);
    let rows = n.div_ceil(cols);
    let (w, h) = (cols * cw + (cols - 1) * GAP, rows * ch + (rows - 1) * GAP);
    let mut sheet = RgbaImage::from_pixel(w, h, Rgba([128, 128, 128, 255]));
    for (i, cell) in cells.iter().enumerate() {
        let (c, r) = (i as u32 % cols, i as u32 / cols);
        imageops::replace(&mut sheet, cell, (c * (cw + GAP)) as i64, (r * (ch + GAP)) as i64);
    }
    // Shrink freely, but blow up by at most 3x: past that it's only bigger, not clearer. A still
    // GIF (one frame) stays as it is.
    let scale = (LONG_EDGE as f64 / w.max(h) as f64).min(if n == 1 { 1.0 } else { 3.0 });
    if (scale - 1.0).abs() < 0.05 {
        return sheet;
    }
    let (sw, sh) = (((w as f64) * scale).round() as u32, ((h as f64) * scale).round() as u32);
    let filter = if scale > 1.0 { FilterType::Nearest } else { FilterType::Triangle };
    imageops::resize(&sheet, sw.max(1), sh.max(1), filter)
}

/// The contact sheet of a GIF: its distinct frames, cropped to what changes, tiled left to
/// right then down.
pub fn sheet(path: &Path) -> Result<RgbaImage, String> {
    let f = decode(path, 24, false)?;
    Ok(tile(&cells(&f)))
}

fn out_dir(path: &Path, out: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(o) = out {
        return Ok(o);
    }
    let stem = path.file_stem().map_or("gif".into(), |s| s.to_string_lossy().replace(' ', "_"));
    let base = match std::env::var_os("NIB_SCRATCH") {
        Some(s) => PathBuf::from(s),
        None => base_dir()?,
    };
    Ok(base.join("gif").join(stem))
}

fn base_dir() -> Result<PathBuf, String> {
    crate::control::run_base().map_err(|e| e.to_string())
}

fn run(args: &[String]) -> Result<Value, String> {
    let (mut file, mut out, mut max, mut full) = (None, None, 24usize, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = Some(PathBuf::from(it.next().ok_or("--out needs a folder")?)),
            "--max" => max = it.next().and_then(|n| n.parse().ok()).ok_or("--max needs a number")?,
            "--full" => full = true,
            o if o.starts_with('-') => return Err(format!("unknown option {o}")),
            p if file.is_none() => file = Some(PathBuf::from(p)),
            _ => return Err("one file at a time".into()),
        }
    }
    let path = file.ok_or("which GIF?")?;
    let f = decode(&path, max, full)?;
    let dir = out_dir(&path, out)?;
    let frames_dir = dir.join("frames");
    std::fs::create_dir_all(&frames_dir).map_err(|e| format!("{}: {e}", frames_dir.display()))?;
    let save = |img: &RgbaImage, p: &Path| img.save(p).map_err(|e| format!("{}: {e}", p.display()));
    let cells = cells(&f);
    let mut list = Vec::new();
    for (i, (c, (_, at))) in cells.iter().zip(&f.kept).enumerate() {
        let p = frames_dir.join(format!("{:03}.png", i + 1));
        save(c, &p)?;
        list.push(json!({ "file": p, "at_ms": at }));
    }
    let full_png = dir.join("full.png");
    save(&f.kept[0].0, &full_png)?;
    let sheet_png = dir.join("sheet.png");
    save(&tile(&cells), &sheet_png)?;
    let (w, h) = f.kept[0].0.dimensions();
    Ok(json!({
        "ok": true,
        "sheet": sheet_png,
        "full": full_png,
        "size": [w, h],
        "frames_total": f.total,
        "frames_distinct": list.len(),
        "duration_ms": f.duration_ms,
        "crop": f.crop.map(|(x, y, w, h)| json!({ "x": x, "y": y, "w": w, "h": h })),
        "order": "left to right, then down",
        "frames": list,
    }))
}

/// Entry point for `nib gif ...`; returns the exit code.
pub fn gif_main(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help" || a == "help") {
        println!("{USAGE}");
        return if args.is_empty() { 2 } else { 0 };
    }
    match run(args) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": e }));
            1
        }
    }
}
