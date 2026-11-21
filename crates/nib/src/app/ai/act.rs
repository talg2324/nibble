//! What a step does to files, for a person (D-057).
//!
//! Each step is read into a list of acts once, when it arrives (or when a past conversation is
//! opened): never stored, and gone with its chat. Agents read and write files with their
//! harness's own tools, but just as often with `cat`, `sed -n`, heredocs and `sed -i`: both come
//! out the same here, so the step list, the rail and Follow show them the same way.

use nib_proto::AiStepKind;

/// One thing a step does to a file.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Act {
    /// Reads `path` from line `from` (1-based), to line `to` when it says.
    Read { path: String, from: u32, to: Option<u32> },
    /// Replaces text in `path` (the harness's edit: what, on the step's kind).
    Edit { path: String },
    /// Changes `path` in place from the shell (`sed -i`): what changed isn't in the command.
    Patch { path: String },
    /// Writes `path` whole, or adds to its end (`append`). `body`: what it wrote, when the command
    /// says (a heredoc, `echo`); a harness write's content is on the step's kind.
    Write { path: String, body: Option<String>, append: bool },
}

impl Act {
    pub(super) fn path(&self) -> &str {
        match self {
            Act::Read { path, .. } | Act::Edit { path } | Act::Patch { path } | Act::Write { path, .. } => path,
        }
    }

    /// It changes the file.
    pub(super) fn writes(&self) -> bool {
        !matches!(self, Act::Read { .. })
    }
}

/// What a step does to files. Paths are absolute, against `root` (where the agent works).
pub(super) fn of(kind: &AiStepKind, root: &str) -> Vec<Act> {
    match kind {
        AiStepKind::Read { path, offset, limit } => {
            let from = offset.unwrap_or(1).max(1) as u32;
            vec![Act::Read { path: path.clone(), from, to: limit.map(|l| from + (l as u32).saturating_sub(1)) }]
        }
        AiStepKind::Edit { path, .. } if !path.is_empty() => vec![Act::Edit { path: path.clone() }],
        AiStepKind::Write { path, .. } => vec![Act::Write { path: path.clone(), body: None, append: false }],
        AiStepKind::Command { command, .. } => shell(command, root),
        _ => Vec::new(),
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Op(&'static str),
    /// A heredoc's body, by its place in the bodies `lex` returns.
    Body(usize),
}

/// `p` as an absolute path, against `cwd`; `None` for what isn't a file (`/dev/null`, a flag,
/// a word the shell would expand).
fn abs_path(p: &str, cwd: &str) -> Option<String> {
    if p.is_empty() || p.starts_with('\0') || p.starts_with('-') || p.starts_with("/dev/") || p.starts_with('&') {
        return None;
    }
    let p = p.strip_prefix("./").unwrap_or(p);
    Some(nib_proto::path::join(cwd, p))
}

/// Split a shell command into words and operators, and the heredocs' bodies. Quotes are removed;
/// a word that needs the shell to expand it (`$`, globs, backticks) is kept with `\0` in front so
/// it is never taken for a path.
#[allow(unused_assignments)] // the last `flush!` resets state nothing reads
fn lex(s: &str) -> (Vec<Tok>, Vec<String>) {
    let mut out = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    // Heredocs opened on this line: their delimiters and bodies' places.
    let mut open: Vec<(String, usize)> = Vec::new();
    let mut want_delim = false;
    let mut cs = s.chars().peekable();
    let mut word = String::new();
    let mut in_word = false;
    let mut expand = false;
    macro_rules! flush {
        () => {
            if in_word {
                let w = std::mem::take(&mut word);
                if want_delim {
                    open.push((w, bodies.len()));
                    out.push(Tok::Body(bodies.len()));
                    bodies.push(String::new());
                    want_delim = false;
                } else {
                    out.push(Tok::Word(if expand { format!("\0{w}") } else { w }));
                }
                in_word = false;
                expand = false;
            }
        };
    }
    while let Some(c) = cs.next() {
        match c {
            '\n' => {
                flush!();
                out.push(Tok::Op(";"));
                // Heredoc bodies follow the line that opened them.
                for (delim, at) in std::mem::take(&mut open) {
                    let mut line = String::new();
                    loop {
                        match cs.next() {
                            Some('\n') | None => {
                                if line.trim() == delim || cs.peek().is_none() && line.is_empty() {
                                    break;
                                }
                                bodies[at].push_str(&line);
                                bodies[at].push('\n');
                                line.clear();
                                if cs.peek().is_none() {
                                    break;
                                }
                            }
                            Some(ch) => line.push(ch),
                        }
                    }
                }
            }
            ' ' | '\t' => flush!(),
            '\\' => {
                if let Some(n) = cs.next() {
                    if n != '\n' {
                        word.push(n);
                        in_word = true;
                    }
                }
            }
            '\'' => {
                in_word = true;
                for ch in cs.by_ref() {
                    if ch == '\'' {
                        break;
                    }
                    word.push(ch);
                }
            }
            '"' => {
                in_word = true;
                while let Some(ch) = cs.next() {
                    match ch {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = cs.next() {
                                word.push(n);
                            }
                        }
                        '$' | '`' => {
                            expand = true;
                            word.push(ch);
                        }
                        _ => word.push(ch),
                    }
                }
            }
            '#' if !in_word => {
                while cs.peek().is_some_and(|&ch| ch != '\n') {
                    cs.next();
                }
            }
            ';' | '|' | '&' | '(' | ')' | '<' | '>' => {
                // `2>`, `2>&1`: a descriptor number belongs to the redirect.
                if matches!(c, '<' | '>') && in_word && word.chars().all(|d| d.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                flush!();
                let op = match (c, cs.peek().copied()) {
                    ('&', Some('&')) => "&&",
                    ('|', Some('|')) => "||",
                    ('>', Some('>')) => ">>",
                    ('<', Some('<')) => "<<",
                    ('&', Some('>')) => "&>",
                    ('>', Some('&')) => ">&",
                    (';', _) => ";",
                    ('|', _) => "|",
                    ('&', _) => "&",
                    ('(', _) => "(",
                    (')', _) => ")",
                    ('<', _) => "<",
                    _ => ">",
                };
                if op.len() == 2 {
                    cs.next();
                }
                if op == "<<" {
                    // `<<<` is a here-string; `<<-` strips tabs.
                    if cs.peek() == Some(&'<') {
                        cs.next();
                        out.push(Tok::Op("<"));
                        continue;
                    }
                    if cs.peek() == Some(&'-') {
                        cs.next();
                    }
                    while cs.peek().is_some_and(|ch| *ch == ' ') {
                        cs.next();
                    }
                    want_delim = true;
                }
                out.push(Tok::Op(op));
            }
            '$' | '`' | '*' | '?' | '[' | '{' | '~' => {
                expand = true;
                in_word = true;
                word.push(c);
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    flush!();
    (out, bodies)
}

/// The first number in `s` (`120` in `120,175p`).
fn first_num(s: &str) -> Option<u32> {
    s.split(|c: char| !c.is_ascii_digit()).find(|n| !n.is_empty())?.parse().ok()
}

/// What a shell command does to files, in order.
fn shell(command: &str, root: &str) -> Vec<Act> {
    let mut out = Vec::new();
    let mut cwd = root.trim_end_matches('/').to_string();
    let (toks, mut bodies) = lex(command);
    for cmd in toks.split(|t| matches!(t, Tok::Op(";" | "&&" | "||" | "|" | "&" | "(" | ")"))) {
        let mut args: Vec<&str> = Vec::new();
        let mut writes: Vec<(&str, bool)> = Vec::new();
        let mut stdin: Option<usize> = None;
        let mut i = 0;
        while i < cmd.len() {
            match &cmd[i] {
                Tok::Op(op @ (">" | ">>" | "&>" | ">&" | "<" | "<<")) => {
                    match cmd.get(i + 1) {
                        Some(Tok::Word(w)) => {
                            if matches!(*op, ">" | ">>" | "&>") {
                                writes.push((w, *op == ">>"));
                            }
                            i += 1;
                        }
                        Some(Tok::Body(n)) => {
                            stdin = Some(*n);
                            i += 1;
                        }
                        _ => {}
                    }
                }
                Tok::Word(w) => args.push(w),
                _ => {}
            }
            i += 1;
        }
        // `FOO=1 sudo env cat f`: the command is `cat`.
        while let Some(a) = args.first() {
            if matches!(*a, "sudo" | "command" | "time" | "env" | "nice" | "exec") || (a.contains('=') && !a.starts_with('-')) {
                args.remove(0);
            } else {
                break;
            }
        }
        let Some((&prog, rest)) = args.split_first() else { continue };
        let prog = prog.rsplit('/').next().unwrap_or(prog);
        let here = cwd.clone();
        let abs = |p: &str| abs_path(p, &here);
        // The operands, without flags and the values of `valued` flags.
        let operands = |valued: &[&str]| -> Vec<&str> {
            let mut v = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if valued.contains(a) {
                    it.next();
                } else if !(a.starts_with('-') && a.len() > 1) {
                    v.push(*a);
                }
            }
            v
        };
        let read = |p: &str, from: u32, to: Option<u32>| abs(p).map(|path| Act::Read { path, from: from.max(1), to });
        match prog {
            "cd" => {
                if let Some(d) = rest.first().and_then(|d| abs_path(d, &cwd)) {
                    cwd = d.trim_end_matches('/').to_string();
                }
            }
            "cat" | "bat" | "less" | "more" | "nl" => {
                let valued: &[&str] = if prog == "bat" { &["-l", "-r", "--line-range", "--language"] } else { &[] };
                out.extend(operands(valued).into_iter().filter_map(|p| read(p, 1, None)));
            }
            "head" => {
                let n = rest.iter().position(|a| *a == "-n").and_then(|i| rest.get(i + 1)).and_then(|n| first_num(n));
                let n = n.or_else(|| rest.iter().find(|a| a.starts_with('-') && a[1..].chars().all(|c| c.is_ascii_digit()) && a.len() > 1).and_then(|a| first_num(a)));
                out.extend(operands(&["-n", "-c"]).into_iter().filter_map(|p| read(p, 1, Some(n.unwrap_or(10)))));
            }
            "tail" => out.extend(operands(&["-n", "-c"]).into_iter().filter_map(|p| read(p, 1, None))),
            "sed" | "perl" => {
                let in_place = rest.iter().any(|a| a.starts_with("-i") || *a == "--in-place" || (prog == "perl" && a.starts_with('-') && !a.starts_with("--") && a.contains('i')));
                let scripted = rest.contains(&"-e");
                let mut ops = operands(&["-e", "-f"]);
                // `sed -i '' …` on macOS: the empty backup suffix.
                if prog == "sed" && in_place && ops.first() == Some(&"") && rest.contains(&"-i") {
                    ops.remove(0);
                }
                let script = if scripted { rest.iter().skip_while(|a| **a != "-e").nth(1).copied() } else if ops.is_empty() { None } else { Some(ops.remove(0)) };
                for p in ops {
                    if in_place {
                        if let Some(path) = abs(p) {
                            out.push(Act::Patch { path });
                        }
                    } else if prog == "sed" {
                        // `120,175p`: from 120 to 175.
                        let s = script.unwrap_or("");
                        let mut nums = s.split(|c: char| !c.is_ascii_digit()).filter(|n| !n.is_empty()).filter_map(|n| n.parse().ok());
                        let from = nums.next().unwrap_or(1);
                        // `42p`: just line 42.
                        let to = nums.next().or((!s.contains(',') && first_num(s).is_some()).then_some(from));
                        out.extend(read(p, from, to));
                    }
                }
            }
            "awk" => {
                let script = rest.iter().find(|a| !a.starts_with('-')).copied().unwrap_or("");
                let s = script.replace(' ', "");
                let after = |k: &str| s.find(k).and_then(|at| first_num(&s[at + k.len()..]));
                let from = after("NR>=").or(after("NR==")).or(after("NR>").map(|n| n + 1));
                let to = after("NR<=").or(after("NR==")).or(after("NR<").map(|n| n.saturating_sub(1)));
                for p in rest.iter().filter(|a| !a.starts_with('-') && **a != script) {
                    out.extend(read(p, from.unwrap_or(1), to));
                }
            }
            "tee" => {
                let append = rest.iter().any(|a| *a == "-a" || *a == "--append");
                for p in operands(&[]) {
                    if let Some(path) = abs(p) {
                        out.push(Act::Write { path, body: stdin.map(|n| bodies[n].clone()), append });
                    }
                }
            }
            _ => {}
        }
        // Text written to a file by a command that only makes text (not `cargo build > log`:
        // that is a program at work, and shows in its terminal).
        let body = match prog {
            "cat" if rest.is_empty() => stdin.map(|n| std::mem::take(&mut bodies[n])),
            "echo" if rest.iter().all(|a| !a.starts_with('\0')) => {
                let words: Vec<&str> = rest.iter().copied().filter(|a| *a != "-e").collect();
                Some(match words.first() {
                    Some(&"-n") => words[1..].join(" "),
                    _ => format!("{}\n", words.join(" ")),
                })
            }
            _ => None,
        };
        if matches!(prog, "cat" | "echo" | "printf" | "sed" | "awk" | "perl" | "head" | "tail" | "sort" | "uniq" | "cut" | "tr" | "jq" | "grep" | "envsubst") {
            for (w, append) in writes {
                if let Some(path) = abs(w) {
                    out.push(Act::Write { path, body: body.clone(), append });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{shell, Act};

    fn read(path: &str, from: u32, to: Option<u32>) -> Act {
        Act::Read { path: path.into(), from, to }
    }

    #[test]
    fn shell_reads_are_files_at_lines() {
        assert_eq!(shell("cat src/a.rs", "/p"), [read("/p/src/a.rs", 1, None)]);
        assert_eq!(shell("cd /x && sed -n '120,175p' b.rs", "/p"), [read("/x/b.rs", 120, Some(175))]);
        assert_eq!(shell("sed -n 42p b.rs", "/p"), [read("/p/b.rs", 42, Some(42))]);
        assert_eq!(shell("head -n 40 ./c.py | grep foo", "/p"), [read("/p/c.py", 1, Some(40))]);
        assert_eq!(shell("cat -n /a/b.txt 2>/dev/null", "/p"), [read("/a/b.txt", 1, None)]);
        assert_eq!(shell("awk 'NR>=30 && NR<=60' f.rs", "/p"), [read("/p/f.rs", 30, Some(60))]);
        assert_eq!(shell("grep -rn foo src; ls", "/p"), []);
        assert_eq!(shell("cat \"$HOME/x\"", "/p"), []);
    }

    #[test]
    fn shell_writes_are_files_changed() {
        let heredoc = "cat > notes.md <<'EOF'\n# title\ncat secret.txt\nEOF\necho done";
        assert_eq!(shell(heredoc, "/p"), [Act::Write { path: "/p/notes.md".into(), body: Some("# title\ncat secret.txt\n".into()), append: false }]);
        assert_eq!(shell("sed -i '' 's/a/b/g' x.rs y.rs", "/p"), [Act::Patch { path: "/p/x.rs".into() }, Act::Patch { path: "/p/y.rs".into() }]);
        assert_eq!(shell("echo hi >> log.txt", "/p"), [Act::Write { path: "/p/log.txt".into(), body: Some("hi\n".into()), append: true }]);
        assert_eq!(shell("printf 'a' | tee out.txt", "/p"), [Act::Write { path: "/p/out.txt".into(), body: None, append: false }]);
        assert_eq!(shell("cargo build > build.log 2>&1", "/p"), []);
        assert_eq!(shell("perl -pi -e 's/a/b/' z.txt", "/p"), [Act::Patch { path: "/p/z.txt".into() }]);
    }
}
