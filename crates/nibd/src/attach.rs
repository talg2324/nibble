//! Files and folders attached to a chat message with `@`: their contents go after the message's
//! text, so any assistant sees them, even one that can't read files itself (Claude Code's chat
//! runs with no tools). A folder is only the names directly in it: no contents, nothing deeper.

use nib_proto::AiAttach;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Most of one file that goes in.
const FILE_MAX: usize = 100 * 1024;
/// Most file contents one message carries, all attachments together.
const TOTAL_MAX: usize = 400 * 1024;
/// Most names of a folder's listing.
const LIST_MAX: usize = 500;

/// `text` with what `attach` names: skills before it (without their frontmatter), files and
/// folders after. Paths are relative to `root` unless absolute; ones that don't exist are left
/// out.
pub fn expand(root: &Path, text: &str, attach: &[AiAttach]) -> String {
    let mut x = Expander { root, out: String::new(), budget: TOTAL_MAX, seen: HashSet::new() };
    let mut head = String::new();
    for a in attach {
        if let Some(name) = &a.skill {
            let body = match &a.text {
                Some(t) => Some(t.clone()),
                None => fs::read_to_string(root.join(&a.path)).ok(),
            };
            if let Some(body) = body {
                let (_, body) = nib_proto::skills::split(&body);
                let _ = write!(head, "<skill name=\"{name}\">\n{}\n</skill>\n\n", body.trim_end());
            }
            continue;
        }
        let rel = a.path.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        let abs = root.join(rel);
        match (&a.text, fs::metadata(&abs)) {
            (Some(t), _) => {
                x.file(rel, Contents::Text(t.as_bytes()));
            }
            (None, Ok(m)) if m.is_dir() => x.folder(rel, &abs),
            (None, Ok(_)) => {
                x.file(rel, Contents::Disk(&abs));
            }
            (None, Err(_)) => {}
        }
    }
    let mut msg = format!("{head}{text}");
    if !x.out.is_empty() {
        msg.push_str("\n\n");
        msg.push_str(x.out.trim_end());
    }
    msg
}

enum Contents<'a> {
    Text(&'a [u8]),
    Disk(&'a Path),
}

struct Expander<'a> {
    root: &'a Path,
    out: String,
    /// Bytes of contents still allowed.
    budget: usize,
    /// Files already in (a file can be named and also be in a named folder).
    seen: HashSet<PathBuf>,
}

impl Expander<'_> {
    /// One file, whole or cut at `FILE_MAX` / what's left of the budget. `false`: it didn't fit
    /// at all.
    fn file(&mut self, rel: &str, contents: Contents) -> bool {
        if !self.seen.insert(self.root.join(rel)) {
            return true;
        }
        let bytes = match contents {
            Contents::Text(t) => std::borrow::Cow::Borrowed(t),
            Contents::Disk(p) => match fs::read(p) {
                Ok(b) => std::borrow::Cow::Owned(b),
                Err(e) => {
                    let _ = writeln!(self.out, "<file path=\"{rel}\" note=\"can't read it: {e}\"/>\n");
                    return true;
                }
            },
        };
        let size = bytes.len();
        if is_binary(&bytes) {
            let _ = writeln!(self.out, "<file path=\"{rel}\" note=\"binary, {}, not included\"/>\n", kib(size));
            return true;
        }
        if self.budget == 0 {
            return false;
        }
        let keep = utf8_cut(&bytes, size.min(FILE_MAX).min(self.budget));
        self.budget -= keep;
        let body = String::from_utf8_lossy(&bytes[..keep]);
        let _ = write!(self.out, "<file path=\"{rel}\">\n{body}");
        if !body.ends_with('\n') {
            self.out.push('\n');
        }
        if keep < size {
            let _ = writeln!(self.out, "… (cut: the first {} of {})", kib(keep), kib(size));
        }
        self.out.push_str("</file>\n\n");
        true
    }

    /// A folder: the names directly in it (a folder's with a `/`).
    fn folder(&mut self, rel: &str, abs: &Path) {
        let mut names: Vec<String> = fs::read_dir(abs)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let dir = e.path().is_dir();
                (name != ".git" && name != ".DS_Store").then(|| if dir { format!("{name}/") } else { name })
            })
            .collect();
        names.sort();
        let _ = writeln!(self.out, "<folder path=\"{rel}/\">");
        for name in names.iter().take(LIST_MAX) {
            let _ = writeln!(self.out, "{name}");
        }
        if names.len() > LIST_MAX {
            let _ = writeln!(self.out, "… and {} more", names.len() - LIST_MAX);
        }
        self.out.push_str("</folder>\n\n");
    }
}
/// A NUL in the first few KiB, or text that isn't UTF-8 there.
fn is_binary(b: &[u8]) -> bool {
    let head = &b[..b.len().min(8192)];
    head.contains(&0) || std::str::from_utf8(&head[..utf8_cut(head, head.len())]).is_err()
}

/// The longest prefix of `b` no longer than `n` that doesn't split a UTF-8 character.
fn utf8_cut(b: &[u8], n: usize) -> usize {
    if n >= b.len() {
        return b.len();
    }
    // Back up over continuation bytes (at most 3).
    (n.saturating_sub(3)..=n).rev().find(|&i| i == 0 || b[i] & 0xC0 != 0x80).unwrap_or(n)
}

fn kib(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{} KiB", n.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test (removed first if a past run left it).
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nibd-attach-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn att(path: &str) -> AiAttach {
        AiAttach { path: path.into(), text: None, skill: None }
    }

    #[test]
    fn files_and_folders_follow_the_text() {
        let dir = tmp("mixed");
        let root = dir.as_path();
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        fs::write(root.join("src/deep/b.rs"), "fn b() {}").unwrap();
        fs::write(root.join("logo.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        fs::write(root.join("notes.md"), "on disk").unwrap();

        // Unknown names are just text.
        assert_eq!(expand(root, "hi @bob", &[att("bob")]), "hi @bob");

        let out = expand(root, "look", &[att("src/"), att("src/a.rs"), att("logo.png")]);
        assert!(out.starts_with("look\n\n<folder path=\"src/\">\na.rs\ndeep/\n</folder>\n\n<file path=\"src/a.rs\">\nfn a() {}\n</file>"), "{out}");
        assert!(!out.contains("fn b()"), "a folder is its names, nothing deeper");
        assert!(out.contains("<file path=\"logo.png\" note=\"binary, 7 B, not included\"/>"), "{out}");

        // The editor's text wins over the disk's.
        let unsaved = AiAttach { path: "notes.md".into(), text: Some("edited".into()), skill: None };
        assert!(expand(root, "x", &[unsaved]).contains("<file path=\"notes.md\">\nedited\n</file>"));
    }

    #[test]
    fn a_skill_goes_first_without_its_frontmatter() {
        let dir = tmp("skill");
        fs::create_dir_all(dir.join("s")).unwrap();
        fs::write(dir.join("s/SKILL.md"), "---\nname: s\n---\nDo it this way.\n").unwrap();
        let disk = AiAttach { path: "s/SKILL.md".into(), text: None, skill: Some("s".into()) };
        assert_eq!(expand(&dir, "go", &[disk]), "<skill name=\"s\">\nDo it this way.\n</skill>\n\ngo");
        let sent = AiAttach { path: "nib-x".into(), text: Some("---\nname: nib-x\n---\nBody".into()), skill: Some("nib-x".into()) };
        assert!(expand(&dir, "go", &[sent]).starts_with("<skill name=\"nib-x\">\nBody\n</skill>"));
    }

    #[test]
    fn big_files_are_cut_on_a_character() {
        let dir = tmp("big");
        fs::write(dir.join("big.txt"), "é".repeat(FILE_MAX)).unwrap();
        let out = expand(&dir, "x", &[att("big.txt")]);
        assert!(out.contains("… (cut: the first 100 KiB of 200 KiB)"), "{}", &out[out.len() - 200..]);
        assert!(!out.contains('\u{FFFD}'));
    }
}
