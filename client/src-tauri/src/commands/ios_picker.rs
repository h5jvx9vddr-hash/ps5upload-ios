//! Files-app picker for the iOS build (lib/openDialog.ts on the renderer side).
//!
//! plugin-dialog's iOS picker copies files into the sandbox and can't pick
//! folders; tauri-plugin-ios-picker opens files and folders in place and
//! returns real paths the in-process engine can stream from. Elsewhere the
//! command errors — the renderer only calls it on iOS.

#[tauri::command]
pub async fn ios_pick_paths(
    app: tauri::AppHandle,
    directory: bool,
    multiple: bool,
) -> Result<Vec<String>, String> {
    #[cfg(target_os = "ios")]
    {
        use tauri::Manager;
        // The picker blocks until the user is done; keep it off the async workers.
        return tauri::async_runtime::spawn_blocking(move || {
            app.state::<tauri_plugin_ios_picker::IosPicker<tauri::Wry>>()
                .pick(tauri_plugin_ios_picker::PickRequest {
                    directory,
                    multiple,
                })
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, directory, multiple);
        Err("the Files-app picker is only available on iOS".to_string())
    }
}
