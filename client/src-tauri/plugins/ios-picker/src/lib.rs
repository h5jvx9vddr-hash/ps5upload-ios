//! Files-app document picker for iOS (see ios/Sources/IosPicker).
//!
//! plugin-dialog's iOS picker copies every file into the sandbox and can't
//! pick folders. This one presents `UIDocumentPickerViewController` with
//! `asCopy: false` for files or folders, starts security-scoped access on what
//! was picked (kept for the life of the process, and re-acquired from saved
//! bookmarks on the next launch so a resumed upload can still read its
//! source), and returns plain filesystem paths the in-process engine opens
//! like any other.

use serde::{Deserialize, Serialize};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_ios_picker);

#[derive(Debug, Clone, Serialize)]
pub struct PickRequest {
    pub directory: bool,
    pub multiple: bool,
}

#[derive(Debug, Deserialize)]
struct PickResponse {
    paths: Vec<String>,
}

/// Managed state: the registered Swift plugin (iOS only).
pub struct IosPicker<R: Runtime> {
    #[cfg(target_os = "ios")]
    handle: tauri::plugin::PluginHandle<R>,
    _runtime: std::marker::PhantomData<fn() -> R>,
}

impl<R: Runtime> IosPicker<R> {
    /// Present the picker and block until the user picks or cancels (an empty
    /// list). Call it off the async runtime (spawn_blocking).
    pub fn pick(&self, request: PickRequest) -> Result<Vec<String>, String> {
        #[cfg(target_os = "ios")]
        {
            return self
                .handle
                .run_mobile_plugin::<PickResponse>("pick", request)
                .map(|r| r.paths)
                .map_err(|e| e.to_string());
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = request;
            Err("the Files-app picker is only available on iOS".to_string())
        }
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ios-picker")
        .setup(|app, api| {
            #[cfg(target_os = "ios")]
            let state = IosPicker {
                handle: api.register_ios_plugin(init_plugin_ios_picker)?,
                _runtime: std::marker::PhantomData,
            };
            #[cfg(not(target_os = "ios"))]
            let state = {
                let _ = api;
                IosPicker::<R> {
                    _runtime: std::marker::PhantomData,
                }
            };
            app.manage(state);
            Ok(())
        })
        .build()
}
