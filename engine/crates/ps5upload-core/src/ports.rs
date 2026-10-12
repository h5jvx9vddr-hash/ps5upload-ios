//! The two console ports, overridable at runtime.
//!
//! Upstream treats both as constants. This fork lets the user change them
//! (Connection screen): a loader on a non-default port, or a console reached
//! through port forwarding where :9120 shows up as some other number. The app
//! pushes its setting into the shell (Tauri command) and the engine
//! (`POST /api/console-ports`); everything that dials the console reads it
//! here. 0 means "back to the default".

use std::sync::atomic::{AtomicU16, Ordering};

/// The PS5 ELF loader's well-known port.
pub const DEFAULT_LOADER_PORT: u16 = 9021;
/// The port the ps5upload helper's AVA1 listener binds on the console.
pub const DEFAULT_HELPER_PORT: u16 = 9120;

static LOADER_PORT: AtomicU16 = AtomicU16::new(DEFAULT_LOADER_PORT);
static HELPER_PORT: AtomicU16 = AtomicU16::new(DEFAULT_HELPER_PORT);

/// The ELF loader port currently in effect.
pub fn loader_port() -> u16 {
    LOADER_PORT.load(Ordering::Relaxed)
}

/// The helper (AVA1) port currently in effect.
pub fn helper_port() -> u16 {
    HELPER_PORT.load(Ordering::Relaxed)
}

/// Set either port; `None` leaves it alone, `Some(0)` restores the default.
pub fn set_ports(loader: Option<u16>, helper: Option<u16>) {
    if let Some(p) = loader {
        let p = if p == 0 { DEFAULT_LOADER_PORT } else { p };
        LOADER_PORT.store(p, Ordering::Relaxed);
    }
    if let Some(p) = helper {
        let p = if p == 0 { DEFAULT_HELPER_PORT } else { p };
        HELPER_PORT.store(p, Ordering::Relaxed);
    }
}
