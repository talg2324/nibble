use nib_proto::path::{canonical, to_wire};
use nib_proto::{DirEntry, EntryKind, ErrorKind, FsOp, Response, RpcError};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

pub fn expand_home(p: &str) -> PathBuf {
    if p == "~" || p.starts_with("~/") || (cfg!(windows) && p.starts_with("~\\")) {
        if let Some(home) = nib_proto::path::home_dir() {
            return home.join(p[1..].trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(p)
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
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.nib-{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        if let Some(meta) = &existing {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
    }
    if let Err(e) = fs::rename(&tmp, &target) {
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
