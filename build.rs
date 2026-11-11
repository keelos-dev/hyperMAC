// The bare binary (build.sh, the LaunchAgent's copy) gets an Info.plist in __TEXT,__info_plist:
// the "System Audio Recording" prompt for the CoreAudio tap (tap.rs) needs
// NSAudioCaptureUsageDescription, and a binary outside a bundle has no other place for it.
// hyperMAC.app (tools/package.sh) carries the same key in its own Info.plist.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("Info.plist");
    // Five spaces after the identifier: room for ".menu", which main.rs `write_menu_binary`
    // puts there (same length) in the menu bar's copy of the binary.
    let pad = " ".repeat(".menu".len());
    std::fs::write(
        &out,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.keelos.hypermac</string>{pad}
  <key>CFBundleName</key><string>hyperMAC</string>
  <key>NSAudioCaptureUsageDescription</key><string>hyperMAC plays this Mac's sound on the remote client and mutes it here meanwhile.</string>
</dict></plist>
"#
        ),
    )
    .unwrap();
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}", out.display());
}
