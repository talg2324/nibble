//! macOS: run the window from an app bundle, so the Dock, Cmd-Tab and the menu bar show it as
//! nibble, with its icon.
//!
//! The Dock names an unbundled program after its file ("nib") and draws the generic executable's
//! icon, and nothing a running program can call changes the Dock's tile afterwards. So before the
//! window starts, `nib` makes (or refreshes) `nibble.app` next to its binary, whose executable is
//! a symlink to that binary, and execs itself through it: the same process, terminal, arguments
//! and folder, now inside a bundle. Things nib keeps next to its binary (nibd, .skills, daemons/)
//! are found from the real path, `crate::exe`.

use std::path::{Path, PathBuf};

/// This binary's real path: through the bundle's symlink to the file itself. `current_exe`
/// gives the path it was started by.
pub(crate) fn exe() -> std::io::Result<PathBuf> {
    let p = std::env::current_exe()?;
    Ok(p.canonicalize().unwrap_or(p))
}

/// Exec this window through `nibble.app`, unless it is in a bundle already or the bundle cannot
/// be written (then it runs as it is, named "nib" in the Dock).
#[cfg(target_os = "macos")]
pub(crate) fn enter() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("NIB_NO_BUNDLE").is_some() {
        return;
    }
    let (Ok(started), Ok(real)) = (std::env::current_exe(), exe()) else { return };
    if in_bundle(&started) {
        return;
    }
    let Some(app) = real.parent().map(|d| d.join("nibble.app")) else { return };
    if let Err(e) = ensure(&app, &real) {
        crate::control::log("info", &format!("running unbundled: {}: {e}", app.display()));
        return;
    }
    let err = std::process::Command::new(app.join("Contents/MacOS/nib")).args(std::env::args_os().skip(1)).exec();
    crate::control::log("info", &format!("running unbundled: exec {}: {err}", app.display()));
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn enter() {}

#[cfg(target_os = "macos")]
fn in_bundle(exe: &Path) -> bool {
    let dirs: Vec<_> = exe.parent().into_iter().flat_map(|d| d.ancestors()).take(3).map(|d| d.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
    matches!(dirs.as_slice(), [m, c, a, ..] if m == "MacOS" && c == "Contents" && a.ends_with(".app"))
}

/// The bundle, as this binary wants it: each file rewritten only when it differs, and each
/// write made under a temporary name and renamed, so two windows starting at once both find a
/// whole bundle.
#[cfg(target_os = "macos")]
fn ensure(app: &Path, real: &Path) -> std::io::Result<()> {
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;
    let plist = write_if_changed(&contents.join("Info.plist"), info_plist().as_bytes())?;
    let icon = write_if_changed(&contents.join("Resources/nibble.icns"), ICNS)?;
    let link = contents.join("MacOS/nib");
    if std::fs::read_link(&link).ok().as_deref() != Some(real) {
        let tmp = contents.join(format!("MacOS/.nib.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(real, &tmp)?;
        std::fs::rename(&tmp, &link)?;
    }
    if plist || icon {
        refresh(app);
    }
    Ok(())
}

/// A new name or icon in a bundle macOS has seen before: the Dock keeps the one it cached for
/// that path until the bundle is newer and checked in again. Best effort; lsregister is the
/// system's own tool for it.
#[cfg(target_os = "macos")]
fn refresh(app: &Path) {
    if let Ok(f) = std::fs::File::open(app) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = std::process::Command::new(LSREGISTER).arg("-f").arg(app).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

/// assets/icon.svg on Apple's icon grid (the artwork 824 of 1024 px, centred), so it sits in the
/// Dock at the size of the icons beside it.
#[cfg(target_os = "macos")]
const ICNS: &[u8] = include_bytes!("../../../assets/nibble.icns");

#[cfg(target_os = "macos")]
/// Whether it wrote.
fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>nibble</string>
  <key>CFBundleDisplayName</key><string>nibble</string>
  <key>CFBundleIdentifier</key><string>com.github.talg2324.nibble</string>
  <key>CFBundleExecutable</key><string>nib</string>
  <key>CFBundleIconFile</key><string>nibble</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
"#,
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn knows_a_path_inside_a_bundle() {
        assert!(in_bundle(Path::new("/x/nibble.app/Contents/MacOS/nib")));
        assert!(!in_bundle(Path::new("/x/target/fast/nib")));
        assert!(!in_bundle(Path::new("/x/Contents/MacOS/nib")));
    }

    #[test]
    fn makes_a_bundle_that_points_at_the_binary() {
        let dir = std::env::temp_dir().join(format!("nib-bundle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("nib");
        std::fs::write(&real, b"").unwrap();
        let app = dir.join("nibble.app");
        ensure(&app, &real).unwrap();
        ensure(&app, &real).unwrap(); // a second window: nothing to change
        assert_eq!(std::fs::read_link(app.join("Contents/MacOS/nib")).unwrap(), real);
        assert_eq!(std::fs::read(app.join("Contents/Resources/nibble.icns")).unwrap(), ICNS);
        assert!(std::fs::read_to_string(app.join("Contents/Info.plist")).unwrap().contains("<string>nibble</string>"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
