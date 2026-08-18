//! Launch-time contract of the desktop app. These are link-time or native-window
//! settings that no runtime assertion can reach from a test binary (`cargo test`
//! always builds with `debug_assertions`), so they are guarded at their source.

use std::path::Path;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
const MAIN_RS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));

/// Without this attribute the binary links as a console app and Windows opens a
/// terminal next to the window for the whole session.
#[test]
fn release_builds_link_as_a_windows_gui_binary() {
    assert!(
        MAIN_RS.contains(r#"#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]"#),
        "src/main.rs no longer declares the windows_subsystem attribute — release \
         builds would open a console window alongside the app on Windows"
    );
}

/// The window is revealed only once the page is up, so the launch never shows the
/// webview's blank base colour. The background is what the fallback reveal paints
/// when the frontend is slow, so it has to match the app's own dark surface.
#[test]
fn the_window_starts_hidden_on_the_dark_background() {
    let config: serde_json::Value =
        serde_json::from_str(&read(Path::new(MANIFEST_DIR).join("tauri.conf.json")))
            .expect("tauri.conf.json is valid JSON");
    let window = &config["app"]["windows"][0];

    assert_eq!(
        window["visible"],
        serde_json::json!(false),
        "the main window must be created hidden and revealed on page load"
    );
    assert_eq!(
        window["backgroundColor"].as_str().map(str::to_lowercase),
        Some(dark_background_token()),
        "the native window background drifted from the dark --bg token"
    );
}

/// The other half of `visible: false`: drop either reveal and the app becomes a
/// running process with no window and no way to reach it.
#[test]
fn a_hidden_window_is_always_revealed() {
    assert!(
        MAIN_RS.contains(".on_page_load("),
        "the page-load reveal is gone — the window would stay hidden until the fallback"
    );
    assert!(
        MAIN_RS.contains("REVEAL_FALLBACK"),
        "the fallback reveal is gone — a frontend that never loads would leave no window"
    );
}

/// The reveal doubles as the signal that the page rendered, and the AppImage render
/// retry reads it. Drop this one store and every AppImage launch looks like a failed
/// one, so the app would restart itself into software rendering every single time —
/// with every other test still green.
#[test]
fn a_loaded_page_is_recorded_for_the_render_retry() {
    assert!(
        MAIN_RS.contains("PAGE_LOADED.store("),
        "the page-load flag is no longer set — the AppImage would restart itself on \
         every launch"
    );
}

/// The rpm bundler writes `Requires:` from this list and nothing else — it never scans
/// the binary — so an empty list ships a package that installs onto a system with no
/// webview and then dies at launch. Sonames, not package names: the package providing
/// them is called something different on every RPM distro.
#[test]
fn the_rpm_declares_the_libraries_it_links() {
    let config: serde_json::Value =
        serde_json::from_str(&read(Path::new(MANIFEST_DIR).join("tauri.conf.json")))
            .expect("tauri.conf.json is valid JSON");
    let depends = config["bundle"]["linux"]["rpm"]["depends"]
        .as_array()
        .expect("the rpm bundle declares its runtime dependencies");

    for lib in [
        "libwebkit2gtk-4.1.so.0()(64bit)",
        "libjavascriptcoregtk-4.1.so.0()(64bit)",
        "libgtk-3.so.0()(64bit)",
    ] {
        assert!(
            depends.iter().any(|d| d == lib),
            "the rpm no longer requires {lib} — dnf would install a build that cannot start"
        );
    }
}

/// `--bg` of the dark theme — the single source of truth for the app's backdrop.
fn dark_background_token() -> String {
    let css = read(Path::new(MANIFEST_DIR).join("ui/src/app.css"));
    let block = css
        .split_once(":root[data-theme='dark']")
        .expect("app.css declares a dark theme block")
        .1;
    block[..block.find('}').expect("the dark theme block is closed")]
        .split_once("--bg:")
        .expect("the dark theme block declares --bg")
        .1
        .split(';')
        .next()
        .expect("--bg is terminated")
        .trim()
        .to_lowercase()
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
