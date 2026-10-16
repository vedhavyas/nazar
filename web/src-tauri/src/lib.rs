//! The nazar shell: the same web app, hosted in a native window.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Info, not the plugin's trace default: the webview stack's trace
        // logging buries the app's own lines.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .build(),
        )
        .run(tauri::generate_context!())
        .expect("the nazar shell runs");
}
