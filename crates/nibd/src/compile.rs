//! Compile on save (Typst, LaTeX): run the compiler the client names on the file it names, and
//! turn what it printed into errors on lines. A newer run of the same file ends the one still
//! going, children included (`latexmk` runs `pdflatex`).

use crate::exe;
use nib_proto::{CompileError, Response};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Longest a compile may run before it is ended.
const TIMEOUT: Duration = Duration::from_secs(120);
/// How much of the output is sent back: its end.
const LOG_TAIL: usize = 64 * 1024;

/// The runs going on, by the file they compile: which run, and its process (group) id.
pub type Running = Arc<Mutex<HashMap<PathBuf, (u64, u32)>>>;

static RUN: AtomicU64 = AtomicU64::new(1);

/// `Request::Compile`. Blocks until the compiler exits: call it off the reader thread.
pub fn run(running: &Running, main: &Path, argv: &[String], cwd: &Path, pdf: &Path) -> Response {
    let answer = |ok, cancelled, missing, errors, log: String| Response::Compiled {
        main: nib_proto::path::to_wire(main),
        ok,
        cancelled,
        missing,
        pdf: (ok && pdf.is_file()).then(|| nib_proto::path::to_wire(pdf)),
        errors,
        log,
    };
    let Some(program) = argv.first() else {
        return answer(false, false, false, Vec::new(), "no compile command".into());
    };
    let exe = exe::find_exe(program).unwrap_or_else(|| PathBuf::from(program));
    let mut cmd = Command::new(&exe);
    cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // A group of its own, so ending it ends the compiler's children too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let what = match main.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("compiling .{ext} files"),
                None => "compiling".into(),
            };
            let msg = exe::not_found(program, &what, "set its path in Settings → Languages");
            return answer(false, false, true, Vec::new(), format!("{msg} (on {})", crate::fs_ops::hostname()));
        }
        Err(e) => return answer(false, false, false, Vec::new(), format!("could not run {program}: {e}")),
    };
    let run = RUN.fetch_add(1, Ordering::Relaxed);
    if let Some((_, older)) = running.lock().unwrap().insert(main.to_path_buf(), (run, child.id())) {
        kill_group(older);
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if started.elapsed() < TIMEOUT => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                timed_out = true;
                kill_group(child.id());
                let _ = child.kill();
                break child.wait().ok();
            }
            Err(_) => break None,
        }
    };
    let cancelled = {
        let mut r = running.lock().unwrap();
        let mine = r.get(main).is_some_and(|(n, _)| *n == run);
        if mine {
            r.remove(main);
        }
        !mine
    };
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    text.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    if timed_out {
        text.push_str(&format!("\nstopped after {} s\n", TIMEOUT.as_secs()));
    }
    let ok = !cancelled && !timed_out && status.is_some_and(|s| s.success());
    let errors = if cancelled { Vec::new() } else { parse(&text, cwd, main) };
    answer(ok, cancelled, false, errors, tail(&text, LOG_TAIL).to_string())
}

/// End process group `pid` (the process alone on Windows, with its children by `taskkill`).
fn kill_group(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// The last `max` bytes of `s`, starting on a whole line when one is in reach.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut at = s.len() - max;
    while !s.is_char_boundary(at) {
        at += 1;
    }
    let rest = &s[at..];
    rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest)
}

/// The errors and warnings in a compiler's output:
/// - `path:line:col: error|warning: message` (Typst's `--diagnostic-format short`);
/// - `path:line: message` (LaTeX with `-file-line-error`);
/// - else LaTeX's own `! message` … `l.<line>` pairs, which are about `main`.
///
/// Lines and columns are 1-based in the output. Paths are relative to `cwd`.
pub fn parse(out: &str, cwd: &Path, main: &Path) -> Vec<CompileError> {
    let mut errors: Vec<CompileError> = Vec::new();
    let mut push = |e: CompileError| {
        if !errors.iter().any(|o| o.path == e.path && o.line == e.line && o.message == e.message) {
            errors.push(e);
        }
    };
    let mut found = false;
    for line in out.lines() {
        let Some((path, ln, col, rest)) = file_line(line) else { continue };
        let (warning, message) = if let Some(m) = rest.strip_prefix("warning:") {
            (true, m.trim())
        } else {
            (false, rest.strip_prefix("error:").unwrap_or(rest).trim())
        };
        found = true;
        push(CompileError {
            path: nib_proto::path::to_wire(&resolve(cwd, path)),
            line: ln.saturating_sub(1),
            col: col.unwrap_or(1).saturating_sub(1),
            message: message.to_string(),
            warning,
        });
    }
    if !found {
        let mut pending: Option<&str> = None;
        for line in out.lines() {
            if let Some(m) = line.strip_prefix("! ") {
                pending = Some(m.trim());
            } else if let (Some(m), Some(n)) = (pending, line.strip_prefix("l.").and_then(leading_number)) {
                push(CompileError { path: nib_proto::path::to_wire(main), line: n.saturating_sub(1), col: 0, message: m.to_string(), warning: false });
                pending = None;
            }
        }
    }
    errors
}

/// `path:line:col: rest` or `path:line: rest`, where `path` names a file (has an extension).
/// The first `:` followed by a number is the split, so `C:\x\a.tex:3: …` works.
fn file_line(line: &str) -> Option<(&str, u32, Option<u32>, &str)> {
    for (i, _) in line.match_indices(':') {
        let path = &line[..i];
        let after = &line[i + 1..];
        let Some(ln) = leading_number(after) else { continue };
        let after = &after[digits(after)..];
        let Some(after) = after.strip_prefix(':') else { continue };
        if path.is_empty() || path.starts_with(' ') || !nib_proto::path::file_name(path).contains('.') {
            return None;
        }
        let (col, rest) = match leading_number(after) {
            Some(c) if after[digits(after)..].starts_with(':') => (Some(c), &after[digits(after) + 1..]),
            _ => (None, after),
        };
        return Some((path, ln, col, rest.trim()));
    }
    None
}

fn digits(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

fn leading_number(s: &str) -> Option<u32> {
    s[..digits(s)].parse().ok()
}

/// `path` under `cwd` (unless absolute), without `./` steps.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let full = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    full.components().filter(|c| !matches!(c, std::path::Component::CurDir)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typst_short_output() {
        let out = "main.typ:3:11: error: unknown variable: foo\nch/a.typ:1:16: warning: unknown font family: x\n";
        let e = parse(out, Path::new("/doc"), Path::new("/doc/main.typ"));
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].path.as_str(), e[0].line, e[0].col, e[0].message.as_str(), e[0].warning), ("/doc/main.typ", 2, 10, "unknown variable: foo", false));
        assert_eq!((e[1].path.as_str(), e[1].line, e[1].warning), ("/doc/ch/a.typ", 0, true));
    }

    #[test]
    fn latex_file_line_errors_and_noise() {
        let out = "\
Latexmk: This is Latexmk, John Collins, 4 Apr 2025. Version 4.86a.
Rule 'pdflatex':  Reasons for rerun
(./main.tex
LaTeX2e <2024-11-01>
./main.tex:5: Undefined control sequence.
l.5 \\foo
./chapters/intro.tex:12: Missing $ inserted.
Output written on main.pdf (1 page, 12 bytes).
./main.tex:5: Undefined control sequence.
Latexmk: Errors, so I did not complete making targets
";
        let e = parse(out, Path::new("/doc"), Path::new("/doc/main.tex"));
        assert_eq!(e.len(), 2, "{e:?}");
        assert_eq!((e[0].path.as_str(), e[0].line, e[0].col, e[0].message.as_str()), ("/doc/main.tex", 4, 0, "Undefined control sequence."));
        assert_eq!((e[1].path.as_str(), e[1].line), ("/doc/chapters/intro.tex", 11));
    }

    #[test]
    fn latex_without_file_line_error() {
        let out = "! Undefined control sequence.\nl.7 \\foo\n               bar\n";
        let e = parse(out, Path::new("/doc"), Path::new("/doc/main.tex"));
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].path.as_str(), e[0].line, e[0].message.as_str()), ("/doc/main.tex", 6, "Undefined control sequence."));
    }

    #[test]
    fn split_at_the_first_colon_with_a_number() {
        assert_eq!(file_line(r"C:\doc\a.tex:3: Oops"), Some((r"C:\doc\a.tex", 3, None, "Oops")));
        assert_eq!(file_line("@preview/pkg:0.1.0/lib.typ:2:4: error: x"), Some(("@preview/pkg:0.1.0/lib.typ", 2, Some(4), "error: x")));
        assert_eq!(file_line("Error: 12: no file here"), None);
        assert_eq!(file_line("time: 12:30:00 ok"), None);
    }

    #[test]
    fn tail_starts_on_a_line() {
        assert_eq!(tail("abc\ndef\nghi", 6), "ghi");
        assert_eq!(tail("short", 100), "short");
    }

    #[cfg(unix)]
    #[test]
    fn a_newer_run_ends_the_older_and_a_missing_program_says_so() {
        let dir = std::env::temp_dir();
        let main = dir.join("nib-compile-test.typ");
        let running: Running = Arc::default();
        let r2 = running.clone();
        let m2 = main.clone();
        let d2 = dir.clone();
        let slow = thread::spawn(move || run(&r2, &m2, &["sleep".into(), "30".into()], &d2, &d2.join("x.pdf")));
        let t = Instant::now();
        while running.lock().unwrap().is_empty() {
            assert!(t.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(10));
        }
        let script = "echo 'nib-compile-test.typ:2:3: error: bad'; exit 1".to_string();
        let fast = run(&running, &main, &["sh".into(), "-c".into(), script], &dir, &dir.join("x.pdf"));
        let Response::Compiled { ok, cancelled, errors, .. } = fast else { panic!() };
        assert!(!ok && !cancelled);
        assert_eq!((errors.len(), errors[0].line, errors[0].col), (1, 1, 2));
        let Response::Compiled { cancelled, .. } = slow.join().unwrap() else { panic!() };
        assert!(cancelled);
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(running.lock().unwrap().is_empty());

        let Response::Compiled { missing, log, .. } = run(&running, &main, &["nib-no-such-compiler".into()], &dir, &dir.join("x.pdf")) else { panic!() };
        assert!(missing && log.contains("nib-no-such-compiler not found"), "{log}");
    }
}
