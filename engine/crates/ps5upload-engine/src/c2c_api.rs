//! Console to console, the parts another device sees (#444): the shared send record around each
//! send, `GET /api/transfer/ps5-to-ps5/remote` (the sends on a console that this engine may
//! watch, take over or resume) and `POST /api/transfer/ps5-to-ps5/test` (Test connection).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use ps5upload_ava1::c2c::{self, Lease, RouteMode, RouteOptions};
use ps5upload_ava1::c2c_record::{
    self as record, Class, Keeper, Outcome, RunningHere, Seen, SendRecord, Snapshot,
};
use ps5upload_ava1::upload::UploadFailure;
use serde::Deserialize;

use crate::{console_addr_or_default, json_err, AddrQuery, AppState, Ps5ToPs5Req};

/// `adopt` of `POST /api/transfer/ps5-to-ps5`: `take` or `watch`, only with the send's own id.
pub(crate) fn parse_adopt(adopt: Option<&str>, has_tx_id: bool) -> Result<Option<String>, String> {
    match adopt.map(str::trim).filter(|a| !a.is_empty()) {
        None => Ok(None),
        Some(a @ ("take" | "watch")) if has_tx_id => Ok(Some(a.to_string())),
        Some("take" | "watch") => Err("adopting a send needs its tx_id".into()),
        Some(other) => Err(format!("adopt {other:?} is not take or watch")),
    }
}

/// Runs one send (`run`, given the lease) inside its shared record: claims the lease (a send
/// another device drives is refused unless this is a take-over), writes and refreshes the
/// record while it runs and leaves it as the send ended.
pub(crate) fn with_record<T>(
    req: &Ps5ToPs5Req,
    tx_id: [u8; 16],
    adopt: Option<&str>,
    snapshot: Snapshot,
    run: impl FnOnce(Option<Arc<Lease>>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let tx_hex = ava1::hex::encode(&tx_id);
    let me = record::me();
    if me.is_empty() {
        return run(None); // no identity: the send fails on its own, with its own reason
    }
    if let Some(mode) = adopt {
        adopt_check(req, &tx_hex, &me, mode)?;
    }
    let rec = SendRecord {
        tx_id: tx_hex.clone(),
        from: req.from.clone(),
        from_key: record::console_key(&req.from),
        to: req.to.clone(),
        to_key: record::console_key(&req.to),
        src: req.src.clone(),
        dest: req.dest.clone(),
        route: req.route.clone().unwrap_or_else(|| "auto".into()),
        direct_addr: req.direct_addr.clone().filter(|d| !d.trim().is_empty()),
        owner: me,
        owner_name: record::device_name(),
        ..Default::default()
    };
    let keeper = Keeper::start(rec, adopt.is_some(), snapshot)?;
    if adopt == Some("take") {
        // Both halves end; the send below opens the same job again, from the receiving
        // console's journal, as this device's own.
        c2c::end_both(&req.from, &req.to, tx_id);
        wait_unlisted(&req.from, &req.to, tx_id);
    }
    let _here = RunningHere::new(&tx_hex);
    let r = run(Some(keeper.lease()));
    keeper.finish(match &r {
        Ok(_) => Outcome::Done,
        Err(e) if format!("{e:#}").contains("transfer_cancelled") => Outcome::Paused,
        Err(e) => Outcome::Failed(format!("{e:#}")),
    });
    r
}

fn refusal(reason: &str, detail: String) -> anyhow::Error {
    UploadFailure {
        reason: reason.into(),
        detail,
    }
    .into()
}

/// What adopting needs from the helpers while another device still drives the send: watching
/// reads the sending console's job, taking over ends both halves of another device's job.
fn adopt_check(req: &Ps5ToPs5Req, tx_hex: &str, me: &str, mode: &str) -> anyhow::Result<()> {
    let existing = record::read(&req.to, tx_hex).ok().flatten();
    let record::Claim::Held { by } = record::claim(existing.as_ref(), me, record::now_ms()) else {
        return Ok(());
    };
    let shared = |c: &str| c2c::shared_jobs(c) == Some(true);
    if mode == "watch" {
        if existing.and_then(|r| r.live_route).as_deref() != Some("direct") {
            return Err(refusal(
                "ava1_cannot_watch",
                format!("the send runs through {by}, so it cannot be watched from here; take it over instead"),
            ));
        }
        if !shared(&req.from) {
            return Err(refusal(
                "ava1_shared_jobs_unsupported",
                format!(
                    "watching a send {by} started needs the newer helper on the sending console"
                ),
            ));
        }
    } else if !(shared(&req.from) && shared(&req.to)) {
        return Err(refusal(
            "ava1_shared_jobs_unsupported",
            format!("{by} is still running this send; taking it over needs the newer helper on both consoles (or pause it on {by} first)"),
        ));
    }
    Ok(())
}

/// After ending both halves: the consoles unlist a cancelled job at once but free it a moment
/// later, and an open in that moment is refused as busy.
fn wait_unlisted(from: &str, to: &str, tx_id: [u8; 16]) {
    for _ in 0..20 {
        let listed = |c: &str| matches!(c2c::status_on(c, tx_id), Ok(Some(_)));
        if !listed(from) && !listed(to) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    std::thread::sleep(Duration::from_millis(500));
}

/// `GET /api/transfer/ps5-to-ps5/remote?addr=` — the sends into this console, each classified
/// for this engine: `mine` (running here), `running_elsewhere`, `paused` or `interrupted`.
pub(crate) async fn remote_handler(
    State(state): State<AppState>,
    Query(q): Query<AddrQuery>,
) -> impl IntoResponse {
    let addr = console_addr_or_default(q.addr, &state.default_ps5_addr);
    match tokio::task::spawn_blocking(move || remote_list(&addr)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => json_err(StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn remote_list(addr: &str) -> anyhow::Result<serde_json::Value> {
    let me = record::me();
    let now = record::now_ms();
    let records = record::list(addr)?;
    let receiver_shared = if records.is_empty() {
        None
    } else {
        c2c::shared_jobs(addr)
    };
    let mut senders: Vec<(String, Option<bool>)> = Vec::new();
    let mut out = Vec::new();
    for r in records {
        let Some(tx_id) = parse_tx(&r.tx_id) else {
            continue;
        };
        let mut class = record::classify(&r, &me, now, record::running_here(&r.tx_id));
        // An older helper lists another device's job for that device only: "not listed"
        // then proves nothing.
        let mut confirmed = true;
        if class == Class::Interrupted {
            let seen = Seen::of(&c2c::status_on(addr, tx_id));
            confirmed = match seen {
                Seen::Unknown => false,
                Seen::NotListed => r.owner == me || receiver_shared == Some(true),
                _ => true,
            };
            match record::settle(class, seen) {
                Some(c) => class = c,
                None => {
                    let _ = record::remove(addr, &r.tx_id); // it finished: the record outlived it
                    continue;
                }
            }
        }
        let sender_shared = if matches!(class, Class::RunningElsewhere | Class::Interrupted) {
            match senders.iter().find(|(a, _)| *a == r.from) {
                Some((_, s)) => *s,
                None => {
                    let s = c2c::shared_jobs(&r.from);
                    senders.push((r.from.clone(), s));
                    s
                }
            }
        } else {
            None
        };
        let both_shared = sender_shared == Some(true) && receiver_shared == Some(true);
        let can_watch = class == Class::RunningElsewhere
            && r.live_route.as_deref() == Some("direct")
            && sender_shared == Some(true);
        let can_take_over = match class {
            Class::Mine => false,
            Class::RunningElsewhere => both_shared,
            Class::Paused | Class::Interrupted => true,
        };
        out.push(serde_json::json!({
            "tx_id": r.tx_id,
            "from": r.from,
            "to": r.to,
            "src": r.src,
            "dest": r.dest,
            "route": r.route,
            "direct_addr": r.direct_addr,
            "owner_name": r.owner_name,
            "mine": r.owner == me,
            "class": class,
            "confirmed": confirmed,
            "live_route": r.live_route,
            "bytes_done": r.bytes_done,
            "bytes_total": r.bytes_total,
            "updated_ms": r.updated_ms,
            "error": r.error,
            "can_watch": can_watch,
            "can_take_over": can_take_over,
        }));
    }
    Ok(serde_json::json!({ "sends": out, "shared_jobs": receiver_shared }))
}

fn parse_tx(hex: &str) -> Option<[u8; 16]> {
    ava1::hex::decode(hex)?.try_into().ok()
}

#[derive(Deserialize)]
pub(crate) struct TestReq {
    from: String,
    to: String,
    #[serde(default)]
    route: Option<String>,
    #[serde(default)]
    direct_addr: Option<String>,
    /// Also a real 4 KiB direct send into a probe folder: catches a forward that reaches
    /// another console than the one meant (its key does not match).
    #[serde(default)]
    deep: bool,
}

/// `POST /api/transfer/ps5-to-ps5/test` — can the sending console reach the receiving one the
/// way a send would dial it (and, for Auto or relay, can this computer reach both)?
pub(crate) async fn test_handler(Json(req): Json<TestReq>) -> impl IntoResponse {
    let mode = match RouteMode::parse(req.route.as_deref()) {
        Ok(m) => m,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    match tokio::task::spawn_blocking(move || test_run(&req, mode)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// How long the connect probe waits.
const REACH_MS: u32 = 3000;
/// How long the deep check's send may take.
const DEEP_LIMIT: Duration = Duration::from_secs(20);

fn test_run(req: &TestReq, mode: RouteMode) -> serde_json::Value {
    let dial = req
        .direct_addr
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let mut reachable = false;
    let direct = (mode != RouteMode::RelayOnly).then(|| match c2c::dial_target(&req.to, dial) {
        Err(e) => serde_json::json!({ "target": null, "verdict": "bad_address", "detail": e }),
        Ok((host, port)) => {
            let r = c2c::reach_verdict(&ps5upload_core::diagnostics::net_reach(
                &req.from, &host, port, REACH_MS,
            ));
            reachable = r.verdict == "reachable";
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            v["target"] = format!("{host}:{port}").into();
            v
        }
    });
    let relay = (mode != RouteMode::DirectOnly).then(|| {
        let leg = |c: &str| match ps5upload_ava1::console::require_ava1(c) {
            Ok(()) => serde_json::json!({ "verdict": "ok" }),
            Err(f) => serde_json::json!({ "verdict": f.reason, "detail": f.detail }),
        };
        serde_json::json!({ "from": leg(&req.from), "to": leg(&req.to) })
    });
    let deep = (req.deep && reachable).then(|| deep_probe(&req.from, &req.to, dial));
    serde_json::json!({ "direct": direct, "relay": relay, "deep": deep })
}

/// A real direct send of 4 KiB into `/data/ps5upload/c2c/.probe-<id>` on the receiving console,
/// then both probe files are removed.
fn deep_probe(from: &str, to: &str, dial: Option<&str>) -> serde_json::Value {
    use ps5upload_core::fs_ops::{fs_delete, fs_mkdir, fs_stat};
    let id = *uuid::Uuid::new_v4().as_bytes();
    let hex = ava1::hex::encode(&id);
    let src = format!("{}/.probe-{hex}.src", record::DIR);
    let dest = format!("{}/.probe-{hex}", record::DIR);
    let body: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 251) as u8).collect();
    let started = std::time::Instant::now();
    let staged = fs_mkdir(from, record::DIR)
        .and_then(|_| ps5upload_core::diagnostics::fs_write_bytes(from, &src, &body, false))
        .and_then(|_| fs_mkdir(to, record::DIR));
    let result = match staged {
        Err(e) => Err(format!("{e:#}")),
        Ok(_) => {
            let cancel = Arc::new(AtomicBool::new(false));
            let done = Arc::new(AtomicBool::new(false));
            {
                let (cancel, done) = (cancel.clone(), done.clone());
                std::thread::spawn(move || {
                    let until = std::time::Instant::now() + DEEP_LIMIT;
                    while std::time::Instant::now() < until && !done.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    cancel.store(true, Ordering::Relaxed);
                });
            }
            let options = RouteOptions {
                mode: RouteMode::DirectOnly,
                dial: dial.map(str::to_string),
                ..Default::default()
            };
            let r = c2c::ps5_to_ps5_routed(
                from,
                &src,
                to,
                &dest,
                id,
                Arc::default(),
                cancel.clone(),
                None,
                &options,
                |_| {},
            );
            done.store(true, Ordering::Relaxed);
            match r {
                Ok(_) => match fs_stat(to, &dest) {
                    Ok(st) if st.size == body.len() as u64 => Ok(()),
                    Ok(st) => Err(format!("{} bytes arrived of {}", st.size, body.len())),
                    Err(e) => Err(format!("{e:#}")),
                },
                Err(_) if cancel.load(Ordering::Relaxed) => Err(format!(
                    "the send did not finish within {} s",
                    DEEP_LIMIT.as_secs()
                )),
                Err(e) => Err(format!("{e:#}")),
            }
        }
    };
    let _ = fs_delete(from, &src);
    let _ = fs_delete(to, &dest);
    match result {
        Ok(()) => serde_json::json!({ "ok": true, "ms": started.elapsed().as_millis() as u64 }),
        Err(detail) => serde_json::json!({ "ok": false, "detail": detail }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopting_needs_take_or_watch_and_the_sends_own_id() {
        assert_eq!(parse_adopt(None, false), Ok(None));
        assert_eq!(parse_adopt(Some(" "), false), Ok(None));
        assert_eq!(parse_adopt(Some("take"), true), Ok(Some("take".into())));
        assert_eq!(parse_adopt(Some(" watch "), true), Ok(Some("watch".into())));
        assert!(parse_adopt(Some("take"), false)
            .unwrap_err()
            .contains("tx_id"));
        assert!(parse_adopt(Some("grab"), true)
            .unwrap_err()
            .contains("take or watch"));
        assert_eq!(parse_tx(&"ab".repeat(16)), Some([0xab; 16]));
        assert_eq!(parse_tx("abcd"), None);
        assert_eq!(parse_tx(&"zz".repeat(16)), None);
    }

    fn state() -> AppState {
        let (events_tx, _rx) = tokio::sync::broadcast::channel(16);
        AppState {
            jobs: Arc::default(),
            default_ps5_addr: "127.0.0.1".to_string(),
            events_tx,
        }
    }

    fn send_req(adopt: Option<&str>, tx_id: Option<&str>) -> Ps5ToPs5Req {
        serde_json::from_value(serde_json::json!({
            "from": "192.0.2.1", "src": "/data/a", "to": "192.0.2.2", "dest": "/data/b",
            "tx_id": tx_id, "adopt": adopt,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_bad_adopt_or_route_is_refused_before_anything_runs() {
        let tx = "ab".repeat(16);
        for req in [
            send_req(Some("grab"), Some(&tx)),
            send_req(Some("take"), None),
        ] {
            let r = crate::ps5_to_ps5_handler(State(state()), Json(req))
                .await
                .into_response();
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        }
        let req: TestReq = serde_json::from_value(serde_json::json!({
            "from": "192.0.2.1", "to": "192.0.2.2", "route": "sideways",
        }))
        .unwrap();
        assert!(!req.deep, "the deep check is asked for, never assumed");
        let r = test_handler(Json(req)).await.into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}
