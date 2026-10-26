use std::path::PathBuf;

/// Shown when the app is built before the UI (`ui/dist` is gitignored).
const PLACEHOLDER: &str = "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
<title>DntDatabase</title></head><body><p>The UI has not been built. Run \
<code>pnpm --dir ui build</code> and rebuild.</p></body></html>\n";

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));

    // `tauri::generate_context!` requires `frontendDist` to exist at compile time, so a
    // fresh clone gets a placeholder page.
    let dist = manifest_dir.join("../ui/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        std::fs::create_dir_all(&dist).expect("create ui/dist");
        std::fs::write(&index, PLACEHOLDER).expect("write ui/dist/index.html");
    }

    // Tauri links Common Controls v6 (dialogs), which needs an application manifest. Tauri
    // would embed it in the app binary only, and test binaries then fail to start with
    // STATUS_ENTRYPOINT_NOT_FOUND; so embed it in every linked target instead.
    let mut windows = tauri_build::WindowsAttributes::new();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        windows = tauri_build::WindowsAttributes::new_without_app_manifest();
        let manifest = manifest_dir.join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        // The main thread runs the event loop, WebView2's callbacks and the IPC handler, which
        // builds every async command's future before spawning it. The default 1 MB reserve
        // left no margin in release builds (a query overflowed it); `commands::guard` keeps
        // those futures small, and 8 MB of reserve (committed only as used) is the backstop.
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("tauri build");
}
