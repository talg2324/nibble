//! Commands that wait: `sleep 30`, `timeout 40 cargo test`, `Start-Sleep -Seconds 5`,
//! `timeout /t 10`. The agents tab shows a countdown for them instead of a silent step.
//!
//! A sleep is exact: nothing happens until it is over. A timeout is an upper bound: the command
//! under it may finish sooner. Only a wait the command *starts* with gets a countdown (after
//! `cargo build && sleep 5` nobody knows when the sleep began); a later one is only named.

/// What a command waits for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Wait {
    pub secs: f64,
    /// A timeout (an upper bound), not a sleep.
    pub bound: bool,
    /// The command starts with it, so it counts from the step's start.
    pub leading: bool,
}

/// `30`, `1.5`, `2m`, `1h`, `500ms` in seconds.
fn seconds(word: &str) -> Option<f64> {
    let w = word.trim_matches(|c| c == '"' || c == '\'');
    let (num, scale) = if let Some(n) = w.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = w.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = w.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = w.strip_suffix('h') {
        (n, 3600.0)
    } else if let Some(n) = w.strip_suffix('d') {
        (n, 86400.0)
    } else {
        (w, 1.0)
    };
    let v: f64 = num.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v * scale)
}

/// The wait in one simple command (no `&&`, `;` or `|` in it).
fn simple(words: &[&str]) -> Option<(f64, bool)> {
    let name = words.first()?.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let args = &words[1..];
    match name {
        // `sleep 1 2` sleeps for the sum.
        "sleep" | "gsleep" => {
            let total: f64 = args.iter().filter_map(|a| seconds(a)).sum();
            (total > 0.0).then_some((total, false))
        }
        "start-sleep" => {
            let mut scale = 1.0;
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match a.to_ascii_lowercase().as_str() {
                    "-milliseconds" | "-ms" | "-m" => scale = 0.001,
                    "-seconds" | "-s" => scale = 1.0,
                    _ => return a.parse::<f64>().ok().map(|v| (v * scale, false)),
                }
            }
            None
        }
        // Windows' `timeout /t 10` is a sleep; coreutils' `timeout 10 cmd` bounds `cmd`.
        "timeout" | "gtimeout" => {
            if let Some(i) = args.iter().position(|a| a.eq_ignore_ascii_case("/t")) {
                return args.get(i + 1).and_then(|a| a.parse::<f64>().ok()).map(|v| (v, false));
            }
            let mut it = args.iter();
            while let Some(a) = it.next() {
                if *a == "-k" || *a == "-s" || *a == "--kill-after" || *a == "--signal" {
                    it.next();
                } else if !a.starts_with('-') {
                    return seconds(a).map(|v| (v, true));
                }
            }
            None
        }
        _ => None,
    }
}

/// The first wait in `command`.
pub(super) fn of(command: &str) -> Option<Wait> {
    let mut leading = true;
    for part in command.split(['\n', ';', '|', '&']) {
        let words: Vec<&str> = part.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        if let Some((secs, bound)) = simple(&words) {
            return Some(Wait { secs, bound, leading });
        }
        // `cd dir && sleep 5` still starts with the sleep, near enough.
        if !matches!(words[0], "cd" | "export" | "set" | "echo" | "true" | ":") {
            leading = false;
        }
    }
    None
}

/// `mm:ss` (or `h:mm:ss`), rounding up so a countdown reaches zero when it ends.
pub(super) fn clock(secs: f64) -> String {
    let s = secs.max(0.0).ceil() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// How long a finished step took: tenths under ten seconds (most commands take less than one),
/// whole seconds under a minute, then minutes and seconds.
pub(super) fn took(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs < 9.95 {
        format!("{secs:.1}s")
    } else if secs < 59.5 {
        format!("{secs:.0}s")
    } else {
        let s = secs.round() as u64;
        if s >= 3600 { format!("{}h{:02}m", s / 3600, s / 60 % 60) } else { format!("{}m{:02}s", s / 60, s % 60) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(secs: f64, bound: bool, leading: bool) -> Option<Wait> {
        Some(Wait { secs, bound, leading })
    }

    #[test]
    fn sleeps_and_timeouts_on_every_os() {
        assert_eq!(of("sleep 15"), w(15.0, false, true));
        assert_eq!(of("sleep 1m 30"), w(90.0, false, true));
        assert_eq!(of("/bin/sleep 0.5"), w(0.5, false, true));
        assert_eq!(of("timeout 40 scripts/test/e2e.sh ai_menu"), w(40.0, true, true));
        assert_eq!(of("gtimeout -k 5 2m cargo test"), w(120.0, true, true));
        assert_eq!(of("Start-Sleep -Seconds 5"), w(5.0, false, true));
        assert_eq!(of("Start-Sleep -Milliseconds 500"), w(0.5, false, true));
        assert_eq!(of("Start-Sleep 3"), w(3.0, false, true));
        assert_eq!(of("timeout /t 10 /nobreak"), w(10.0, false, true));
        assert_eq!(of("timeout.exe /T 10"), w(10.0, false, true));
    }

    #[test]
    fn only_a_wait_the_command_starts_with_counts_down() {
        assert_eq!(of("cd crates && sleep 5 && ls"), w(5.0, false, true));
        assert_eq!(of("cargo build && sleep 5"), w(5.0, false, false));
        assert_eq!(of("nib --headless x &\nsleep 2; nib ctl wait"), w(2.0, false, false));
        assert_eq!(of("cargo test"), None);
        assert_eq!(of("echo sleep"), None);
        assert_eq!(of("sleep"), None);
        assert_eq!(of("sleepy 5"), None);
    }

    #[test]
    fn clocks_read_like_a_digital_one() {
        assert_eq!(clock(0.0), "00:00");
        assert_eq!(clock(8.2), "00:09");
        assert_eq!(clock(75.0), "01:15");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(clock(-3.0), "00:00");
    }

    #[test]
    fn durations_keep_what_matters() {
        assert_eq!(took(0.34), "0.3s");
        assert_eq!(took(9.96), "10s");
        assert_eq!(took(42.4), "42s");
        assert_eq!(took(59.6), "1m00s");
        assert_eq!(took(75.0), "1m15s");
        assert_eq!(took(3725.0), "1h02m");
    }
}
