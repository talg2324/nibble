//! Colour for the agent's terminal. Claude Code hands over plain text (no ANSI), so the command is
//! split like a shell would and the output is coloured a line at a time by what it looks like:
//! diffs, `path:line` hits, numbered file listings, git status / log, errors and test results.
//! Each piece gets an [`Ink`]; the stage turns inks into theme colours.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ink {
    Plain,
    Dim,
    /// The program a command runs.
    Cmd,
    Flag,
    Str,
    Var,
    /// `|`, `&&`, `;`, redirects.
    Op,
    /// `nib ctl` / `nib ai`.
    Nib,
    Path,
    Num,
    Hash,
    Add,
    Del,
    Hunk,
    Head,
    Bad,
    Warn,
    Good,
}

/// Words after which the next word is still the command (`sudo cargo …`, `if grep …`).
const PREFIX: &[&str] = &["sudo", "time", "env", "xargs", "exec", "nohup", "then", "do", "else", "if", "elif", "while", "until", "!", "uv", "npx"];

/// The command, split into shell words, operators, strings and comments.
pub(super) fn command(s: &str) -> Vec<(&str, Ink)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut want_cmd, mut nib_next) = (0, true, false);
    while i < b.len() {
        let c = b[i];
        let start = i;
        let ink = if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                if b[i] == b'\n' {
                    want_cmd = true;
                }
                i += 1;
            }
            Ink::Plain
        } else if c == b'#' {
            i = s[i..].find('\n').map_or(b.len(), |n| i + n);
            Ink::Dim
        } else if c == b'\'' || c == b'"' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if c == b'"' && b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            want_cmd = false;
            Ink::Str
        } else if c == b'$' {
            if b.get(i + 1) == Some(&b'(') {
                i += 2;
                want_cmd = true;
                Ink::Op
            } else if b.get(i + 1) == Some(&b'{') {
                i = s[i..].find('}').map_or(b.len(), |n| i + n + 1);
                Ink::Var
            } else {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || (i == start + 1 && b"?#@*!$0123456789".contains(&b[i]))) {
                    i += 1;
                }
                Ink::Var
            }
        } else if b"|&;<>()".contains(&c) || c.is_ascii_digit() && b.get(i + 1) == Some(&b'>') {
            if let Some(r) = ["2>&1", "1>&2", "2>>", "2>"].iter().find(|r| s[i..].starts_with(**r)) {
                i += r.len();
            } else {
                i += 1;
                while i < b.len() && b"|&>".contains(&b[i]) && i - start < 2 {
                    i += 1;
                }
                if c != b'>' && c != b'<' && c != b')' {
                    want_cmd = true;
                }
            }
            Ink::Op
        } else {
            while i < b.len() && !b[i].is_ascii_whitespace() && !b"|&;<>()'\"$".contains(&b[i]) {
                i += 1;
            }
            let w = &s[start..i];
            let assign = w.find('=').is_some_and(|e| e > 0 && w[..e].bytes().all(|x| x.is_ascii_alphanumeric() || x == b'_'));
            if nib_next && (w == "ctl" || w == "ai") {
                nib_next = false;
                Ink::Nib
            } else if want_cmd && assign {
                Ink::Var
            } else if want_cmd {
                want_cmd = PREFIX.contains(&w);
                nib_next = w == "nib";
                if nib_next { Ink::Nib } else { Ink::Cmd }
            } else if w.starts_with('-') {
                Ink::Flag
            } else if w.contains('/') || w.starts_with('~') {
                Ink::Path
            } else {
                Ink::Plain
            }
        };
        out.push((&s[start..i], ink));
    }
    out
}

/// The output, a line at a time. Newlines are kept as their own plain pieces.
pub(super) fn output(s: &str) -> Vec<(&str, Ink)> {
    let diff = s.lines().any(|l| l.starts_with("@@ ") || l.starts_with("diff --git "));
    let mut out = Vec::new();
    for (n, line) in s.split('\n').enumerate() {
        if n > 0 {
            out.push(("\n", Ink::Plain));
        }
        if !line.is_empty() {
            self::line(line, diff, &mut out);
        }
    }
    out
}

fn line<'a>(l: &'a str, diff: bool, out: &mut Vec<(&'a str, Ink)>) {
    if diff {
        let ink = if ["diff --git ", "+++ ", "--- ", "index "].iter().any(|p| l.starts_with(p)) {
            Some(Ink::Head)
        } else if l.starts_with("@@") {
            Some(Ink::Hunk)
        } else if l.starts_with('+') {
            Some(Ink::Add)
        } else if l.starts_with('-') {
            Some(Ink::Del)
        } else {
            None
        };
        if let Some(ink) = ink {
            return out.push((l, ink));
        }
    }
    // `   12→text` / `   12\ttext`: a numbered file listing (Read, cat -n).
    let digits = l.trim_start().bytes().take_while(u8::is_ascii_digit).count();
    let lead = l.len() - l.trim_start().len();
    if digits > 0 {
        let after = &l[lead + digits..];
        if let Some(sep) = ["→", "\t"].iter().find(|p| after.starts_with(**p)) {
            let k = lead + digits + sep.len();
            out.push((&l[..k], Ink::Dim));
            return out.push((&l[k..], Ink::Plain));
        }
    }
    // Short git status: ` M src/x.rs`, `?? new.rs`.
    if let [x, y, b' ', p, ..] = l.as_bytes() {
        if b" MADRCU?!".contains(x) && b" MADRCU?!".contains(y) && (*x != b' ' || *y != b' ') && *p != b' ' && !l[3..].contains(' ') {
            let ink = match (x, y) {
                (b'?', _) | (b'!', _) => Ink::Dim,
                (b'D', _) | (_, b'D') => Ink::Del,
                (b'A', _) => Ink::Add,
                _ => Ink::Warn,
            };
            out.push((&l[..2], ink));
            return out.push((&l[2..], Ink::Path));
        }
    }
    // Long git status.
    let t = l.trim_start();
    for (p, ink) in [("modified:", Ink::Warn), ("new file:", Ink::Add), ("deleted:", Ink::Del), ("renamed:", Ink::Warn)] {
        if t.starts_with(p) {
            let k = lead + p.len();
            out.push((&l[..k], ink));
            return out.push((&l[k..], Ink::Path));
        }
    }
    // Git log: `commit <sha>` / `<sha> subject`.
    let hex = |w: &str| w.len() >= 7 && w.len() <= 40 && w.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) && w.bytes().any(|c| c.is_ascii_alphabetic());
    let word = l.split(' ').next().unwrap_or("");
    if hex(word) {
        out.push((word, Ink::Hash));
        return out.push((&l[word.len()..], Ink::Plain));
    }
    if let Some(sha) = l.strip_prefix("commit ").filter(|r| hex(r.split(' ').next().unwrap_or(""))) {
        out.push((&l[..l.len() - sha.len()], Ink::Head));
        return out.push((sha, Ink::Hash));
    }
    // `path:12:rest` (grep, compilers), or `  --> path:12:5` (rustc).
    let rest_ink = severity(l);
    let arrow = t.strip_prefix("--> ").map_or(0, |_| lead + 4);
    if let Some((path_end, num_end)) = location(&l[arrow..]) {
        let (pe, ne) = (arrow + path_end, arrow + num_end);
        if arrow > 0 {
            out.push((&l[..arrow], Ink::Dim));
        }
        out.push((&l[arrow..pe], Ink::Path));
        out.push((&l[pe..ne], Ink::Num));
        return out.push((&l[ne..], rest_ink));
    }
    out.push((l, rest_ink));
}

/// `path:line` at the start of `l`: where the path ends and where the line (and column) numbers do.
fn location(l: &str) -> Option<(usize, usize)> {
    let path_end = l.find(':')?;
    let path = &l[..path_end];
    if path.is_empty() || path.contains(' ') || !(path.contains('/') || path.contains('.')) {
        return None;
    }
    let b = l.as_bytes();
    let mut i = path_end;
    let mut any = false;
    while i < b.len() && b[i] == b':' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        any = true;
    }
    if !any {
        return None;
    }
    if i < b.len() && (b[i] == b':' || b[i] == b'-') {
        i += 1;
    }
    Some((path_end, i))
}

/// How a whole line reads: an error, a warning, a pass, or nothing in particular.
fn severity(l: &str) -> Ink {
    let low = l.trim_start().to_ascii_lowercase();
    let bad = ["error", "fatal:", "traceback", "panicked", "failed", "failure", "exception", "✗", "command not found", "no such file"];
    let good = ["test result: ok", "✓", "passed", "finished", "success"];
    if low.ends_with("... ok") || low.ends_with(" ok") && low.starts_with("test ") {
        Ink::Good
    } else if low.ends_with("... failed") || bad.iter().any(|w| low.starts_with(w)) || ["error:", "error[", " failed", "exception:"].iter().any(|w| low.contains(w)) {
        Ink::Bad
    } else if low.starts_with("warning") || low.contains("warning:") {
        Ink::Warn
    } else if good.iter().any(|w| low.starts_with(w)) || low.contains(" passed") && !low.contains(" failed") {
        Ink::Good
    } else {
        Ink::Plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inks(v: Vec<(&str, Ink)>) -> Vec<(&str, Ink)> {
        v.into_iter().filter(|(s, _)| !s.trim().is_empty()).collect()
    }

    #[test]
    fn command_words() {
        let v = inks(command("cd ~/src && FOO=1 cargo test -p nib 2>&1 | grep \"a b\" # why"));
        assert_eq!(
            v,
            vec![
                ("cd", Ink::Cmd),
                ("~/src", Ink::Path),
                ("&&", Ink::Op),
                ("FOO=1", Ink::Var),
                ("cargo", Ink::Cmd),
                ("test", Ink::Plain),
                ("-p", Ink::Flag),
                ("nib", Ink::Plain),
                ("2>&1", Ink::Op),
                ("|", Ink::Op),
                ("grep", Ink::Cmd),
                ("\"a b\"", Ink::Str),
                ("# why", Ink::Dim),
            ]
        );
        assert_eq!(inks(command("nib ctl state")), vec![("nib", Ink::Nib), ("ctl", Ink::Nib), ("state", Ink::Plain)]);
        assert_eq!(command("echo $HOME").last(), Some(&("$HOME", Ink::Var)));
    }

    #[test]
    fn output_lines() {
        let v = output("src/a.rs:12:5: error: nope\n    3→let x = 1;\n M src/b.rs\nabc1234 Fix it\ntest result: ok. 3 passed");
        assert!(v.contains(&("src/a.rs", Ink::Path)) && v.contains(&(":12:5:", Ink::Num)) && v.contains(&(" error: nope", Ink::Bad)));
        assert!(v.contains(&("    3→", Ink::Dim)));
        assert!(v.contains(&(" M", Ink::Warn)) && v.contains(&(" src/b.rs", Ink::Path)));
        assert!(v.contains(&("abc1234", Ink::Hash)));
        assert!(v.contains(&("test result: ok. 3 passed", Ink::Good)));
        let d = output("@@ -1 +1 @@\n-old\n+new\n ctx");
        assert_eq!(inks(d), vec![("@@ -1 +1 @@", Ink::Hunk), ("-old", Ink::Del), ("+new", Ink::Add), (" ctx", Ink::Plain)]);
        assert_eq!(output("- a list item"), vec![("- a list item", Ink::Plain)]);
    }
}
