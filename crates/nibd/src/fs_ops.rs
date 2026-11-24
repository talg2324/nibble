use nib_proto::path::{canonical, to_wire};
use nib_proto::{DirEntry, EntryKind, ErrorKind, FsOp, Response, RpcError};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Write `path` whole or not at all: beside it under a hidden name (which the folders' readers
/// skip), then renamed over it. For the state files that something else reads at any moment
/// (mailbox, apps, reviews, plans, branch records). A file the user edits goes through
/// `write_file`, which keeps permissions and syncs.
pub fn write_whole(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    let (mut f, tmp) = new_beside(path, None)?;
    f.write_all(bytes.as_ref()).and_then(|_| fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// A new, empty file beside `target`, to be written and renamed over it, and its path.
///
/// The folder may be one others write too (a shared checkout, an agent's work tree). So the
/// name cannot be told in advance (64 random bits), and the file is made only if nothing has
/// that name (`create_new`: never through a link somebody left there, never a file somebody
/// else made). `mode` (Unix): the permissions it is made with, for a target whose own are
/// narrower than the default, so that what is written is at no moment readable by more
/// people than the target is.
pub(crate) fn new_beside(target: &Path, mode: Option<u32>) -> std::io::Result<(fs::File, PathBuf)> {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut last = None;
    for _ in 0..8 {
        let tmp = target.with_file_name(format!(".{name}.nib-{}.tmp", &crate::tokens::fresh()[..16]));
        let mut open = fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            std::os::unix::fs::OpenOptionsExt::mode(&mut open, mode & 0o7777);
        }
        #[cfg(not(unix))]
        let _ = mode;
        match open.open(&tmp) {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no name for a temporary file")))
}

pub fn expand_home(p: &str) -> PathBuf {
    nib_proto::path::expand_home(p)
}

pub fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn read_file(path: &Path) -> Result<Response, RpcError> {
    let data = fs::read(path)?;
    let meta = fs::metadata(path)?;
    Ok(Response::File {
        path: to_wire(path),
        data,
        mtime_ns: mtime_ns(&meta),
    })
}

/// Atomic write: temp file in the same dir, fsync, rename. Preserves permissions of an existing
/// file. Writes through symlinks (resolves the target first).
pub fn write_file(
    path: &Path,
    data: &[u8],
    expected_mtime_ns: Option<u128>,
) -> Result<Response, RpcError> {
    let target = match canonical(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e.into()),
    };
    let existing = fs::metadata(&target).ok();
    if let (Some(expected), Some(meta)) = (expected_mtime_ns, &existing) {
        let actual = mtime_ns(meta);
        if actual != expected {
            return Err(RpcError::new(
                ErrorKind::Conflict,
                format!("{} changed on disk since it was opened", target.display()),
            ));
        }
    }
    #[cfg(unix)]
    let mode = existing.as_ref().map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));
    #[cfg(not(unix))]
    let mode = None;
    let (mut f, tmp) = new_beside(&target, mode)?;
    let written = (|| {
        f.write_all(data)?;
        f.sync_all()?;
        // Exactly the target's, whatever the umask took from the mode it was made with.
        if let Some(meta) = &existing {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        drop(f);
        fs::rename(&tmp, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    let meta = fs::metadata(&target)?;
    Ok(Response::Written {
        path: to_wire(&target),
        mtime_ns: mtime_ns(&meta),
    })
}

pub fn list_dir(path: &Path) -> Result<Response, RpcError> {
    let path = &canonical(path)?;
    let mut entries = Vec::new();
    for e in fs::read_dir(path)? {
        let e = e?;
        let ft = e.file_type()?;
        let kind = if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else if ft.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        entries.push(DirEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Dir, &a.name).cmp(&(b.kind != EntryKind::Dir, &b.name))
    });
    Ok(Response::Dir {
        path: to_wire(path),
        entries,
    })
}

/// `rel` under `root`. Refuses anything that could point elsewhere (absolute, `..`).
fn inside(root: &Path, rel: &str) -> Result<PathBuf, RpcError> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(RpcError::new(ErrorKind::BadRequest, format!("not a path inside the project: {rel:?}")));
    }
    Ok(root.join(p))
}

fn must_be_free(p: &Path, rel: &str) -> Result<(), RpcError> {
    match fs::symlink_metadata(p) {
        Ok(_) => Err(RpcError::new(ErrorKind::Conflict, format!("{rel} already exists"))),
        Err(_) => Ok(()),
    }
}

/// The file tree's create / rename / delete.
pub fn fs_op(root: &Path, op: &FsOp) -> Result<Response, RpcError> {
    match op {
        FsOp::CreateFile { path } => {
            let p = inside(root, path)?;
            must_be_free(&p, path)?;
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::OpenOptions::new().write(true).create_new(true).open(&p)?;
        }
        FsOp::CreateDir { path } => {
            let p = inside(root, path)?;
            must_be_free(&p, path)?;
            fs::create_dir_all(&p)?;
        }
        FsOp::Rename { from, to } => {
            let (src, dst) = (inside(root, from)?, inside(root, to)?);
            fs::symlink_metadata(&src)?;
            must_be_free(&dst, to)?;
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            if !crate::git::mv(root, from, to) {
                fs::rename(&src, &dst)?;
            }
        }
        FsOp::Delete { paths } => {
            for path in paths {
                let p = inside(root, path)?;
                // A link is removed itself, never what it points to.
                if fs::symlink_metadata(&p)?.is_dir() {
                    fs::remove_dir_all(&p)?;
                } else {
                    fs::remove_file(&p)?;
                }
            }
        }
        FsOp::Put { dirs, files } => {
            let targets = files.iter().map(|f| inside(root, &f.path)).collect::<Result<Vec<_>, _>>()?;
            for (p, f) in targets.iter().zip(files) {
                must_be_free(p, &f.path)?;
            }
            for d in dirs {
                fs::create_dir_all(inside(root, d)?)?;
            }
            for (p, f) in targets.iter().zip(files) {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::OpenOptions::new().write(true).create_new(true).open(p)?.write_all(&f.data)?;
            }
        }
    }
    Ok(Response::Ok)
}

#[cfg(all(test, unix))]
mod write_tests {
    use super::*;

    /// A folder others may write (a shared checkout, an agent's work tree): a link left where the
    /// temporary file used to be made, by a name that could be told in advance, sent the save
    /// through it, into any file the user may write.
    #[test]
    fn a_save_does_not_follow_a_link_left_where_its_temporary_file_goes() {
        let dir = std::env::temp_dir().join(format!("nibd-save-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("shared")).unwrap();
        let (victim, target) = (dir.join("authorized_keys"), dir.join("shared/notes.txt"));
        fs::write(&victim, "the user's keys\n").unwrap();
        fs::write(&target, "old\n").unwrap();
        for name in ["notes.txt", "state.json"] {
            // The names both writers used before: the process id, and a count from 0.
            for planted in [format!(".{name}.nib-{}.tmp", std::process::id()), format!(".{name}.{}-0.tmp", std::process::id())] {
                std::os::unix::fs::symlink(&victim, dir.join("shared").join(planted)).unwrap();
            }
        }
        write_file(&target, b"new\n", None).unwrap();
        write_whole(&dir.join("shared/state.json"), "{}").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "the user's keys\n");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        assert!(!fs::symlink_metadata(&target).unwrap().file_type().is_symlink() && !fs::symlink_metadata(dir.join("shared/state.json")).unwrap().file_type().is_symlink());
        // A file that was private stays so, and is never anything wider on the way.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        write_file(&target, b"secret\n", None).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        // Nothing is left beside them.
        let left: Vec<String> = fs::read_dir(dir.join("shared")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".tmp") && !fs::symlink_metadata(dir.join("shared").join(n)).unwrap().file_type().is_symlink()).collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
