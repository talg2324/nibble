//! Where agents (and the user) keep files that aren't part of the project, on the machine where
//! the code lives. Terminals get both as environment variables; the file tree shows both.
//!
//! - **NIBDIR** (`$NIBDIR`): persistent, one per project:
//!   `<XDG_DATA_HOME or ~/.local/share>/nib/nibdir/<project name>/`; a second project with the
//!   same folder name gets `<name>-2`, and so on. `nibdir/projects.json` maps each project root to
//!   its folder (outside every NIBDIR, so the tree never shows it).
//!   A project inside a temp directory (tests, throwaway experiments) gets a throwaway NIBDIR
//!   instead, next to its scratch and deleted with it, so they don't pile up for ever.
//! - **scratch** (`$NIB_SCRATCH`): one per daemon session:
//!   `<XDG_CACHE_HOME or ~/.cache>/nib/scratch/<project name>-<pid>/`. Deleted once its daemon is
//!   gone and nothing in it changed for a day.
//!
//! On Windows (no XDG variables set) both live under `%LOCALAPPDATA%\nib\`: `data\nibdir\…` and
//! `cache\scratch\…`.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// A dead session's scratch directory is kept this long after its last change.
const KEEP_SCRATCH: Duration = Duration::from_secs(24 * 3600);

pub struct Places {
    pub scratch: PathBuf,
    pub nibdir: PathBuf,
}

fn home() -> Option<PathBuf> {
    nib_proto::path::home_dir()
}

pub(crate) fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    // Tests send Hello many times: keep them out of the real home directory.
    if cfg!(test) {
        return std::env::temp_dir().join("nibd-test-places").join(var);
    }
    // Windows: `%LOCALAPPDATA%\nib\cache` / `\data` unless XDG is set on purpose (next to the
    // logs nib keeps in `%LOCALAPPDATA%\nib`; dot-folders in the profile are a Unix habit).
    if cfg!(windows) && std::env::var_os(var).is_none() {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let kind = if var == "XDG_CACHE_HOME" { "cache" } else { "data" };
            return PathBuf::from(local).join("nib").join(kind);
        }
    }
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| home().map(|h| fallback.iter().fold(h, |p, s| p.join(s))))
        .unwrap_or_else(std::env::temp_dir)
        .join("nib")
}

fn slug(root: &Path) -> String {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
    name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect()
}

/// The name the project at canonical `root` goes by on this machine: its NIBDIR folder's
/// (`projects.json`, so two projects with the same folder name are `app` and `app-2`), else
/// its folder's name (a temp project, or one never opened). Its agents' history is kept under it.
pub fn project_name(root: &Path) -> String {
    let index = xdg("XDG_DATA_HOME", &[".local", "share"]).join("nibdir").join("projects.json");
    read_index(&index).get(&*root.to_string_lossy()).cloned().unwrap_or_else(|| slug(root))
}

fn private_dir(p: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(p)
}

/// A directory of this process's own in the temp dir, for the files nibd hands to other programs
/// (a system prompt, pictures, a copy of the git index) and for the mailbox when there is no
/// scratch directory. On a machine shared with other users `/tmp` is theirs too: a name they can
/// guess (`/tmp/nibd-images-1234`) they can make first, and then read what is put in it and fill
/// it with links to the user's files. So the name has 128 random bits, and the directory is made
/// new and private here or not used at all. `None`: it could not be made.
pub fn private_temp() -> Option<&'static Path> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        for _ in 0..8 {
            let dir = std::env::temp_dir().join(format!("nibd-{}-{}", std::process::id(), crate::tokens::fresh()));
            let mut b = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
            // Not `recursive`: a directory that is already there is somebody else's.
            match b.create(&dir) {
                Ok(()) => return Some(dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    })
    .as_deref()
}

/// Suffix of a temp project's throwaway NIBDIR, next to its scratch directory.
const TEMP_NIBDIR: &str = ".nibdir";

/// `root` (canonical) is inside a temp directory.
fn in_temp(root: &Path) -> bool {
    let tmp = std::env::temp_dir();
    // Canonical the way the root is (no `\\?\` on Windows), or the two never match.
    let tmp = nib_proto::path::canonical(&tmp).unwrap_or(tmp);
    root.starts_with(&tmp)
        || ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp", "/private/var/folders"].iter().any(|t| root.starts_with(t))
}

/// Make (or find) both directories for the project at canonical `root`.
pub fn setup(root: &Path) -> std::io::Result<Places> {
    let slug = slug(root);
    let base = xdg("XDG_CACHE_HOME", &[".cache"]).join("scratch");
    private_dir(&base)?;
    prune(&base);
    let scratch = base.join(format!("{slug}-{}", std::process::id()));
    private_dir(&scratch)?;

    let nibdir = if in_temp(root) {
        base.join(format!("{slug}-{}{TEMP_NIBDIR}", std::process::id()))
    } else {
        let dir = xdg("XDG_DATA_HOME", &[".local", "share"]).join("nibdir");
        private_dir(&dir)?;
        dir.join(nibdir_name(&dir, root, &slug)?)
    };
    private_dir(&nibdir)?;
    Ok(Places { scratch, nibdir })
}

type Index = std::collections::BTreeMap<String, String>;

fn read_index(path: &Path) -> Index {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The folder in `dir` that belongs to `root`: the one `projects.json` remembers, else the
/// project's name, or `<name>-2`, `-3`, … if that is taken (by another project, or by anything
/// already on disk that isn't in the index).
fn nibdir_name(dir: &Path, root: &Path, slug: &str) -> std::io::Result<String> {
    let index_path = dir.join("projects.json");
    let key = root.to_string_lossy().into_owned();
    if let Some(name) = read_index(&index_path).get(&key) {
        return Ok(name.clone());
    }
    let mut index = read_index(&index_path);
    let taken = |name: &str, index: &Index| index.values().any(|v| v == name) || dir.join(name).exists();
    let mut name = slug.to_string();
    let mut n = 2;
    while taken(&name, &index) {
        name = format!("{slug}-{n}");
        n += 1;
    }
    // Another nibd may have written meanwhile: keep its entries (and its answer for this root).
    let fresh = read_index(&index_path);
    if let Some(name) = fresh.get(&key) {
        return Ok(name.clone());
    }
    index.extend(fresh);
    index.insert(key, name.clone());
    crate::fs_ops::write_whole(&index_path, serde_json::to_vec_pretty(&index).unwrap_or_default())?;
    Ok(name)
}

fn alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: plain Win32 calls on a handle we open and close here.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // No such process; or one we may not query (another user's), which is not ours.
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            return ok && code == STILL_ACTIVE as u32;
        }
    }
    #[allow(unreachable_code)]
    if Path::new("/proc/self").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    // No /proc (macOS): ask the shell. Only runs for directories old enough to delete.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Newest modification time of anything in `dir` (one level of recursion is enough to notice
/// recent use; deep trees are rare in scratch).
fn last_change(dir: &Path) -> Option<std::time::SystemTime> {
    let mut t = std::fs::metadata(dir).and_then(|m| m.modified()).ok()?;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
            t = t.max(m);
        }
    }
    Some(t)
}

/// Delete scratch directories whose daemon is gone and that haven't changed for `KEEP_SCRATCH`.
fn prune(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).map(|n| n.trim_end_matches(TEMP_NIBDIR));
        let Some(pid) = name.and_then(|n| n.rsplit_once('-')).and_then(|(_, pid)| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let old = last_change(&p).is_some_and(|t| t.elapsed().is_ok_and(|age| age > KEEP_SCRATCH));
        if old && !alive(pid) {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_projects() {
        assert!(in_temp(&nib_proto::path::canonical(&std::env::temp_dir()).unwrap().join("x")));
        assert!(in_temp(Path::new("/tmp/proj")));
        assert!(!in_temp(Path::new("/Users/me/src/nibble")) && !in_temp(Path::new("/home/me/src/nibble")));
    }

    #[test]
    fn nibdir_is_named_after_the_project() {
        let dir = std::env::temp_dir().join(format!("nibd-nibdir-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = |root: &str| nibdir_name(&dir, Path::new(root), &slug(Path::new(root))).unwrap();
        assert_eq!(name("/home/me/src/api"), "api");
        assert_eq!(name("/home/me/src/api"), "api", "the same project keeps its folder");
        assert_eq!(name("/home/me/work/api"), "api-2", "another project named api");
        // A folder the user made by hand isn't taken over.
        std::fs::create_dir(dir.join("web")).unwrap();
        assert_eq!(name("/home/me/src/web"), "web-2");
        assert_eq!(name("/home/me/work/api"), "api-2");
        let index = read_index(&dir.join("projects.json"));
        assert_eq!(index.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
