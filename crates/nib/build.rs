fn main() {
    // Windows: give nib.exe its icon (nib.rc). Linux and macOS take theirs from the desktop
    // (nib.desktop / the app bundle), not from the binary.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=../../assets/icon.ico");
        embed_resource::compile("nib.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
