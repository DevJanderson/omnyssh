// Release builds link as a Windows GUI binary, so launching the app never opens a
// console window alongside it. Debug builds keep the console for cargo output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! OmnySSH Desktop entry point. Wires the tauri-specta IPC boundary (commands +
//! events), regenerates the TypeScript bindings in dev, spawns the core-event
//! bridge, and boots the window (tech-gui.md §3.3–§3.4).

mod bridge;
mod commands;
mod dto;
mod error;
mod events;
mod state;

use commands::hosts::{delete_host, list_hosts, refresh_metrics, reload_hosts, save_host};
use commands::keysetup::start_key_setup;
use commands::sftp::{
    list_local_dir, preview_local_file, sftp_close, sftp_delete, sftp_download, sftp_list,
    sftp_mkdir, sftp_open, sftp_preview, sftp_rename, sftp_upload,
};
use commands::snippets::{delete_snippet, execute_snippet, list_snippets, save_snippet};
use commands::terminal::{terminal_close, terminal_open, terminal_resize, terminal_write};
use commands::update::{check_update, install_update, load_update_config, save_update_config};
use omnyssh_core::event::{CoreEvent, SessionId};
use omnyssh_core::ssh::pty::PtyManager;
use state::GuiState;
use tauri::webview::PageLoadEvent;
use tauri::Manager;
use tauri_specta::{collect_commands, collect_events, Builder};

// Absolute at build time, so the export target is independent of the run CWD.
const BINDINGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/src/lib/bindings.ts");

/// How long the hidden window may wait for the page before it is revealed anyway.
/// The app has no tray icon, so a frontend that never loads must not leave a
/// running process the user cannot see or reach.
const REVEAL_FALLBACK: std::time::Duration = std::time::Duration::from_secs(3);

/// The single definition of the IPC surface. Shared by `main` (dev export +
/// wiring) and the drift test so they can never disagree.
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            list_hosts,
            reload_hosts,
            save_host,
            delete_host,
            list_snippets,
            save_snippet,
            delete_snippet,
            execute_snippet,
            terminal_open,
            terminal_write,
            terminal_resize,
            terminal_close,
            sftp_open,
            sftp_list,
            sftp_upload,
            sftp_download,
            sftp_mkdir,
            sftp_rename,
            sftp_delete,
            sftp_preview,
            sftp_close,
            list_local_dir,
            preview_local_file,
            start_key_setup,
            refresh_metrics,
            check_update,
            install_update,
            load_update_config,
            save_update_config
        ])
        .events(collect_events![
            events::HostsLoaded,
            events::HostStatusChanged,
            events::MetricsUpdated,
            events::ServicesDetected,
            events::ServicesFailed,
            events::SnippetResult,
            events::TerminalExited,
            events::SftpConnected,
            events::SftpDirListed,
            events::SftpOpDone,
            events::SftpDisconnected,
            events::FilePreview,
            events::TransferProgress,
            events::KeySetupProgress,
            events::KeySetupComplete,
            events::KeySetupFailed,
            events::KeySetupRollback,
            events::UpdateAvailable,
            events::Error
        ])
}

/// Writes `bindings.ts` for the current IPC surface. Dev/test only — release
/// builds ship no exporter and never regenerate.
#[cfg(debug_assertions)]
fn export_bindings(path: impl AsRef<std::path::Path>) {
    // Emit `number` for u64 fields (`ageSeconds`, later the session/transfer ids);
    // their values stay well within JS's safe-integer range.
    let ts = specta_typescript::Typescript::default()
        .bigint(specta_typescript::BigIntExportBehavior::Number);
    specta_builder()
        .export(ts, path)
        .expect("failed to export TypeScript bindings");
}

fn main() {
    let builder = specta_builder();

    #[cfg(debug_assertions)]
    export_bindings(BINDINGS_PATH);

    tauri::Builder::default()
        // Persists UI prefs (theme, sidebar collapse, refresh interval) from the
        // frontend JS API — no bespoke command (tech-gui.md §4.2, §5.1).
        .plugin(tauri_plugin_store::Builder::new().build())
        // Desktop self-update (tech-gui.md §4.3); endpoints/signing land in Stage 5.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Opens the support dialog's GitHub/Telegram links in the default browser.
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(builder.invoke_handler())
        // The window is created hidden (tauri.conf.json `visible: false`) so the
        // launch never shows the webview's blank base colour; reveal it once the
        // document — stylesheet included — is up.
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), PageLoadEvent::Finished) {
                let window = webview.window();
                // Reveal once: a later page load must not raise the window over
                // whatever the user is doing.
                if !window.is_visible().unwrap_or(false) {
                    let _ = window.show();
                    // A window shown after build does not become key on its own everywhere.
                    let _ = window.set_focus();
                }
            }
        })
        .setup(move |app| {
            builder.mount_events(app);

            let reveal = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(REVEAL_FALLBACK).await;
                if let Some(window) = reveal.get_webview_window("main") {
                    // Only when the page never got there: on macOS showing an
                    // already-visible window raises it over whatever the user
                    // switched to meanwhile.
                    if !window.is_visible().unwrap_or(false) {
                        let _ = window.show();
                    }
                }
            });

            let (engine_tx, engine_rx) = tokio::sync::mpsc::channel::<CoreEvent>(256);
            // The additive PTY raw-byte tap (§3.6): the manager mirrors each session's
            // bytes into `raw_tx`; a forwarder demuxes them into per-tab channels.
            let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<(SessionId, Vec<u8>)>(256);
            let pty = PtyManager::with_raw_output(raw_tx);

            // Pre-load the shared host config so the first `list_hosts` paints
            // immediately. A load failure here is non-fatal — the frontend's
            // `reload_hosts` re-attempts and surfaces the error (tech-gui.md §3.4).
            let gui_state = GuiState::new(engine_tx, pty);
            if let Ok(hosts) = omnyssh_core::config::load_all_hosts() {
                gui_state.set_hosts(hosts);
            }
            app.manage(gui_state);

            // Spawn the forwarders after `manage` so both can reach `GuiState` via
            // `app.state()` (the bridge maps PtyExited; the tap routes raw bytes).
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(bridge::forward_core_events(handle.clone(), engine_rx));
            tauri::async_runtime::spawn(bridge::forward_terminal_output(handle, raw_rx));

            // The pollers and the startup update check are both started by the frontend's
            // first `reload_hosts`, once its event bridge is listening — starting them
            // here would race listener registration and drop `HostStatusChanged` /
            // `UpdateAvailable` before the webview can receive them (§3.4).
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to launch OmnySSH Desktop");
}

#[cfg(test)]
mod tests {
    use super::{export_bindings, BINDINGS_PATH};

    /// The committed bindings must match a fresh export — fails loudly on drift
    /// without mutating the tracked file (tech-gui.md §0.2 acceptance, §3.3).
    #[test]
    fn committed_bindings_are_in_sync() {
        let tmp = std::env::temp_dir().join(format!("omnyssh-bindings-{}.ts", std::process::id()));
        export_bindings(&tmp);
        let generated = std::fs::read_to_string(&tmp).expect("read fresh bindings");
        let _ = std::fs::remove_file(&tmp);

        let committed = std::fs::read_to_string(BINDINGS_PATH).expect("read committed bindings");
        assert_eq!(
            committed, generated,
            "ui/src/lib/bindings.ts is out of date — regenerate with `cargo tauri dev`"
        );
    }
}
