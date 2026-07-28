//! Launch-time contract of the desktop app. These are link-time or native-window
//! settings that no runtime assertion can reach from a test binary (`cargo test`
//! always builds with `debug_assertions`), so they are guarded at their source.

const MAIN_RS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"));

/// Without this attribute the binary links as a console app and Windows opens a
/// terminal next to the window for the whole session.
#[test]
fn release_builds_link_as_a_windows_gui_binary() {
    assert!(
        MAIN_RS.contains(r#"windows_subsystem = "windows""#),
        "src/main.rs no longer declares windows_subsystem — release builds would \
         open a console window alongside the app on Windows"
    );
}
