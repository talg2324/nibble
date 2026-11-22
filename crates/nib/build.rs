fn main() {
    // Windows: give nib.exe its icon and its name, "nibble" (nib.rc). macOS and Linux take the name
    // from the running window (`name_the_app` and the app id in main.rs), and Linux the icon from
    // a nibble.desktop.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=nib.rc");
        println!("cargo:rerun-if-changed=../../assets/icon.ico");
        embed_resource::compile("nib.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
