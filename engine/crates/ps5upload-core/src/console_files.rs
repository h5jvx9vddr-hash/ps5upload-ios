//! The console file manager's shared state, as the PC app sees it: the pinned / recent folders
//! the console page keeps on the console (`files.prefs_get` / `files.prefs_set`), and the
//! "Show on TV" / "Show on PC" handoffs (`files.handoff_put` / `files.handoff_take`).
//!
//! Every call answers `Ok(None)` when the helper is older and does not know the method
//! (`ERR_UNKNOWN_METHOD`): the engine then reports `{"supported":false}` and the PC app hides
//! these features, so nothing changes against an older helper.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mgmt::{self, m, MgmtError};

/// AVA1 `ERR_UNKNOWN_METHOD` (`protocol/ava1/schema/ava1.toml`): what an older helper answers
/// for a method it does not have. (`ps5upload-core` cannot depend on the `ava1` crate; a
/// drift test in `ps5upload-ava1` checks the number.)
pub const ERR_UNKNOWN_METHOD: u16 = 6;

/// The pinned folders, recent folders (newest first) and the last folder shown on the console.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesPrefs {
    #[serde(default)]
    pub favorites: Vec<String>,
    #[serde(default)]
    pub recent: Vec<String>,
    #[serde(default)]
    pub last: Option<String>,
}

/// A change to the prefs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrefsOp {
    Pin,
    Unpin,
    Visit,
}

/// Which side a handoff is for: the console page (Show on TV) or the PC app (Show on PC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Console,
    Pc,
}

/// A handoff that was waiting: open `path`; it was put `age_ms` ago (under two minutes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub path: String,
    #[serde(default)]
    pub age_ms: u64,
}

/// True when `e` is the helper saying it does not have the method.
pub fn is_unsupported(e: &anyhow::Error) -> bool {
    e.downcast_ref::<MgmtError>()
        .is_some_and(|m| m.status == ERR_UNKNOWN_METHOD)
}

fn supported<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if is_unsupported(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Get or update Minion's tile using the paired helper, independent of LAN web access.
pub fn minion_tile(addr: &str, tile: Option<bool>) -> Result<serde_json::Value> {
    let body = match tile {
        Some(tile) => serde_json::json!({ "tile": tile }),
        None => serde_json::json!({}),
    };
    match mgmt::call(addr, m::MINION_TILE, &serde_json::to_vec(&body)?) {
        Err(e) if is_unsupported(&e) => Ok(serde_json::json!({"supported":false})),
        Err(e) => Err(e),
        Ok(b) => {
            let mut v: serde_json::Value = serde_json::from_slice(&b)?;
            anyhow::ensure!(
                v.get("tile").and_then(|v| v.as_bool()).is_some(),
                "Minion settings: {}",
                v.get("err").unwrap_or(&v)
            );
            v["supported"] = true.into();
            Ok(v)
        }
    }
}

/// Reads the prefs, keeping only absolute paths (the console page's own rule).
fn parse_prefs(body: &[u8]) -> Result<FilesPrefs> {
    let mut p: FilesPrefs = serde_json::from_slice(body)?;
    p.favorites.retain(|x| x.starts_with('/'));
    p.recent.retain(|x| x.starts_with('/'));
    if p.last.as_deref().is_some_and(|x| !x.starts_with('/')) {
        p.last = None;
    }
    Ok(p)
}

/// The console's pinned / recent folders, or `None` from an older helper.
pub fn prefs_get(addr: &str) -> Result<Option<FilesPrefs>> {
    supported(mgmt::call(addr, m::FILES_PREFS_GET, b"").and_then(|b| parse_prefs(&b)))
}

/// Pins, unpins or records a visit of `path`; the new prefs, or `None` from an older helper.
pub fn prefs_set(addr: &str, op: PrefsOp, path: &str) -> Result<Option<FilesPrefs>> {
    let body = serde_json::json!({ "op": op, "path": path }).to_string();
    supported(mgmt::call(addr, m::FILES_PREFS_SET, body.as_bytes()).and_then(|b| parse_prefs(&b)))
}

/// Leaves `path` for `to` to open (it polls). `None` from an older helper.
pub fn handoff_put(addr: &str, to: Side, path: &str) -> Result<Option<()>> {
    let body = serde_json::json!({ "to": to, "path": path }).to_string();
    supported(mgmt::call(addr, m::FILES_HANDOFF_PUT, body.as_bytes()).map(|_| ()))
}

/// Takes the handoff waiting for `side`: `Some(None)` when there is none, `None` from an older
/// helper.
pub fn handoff_take(addr: &str, side: Side) -> Result<Option<Option<Handoff>>> {
    let body = serde_json::json!({ "for": side }).to_string();
    supported(
        mgmt::call(addr, m::FILES_HANDOFF_TAKE, body.as_bytes()).and_then(|b| {
            let v: serde_json::Value = serde_json::from_slice(&b)?;
            Ok(match v.get("path").and_then(|p| p.as_str()) {
                Some(p) if p.starts_with('/') => Some(Handoff {
                    path: p.to_string(),
                    age_ms: v.get("age_ms").and_then(|a| a.as_u64()).unwrap_or(0),
                }),
                _ => None,
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mgmt::{Method, MgmtTransport};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Answers each method from a table and records the bodies it was sent.
    struct Fake {
        reply: fn(u16) -> std::result::Result<Vec<u8>, u16>,
        seen: Mutex<Vec<(u16, String)>>,
    }

    impl MgmtTransport for Fake {
        fn call(
            &self,
            _addr: &str,
            method: Method,
            label: &str,
            body: &[u8],
            _timeout: Duration,
        ) -> Result<Option<Vec<u8>>> {
            self.seen
                .lock()
                .unwrap()
                .push((method.id, String::from_utf8_lossy(body).into_owned()));
            match (self.reply)(method.id) {
                Ok(b) => Ok(Some(b)),
                Err(status) => Err(MgmtError {
                    label: label.to_string(),
                    status,
                    cause: "unknown_method".into(),
                }
                .into()),
            }
        }
    }

    fn fake(reply: fn(u16) -> std::result::Result<Vec<u8>, u16>) -> Arc<Fake> {
        Arc::new(Fake {
            reply,
            seen: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn minion_tile_uses_paired_transport_and_preserves_lan() {
        let f = fake(|_| Ok(br#"{"tile":true,"tile_busy":true,"tile_failed":false}"#.to_vec()));
        let _g = mgmt::scoped_transport(f.clone());
        let state = minion_tile("h", Some(true)).unwrap();
        assert_eq!(state["supported"], true);
        assert_eq!(state["tile_busy"], true);
        assert_eq!(f.seen.lock().unwrap()[0], (152, r#"{"tile":true}"#.into()));
    }

    #[test]
    fn minion_tile_detects_older_helpers_and_save_failures() {
        let _g = mgmt::scoped_transport(fake(|_| Err(ERR_UNKNOWN_METHOD)));
        assert_eq!(minion_tile("h", None).unwrap()["supported"], false);
        drop(_g);
        let _g =
            mgmt::scoped_transport(fake(
                |_| Ok(br#"{"ok":false,"err":"save_failed"}"#.to_vec()),
            ));
        assert!(minion_tile("h", Some(false)).is_err());
    }

    #[test]
    fn the_methods_are_the_schema_numbers() {
        assert_eq!(m::FILES_PREFS_GET.id, 148);
        assert_eq!(m::FILES_PREFS_SET.id, 149);
        assert_eq!(m::FILES_HANDOFF_PUT.id, 150);
        assert_eq!(m::FILES_HANDOFF_TAKE.id, 151);
    }

    #[test]
    fn prefs_and_handoffs_round_trip() {
        let f = fake(|id| {
            match id {
            148 | 149 => Ok(br#"{"v":1,"favorites":["/data/homebrew","bad"],"recent":["/mnt/usb0"],"last":"/mnt/usb0"}"#.to_vec()),
            150 => Ok(br#"{"ok":true}"#.to_vec()),
            _ => Ok(br#"{"path":"/data/x","age_ms":1200}"#.to_vec()),
        }
        });
        let _g = mgmt::scoped_transport(f.clone());
        let p = prefs_get("10.0.0.2").unwrap().unwrap();
        assert_eq!(p.favorites, vec!["/data/homebrew"]);
        assert_eq!(p.last.as_deref(), Some("/mnt/usb0"));
        prefs_set("10.0.0.2", PrefsOp::Pin, "/data/homebrew")
            .unwrap()
            .unwrap();
        handoff_put("10.0.0.2", Side::Console, "/data/a b")
            .unwrap()
            .unwrap();
        let h = handoff_take("10.0.0.2", Side::Pc)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            h,
            Handoff {
                path: "/data/x".into(),
                age_ms: 1200
            }
        );
        let seen = f.seen.lock().unwrap().clone();
        assert_eq!(
            seen[1],
            (149, r#"{"op":"pin","path":"/data/homebrew"}"#.into())
        );
        assert_eq!(
            seen[2],
            (150, r#"{"path":"/data/a b","to":"console"}"#.into())
        );
        assert_eq!(seen[3], (151, r#"{"for":"pc"}"#.into()));
    }

    #[test]
    fn nothing_waiting_is_some_none() {
        let _g = mgmt::scoped_transport(fake(|_| Ok(b"{}".to_vec())));
        assert_eq!(handoff_take("h", Side::Console).unwrap(), Some(None));
    }

    #[test]
    fn an_older_helper_is_unsupported_not_an_error() {
        let _g = mgmt::scoped_transport(fake(|_| Err(ERR_UNKNOWN_METHOD)));
        assert_eq!(prefs_get("h").unwrap(), None);
        assert_eq!(prefs_set("h", PrefsOp::Unpin, "/data").unwrap(), None);
        assert_eq!(handoff_put("h", Side::Console, "/data").unwrap(), None);
        assert_eq!(handoff_take("h", Side::Pc).unwrap(), None);
    }

    #[test]
    fn other_refusals_stay_errors() {
        let _g = mgmt::scoped_transport(fake(|_| Err(5)));
        assert!(prefs_get("h").is_err());
        assert!(handoff_put("h", Side::Console, "/data").is_err());
    }
}
