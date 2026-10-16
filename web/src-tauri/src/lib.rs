//! The nazar shell: the same web app, hosted in a native window.

#[cfg(target_os = "android")]
use tauri::Manager as _;

/// The phone's half of the updater, as a Kotlin plugin the commands below
/// bridge to. Everything the update does on Android is Android's own: the
/// manifest fetch, the download, the signer read of this install, and the
/// installer intent.
#[cfg(target_os = "android")]
mod android {
    use std::sync::Mutex;

    use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
    use tauri::{Manager, Runtime};

    /// The Kotlin handle, and the release the last check offered. The install
    /// works from the remembered one rather than asking again: the line the
    /// reader tapped names that version, and a check between the two would
    /// install a release the line never named.
    pub struct Updater<R: Runtime>(pub PluginHandle<R>, pub Mutex<Option<Found>>);

    #[derive(serde::Serialize)]
    pub struct CheckArgs {
        pub version: String,
        pub endpoint: String,
    }

    #[derive(Clone, serde::Deserialize)]
    pub struct Found {
        pub version: String,
        pub url: String,
    }

    #[derive(serde::Serialize)]
    pub struct InstallArgs {
        pub version: String,
        pub url: String,
    }

    pub fn init<R: Runtime>() -> TauriPlugin<R> {
        Builder::new("androidupdate")
            .setup(|app, api| {
                let handle = api.register_android_plugin("dev.vedhavyas.nazar", "UpdatePlugin")?;
                app.manage(Updater(handle, Mutex::new(None)));
                Ok(())
            })
            .build()
    }
}

/// The manifest endpoint the bridge reads, out of the config where it is
/// written.
#[cfg(target_os = "android")]
fn update_endpoint(app: &tauri::AppHandle) -> Result<String, String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("endpoints"))
        .and_then(|endpoints| endpoints.as_array())
        .and_then(|endpoints| endpoints.first())
        .and_then(|endpoint| endpoint.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "no updater endpoint is configured".to_string())
}

#[cfg(target_os = "android")]
async fn android_check(app: &tauri::AppHandle) -> Result<Option<android::Found>, String> {
    let handle = app.state::<android::Updater<tauri::Wry>>();
    let args = android::CheckArgs {
        version: app.package_info().version.to_string(),
        endpoint: update_endpoint(app)?,
    };
    handle
        .0
        .run_mobile_plugin_async("check", args)
        .await
        .map_err(|err| err.to_string())
}

/// The version an update check found, or `None` when this build is current.
/// What it found is remembered for the install that line-offering may follow.
#[cfg(target_os = "android")]
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let found = android_check(&app).await?;
    if let Ok(mut remembered) = app.state::<android::Updater<tauri::Wry>>().1.lock() {
        *remembered = found.clone();
    }
    Ok(found.map(|found| found.version))
}

/// Download the remembered release, check it, and hand it to the system
/// installer, answering with the stage that finishes it: the phone's install
/// IS the prompt, and the app is replaced with it.
#[cfg(target_os = "android")]
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    let handle = app.state::<android::Updater<tauri::Wry>>();
    let found = handle
        .1
        .lock()
        .ok()
        .and_then(|remembered| remembered.clone())
        .ok_or_else(|| "no update is available".to_string())?;
    handle
        .0
        .run_mobile_plugin_async::<()>(
            "install",
            android::InstallArgs {
                version: found.version,
                url: found.url,
            },
        )
        .await
        .map_err(|err| err.to_string())?;
    Ok("install".to_string())
}

/// Tell the person about something, outside the window: the shell's half of
/// the web bridge's `notify` command, one shape on every platform.
#[tauri::command]
async fn notify(app: tauri::AppHandle, title: String, body: String) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt as _;

    // A phone that has not decided yet is asked on the first telling; a
    // refusal is Android's to remember, and asking again cannot reprompt.
    #[cfg(target_os = "android")]
    if app
        .notification()
        .permission_state()
        .is_ok_and(|state| state != tauri::plugin::PermissionState::Granted)
    {
        app.notification()
            .request_permission()
            .map_err(|err| err.to_string())?;
    }

    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|err| err.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // `invoke_handler` REPLACES the handler rather than adding to it, so the
    // Android arm carries every command the shell answers.
    #[cfg(target_os = "android")]
    let builder = tauri::Builder::default()
        .plugin(android::init())
        .invoke_handler(tauri::generate_handler![check_update, install_update, notify]);
    #[cfg(not(target_os = "android"))]
    let builder = tauri::Builder::default().invoke_handler(tauri::generate_handler![notify]);

    builder
        .plugin(tauri_plugin_notification::init())
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
