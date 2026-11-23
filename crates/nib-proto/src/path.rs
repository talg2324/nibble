//! Paths on the wire.
//!
//! Contract: a path in a message is a `String`, absolute in the daemon's own style, always using
//! `/` as the separator. On Unix that is the plain path; on Windows it is `C:/proj/src/a.py`, never
//! `C:\proj\...` and never the `\\?\C:\...` form `fs::canonicalize` returns (Windows file APIs accept
//! `/` as long as there is no `\\?\` prefix). Relative paths (tree entries, search hits) are
//! relative to the project root and also use `/`.
//!
//! The daemon converts with [`to_wire`] (and [`canonical`] for paths it resolves); the client
//! treats paths as strings and asks [`is_abs`] instead of `starts_with('/')`.

use std::path::{Path, PathBuf};

/// `p` as a wire string: `\` becomes `/` on Windows and any verbatim (`\\?\`) prefix is dropped.
pub fn to_wire(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        strip_verbatim(&s).replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share` → `\\srv\share`; anything else unchanged.
fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        s.to_owned()
    }
}

/// `fs::canonicalize` without the Windows verbatim prefix, so the result can be joined with `/`
/// paths, handed to `cmd.exe` as a cwd, or turned into a `file://` URI.
pub fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    if cfg!(windows) {
        Ok(PathBuf::from(strip_verbatim(&c.to_string_lossy()).replace('\\', "/")))
    } else {
        Ok(c)
    }
}

/// The user's home: `HOME`, else `USERPROFILE` (Windows sets only that one unless launched from
/// Git Bash or similar).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// `~` and `~/x` (`~\x` on Windows) in the user's home; anything else as it is.
pub fn expand_home(p: &str) -> PathBuf {
    if p == "~" || p.starts_with("~/") || (cfg!(windows) && p.starts_with("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(p[1..].trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(p)
}

/// nib's cache folder on this machine (logs, the GPU choice, grammars being built, the web
/// tab's browser profile): `$XDG_CACHE_HOME/nib`, `~/.cache/nib`, `%LOCALAPPDATA%\nib`, else
/// the temp folder's `nib`. (nibd's own places, `nibd::places`, are its business.)
pub fn nib_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        // `HOME` only (not `USERPROFILE`): on Windows the cache is in `%LOCALAPPDATA%`.
        .or_else(|| std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".cache")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("nib")
}

/// Absolute in either style: `/x`, `C:/x`, `C:\x`, `\\server\share`.
pub fn is_abs(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with(r"\\")
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\'))
}

/// `rel` under `base`, unless `rel` is already absolute.
pub fn join(base: &str, rel: &str) -> String {
    if is_abs(rel) {
        rel.to_owned()
    } else if rel.is_empty() {
        base.to_owned()
    } else {
        format!("{}/{rel}", base.trim_end_matches('/'))
    }
}

/// The last component (`a.py` of `C:/p/a.py`); either separator.
pub fn file_name(p: &str) -> &str {
    let p = p.trim_end_matches(['/', '\\']);
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// Same path, compared the way the daemon's filesystem would: on Windows, `\` and `/` are equal
/// and case is ignored (NTFS is case-insensitive); elsewhere the string itself.
pub fn same(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        let norm = |s: &str| s.replace('\\', "/").to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_in_both_styles() {
        assert!(is_abs("/home/u/a.py"));
        assert!(is_abs("C:/proj/a.py"));
        assert!(is_abs(r"c:\proj\a.py"));
        assert!(is_abs(r"\\srv\share\a"));
        assert!(!is_abs("src/a.py"));
        assert!(!is_abs("C:"));
        assert!(!is_abs("a:b"));
    }

    #[test]
    fn join_and_name() {
        assert_eq!(join("C:/proj/", "src/a.py"), "C:/proj/src/a.py");
        assert_eq!(join("/r", "/abs"), "/abs");
        assert_eq!(join("/r", "D:/x"), "D:/x");
        assert_eq!(file_name("C:/proj/src/a.py"), "a.py");
        assert_eq!(file_name(r"C:\proj\b.rs"), "b.rs");
        assert_eq!(file_name("/r/dir/"), "dir");
    }

    #[test]
    fn verbatim_prefix_dropped() {
        assert_eq!(strip_verbatim(r"\\?\C:\proj"), r"C:\proj");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share"), r"\\srv\share");
        assert_eq!(strip_verbatim("/home/u"), "/home/u");
    }
}
