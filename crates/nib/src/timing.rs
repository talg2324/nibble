//! Startup / frame timing. `NIB_TIMING=1` prints time-to-first-frame; `nib --bench FILE` scrolls a
//! file one line per frame and reports frames/s (the number to check on the Horizon VM).
//! Slow operations go to `nib.log` as `slow  …` lines; `NIB_TIMING=all` logs every one as
//! `time  …` (see `nib_proto::timing`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();
static FIRST: AtomicBool = AtomicBool::new(false);
static DRAWS: AtomicU64 = AtomicU64::new(0);
static DRAW_NS: AtomicU64 = AtomicU64::new(0);

pub fn start() {
    START.get_or_init(Instant::now);
}

pub fn since_start() -> Duration {
    START.get().map(|s| s.elapsed()).unwrap_or_default()
}

/// Log `what name` taking `d` when it was slow (over `slow`, or `NIB_SLOW_MS`), or always under
/// `NIB_TIMING=all`.
pub fn report(what: &str, name: &str, d: Duration, slow: Duration) {
    if let Some(kind) = nib_proto::timing::verdict(d, slow) {
        crate::control::log(kind, &nib_proto::timing::line(what, name, d));
    }
}

pub fn record_draw(d: Duration) {
    report("draw", "editor", d, nib_proto::timing::SLOW_UI);
    DRAWS.fetch_add(1, Ordering::Relaxed);
    DRAW_NS.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    if !FIRST.swap(true, Ordering::Relaxed) && std::env::var_os("NIB_TIMING").is_some() {
        eprintln!(
            "nib: first editor frame built at {:.1} ms after start",
            since_start().as_secs_f64() * 1e3
        );
    }
}

/// (draw calls, mean CPU time building the editor's primitives).
pub fn draw_stats() -> (u64, Duration) {
    let n = DRAWS.load(Ordering::Relaxed);
    let ns = DRAW_NS.load(Ordering::Relaxed);
    (n, Duration::from_nanos(ns.checked_div(n).unwrap_or(0)))
}

pub fn reset_draw_stats() {
    DRAWS.store(0, Ordering::Relaxed);
    DRAW_NS.store(0, Ordering::Relaxed);
}
