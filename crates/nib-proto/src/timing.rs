//! How long things take, for the logs (nib's `nib.log`, nibd's per-target log).
//!
//! By default only slow operations are logged, as `slow  <what> <name> <ms>`: over `NIB_SLOW_MS`
//! when it is set, otherwise over the default each call site passes (a UI step is slow far sooner
//! than a round trip to the daemon). `NIB_TIMING=all` logs every operation, as `time  …`.
//! The daemon reads its own environment: a local nibd inherits the window's, a remote one does not.

use std::fmt::{self, Debug, Write};
use std::sync::OnceLock;
use std::time::Duration;

/// Default threshold for a step on the UI thread (an update, building the view, a draw).
pub const SLOW_UI: Duration = Duration::from_millis(50);
/// Default threshold for a request to the daemon, or work the daemon does.
pub const SLOW_RPC: Duration = Duration::from_millis(250);

struct Env {
    all: bool,
    slow: Option<Duration>,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| Env {
        all: std::env::var("NIB_TIMING").is_ok_and(|v| v.eq_ignore_ascii_case("all")),
        slow: std::env::var("NIB_SLOW_MS").ok().and_then(|v| v.trim().parse().ok()).map(Duration::from_millis),
    })
}

/// Every operation is timed and logged (`NIB_TIMING=all`).
pub fn all() -> bool {
    env().all
}

/// The log line's kind for an operation that took `d` (`slow` / `time`), or None: don't log it.
pub fn verdict(d: Duration, default_slow: Duration) -> Option<&'static str> {
    if d >= env().slow.unwrap_or(default_slow) {
        Some("slow")
    } else if all() {
        Some("time")
    } else {
        None
    }
}

/// `<what> <name> <ms>ms`, the text of a timing line.
pub fn line(what: &str, name: &str, d: Duration) -> String {
    format!("{what} {name} {:.2}ms", d.as_secs_f64() * 1e3)
}

/// An enum value's variant path from its `Debug` form, without its fields:
/// `Server(Notify(PtyOutput { .. }))` gives `Server/Notify/PtyOutput`. Formatting stops at the
/// first field, so a big payload (a file's text) costs nothing.
pub fn label<T: Debug + ?Sized>(v: &T) -> String {
    struct Path {
        out: String,
        seg_start: bool,
    }
    impl Write for Path {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for c in s.chars() {
                match c {
                    '(' => {
                        if self.out.ends_with("Some") {
                            self.out.truncate(self.out.len() - 4);
                        } else {
                            self.out.push('/');
                        }
                        self.seg_start = true;
                    }
                    // A variant name starts with a capital; anything else is a field.
                    c if self.seg_start && !c.is_ascii_uppercase() => return Err(fmt::Error),
                    c if c.is_ascii_alphanumeric() || c == '_' => {
                        self.out.push(c);
                        self.seg_start = false;
                    }
                    _ => return Err(fmt::Error),
                }
            }
            Ok(())
        }
    }
    let mut p = Path { out: String::new(), seg_start: true };
    let _ = write!(p, "{v:?}");
    p.out.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::label;

    #[derive(Debug)]
    #[allow(dead_code)]
    enum Inner {
        PtyOutput { id: u32 },
        Open(String),
        Scroll(f32),
        Tick,
    }
    #[derive(Debug)]
    #[allow(dead_code)]
    enum Outer {
        Server(Inner),
        Maybe(Option<Inner>),
    }

    #[test]
    fn labels() {
        assert_eq!(label(&Outer::Server(Inner::PtyOutput { id: 3 })), "Server/PtyOutput");
        assert_eq!(label(&Outer::Server(Inner::Open("a/b.rs".into()))), "Server/Open");
        assert_eq!(label(&Outer::Server(Inner::Scroll(3.5))), "Server/Scroll");
        assert_eq!(label(&Inner::Tick), "Tick");
        assert_eq!(label(&Outer::Maybe(Some(Inner::Tick))), "Maybe/Tick");
        assert_eq!(label(&Outer::Maybe(None)), "Maybe/None");
    }
}
