//! Files-app document picker for iOS (see ios/Sources/IosPicker).
//!
//! plugin-dialog's iOS picker copies every file into the sandbox and can't
//! pick folders. This one presents `UIDocumentPickerViewController` with
//! `asCopy: false` for files or folders, starts security-scoped access on what
//! was picked (kept for the life of the process, and re-acquired from saved
//! bookmarks on the next launch so a resumed upload can still read its
//! source), and returns plain filesystem paths the in-process engine opens
//! like any other.
//!
//! It also carries the rest of the iOS shell: the Local Network permission
//! bridge (`window.PS5UploadNet`, as on Android) and Bonjour discovery.

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

/// One resolved Bonjour service (`ios/Sources`, BonjourSession).
#[derive(Debug, Clone, Deserialize)]
pub struct BonjourHit {
    /// Service type as Bonjour reports it, e.g. `_ftp._tcp.`.
    pub service: String,
    pub name: String,
    /// `ps5.local.`; empty when the service didn't name one.
    pub host: String,
    /// Numeric IPv4/IPv6 addresses.
    pub addresses: Vec<String>,
    pub port: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseRequest {
    types: Vec<String>,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct BrowseResponse {
    hits: Vec<BonjourHit>,
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

impl<R: Runtime> IosPicker<R> {
    /// Browse the given Bonjour service types (`_ftp._tcp.`) through the
    /// system's mDNSResponder for `timeout_ms` and return what resolved.
    /// Blocks for the whole browse: call it off the async runtime.
    pub fn bonjour_browse(&self, types: Vec<String>, timeout_ms: u64) -> Result<Vec<BonjourHit>, String> {
        #[cfg(target_os = "ios")]
        {
            return self
                .handle
                .run_mobile_plugin::<BrowseResponse>("bonjourBrowse", BrowseRequest { types, timeout_ms })
                .map(|r| r.hits)
                .map_err(|e| e.to_string());
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = (types, timeout_ms);
            Err("Bonjour browsing through the plugin is only available on iOS".to_string())
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
