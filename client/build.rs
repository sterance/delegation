//! Embeds the icon into delegation-tray.exe. Only runs when building on
//! Windows (it needs the Windows resource compiler); other builds, and the
//! other binaries, are untouched.

fn main() {
    println!("cargo:rerun-if-changed=assets/delegation-tray.rc");
    println!("cargo:rerun-if-changed=assets/delegation.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for("assets/delegation-tray.rc", ["delegation-tray"], embed_resource::NONE)
            .manifest_optional()
            .expect("compiling delegation-tray.rc");
    }
}
