//! Console to console (#433, SPEC.md §18): straight between the two consoles when they can
//! reach each other, else through this computer (the relay). The receiving console hands out a
//! ticket for the sending console's key (c2c.allow); the sending console dials it with that
//! ticket and pushes the job (c2c.send). The job on the receiving console is the engine's own,
//! with the same id as a relay would use, so either route resumes what the other left.
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ava1::gen::{self, C2cAllow, C2cSend, C2cTicket, JobRef, Status};
use ava1::send::{Progress, SendReport};
use ava1::wire::Message;

use crate::pool::{pool, Pool};

/// How a console-to-console job travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Straight between the consoles.
    Direct,
    /// Through this computer, and why not straight.
    Relay { reason: String },
}

/// The sending console's dial takes a TCP connect and a handshake per connection, and it walks
/// the source before it answers.
const SEND_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(500);
/// No byte acknowledged for this long ends the direct send; the relay picks it up.
const DIRECT_STALL: Duration = Duration::from_secs(120);
/// Status calls that keep failing for this long end it the same way.
const STATUS_GIVE_UP: Duration = Duration::from_secs(30);
/// The data lanes the sending console opens to the other.
const LANES: u8 = 4;

static TEST_PORT: AtomicU16 = AtomicU16::new(0);

/// Test knob: the sending console is told this port for the other console (a closed one
/// makes the direct route fail, so the fallback runs). 0 clears it.
#[doc(hidden)]
pub fn set_dial_port_for_tests(port: u16) {
    TEST_PORT.store(port, Ordering::Relaxed);
}

enum Direct {
    Done(SendReport),
    Fallback(String),
    Cancelled,
    /// The job failed for a reason the relay would meet too (no space, a bad path).
    Failed(String),
}

/// The port a console dials for a direct send when the address names none: the helper's.
pub const DIRECT_PORT: u16 = gen::DEFAULT_PORT;

/// How a console-to-console copy may go (#444).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteOptions {
    /// `Auto`: straight between the consoles when they can connect, else through this
    /// computer. `DirectOnly`: never through this computer (a phone on mobile data, say); a
    /// direct send that cannot run fails with its reason. `RelayOnly`: never direct.
    pub mode: RouteMode,
    /// What the sending console dials to reach the receiving one, `host:port` or `host`
    /// (port 9120), in place of the receiving console's own address. For a console whose
    /// outgoing connections go through a local forward (ps5-tailscale: `127.0.0.1:19120`
    /// forwarded to the other console), or any setup where the address this computer uses is
    /// not the one the consoles use between themselves. The ticket and the peer key still
    /// decide who may send, not the address.
    pub dial: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RouteMode {
    #[default]
    Auto,
    DirectOnly,
    RelayOnly,
}

impl RouteMode {
    /// `auto` (or nothing), `direct`, `relay`.
    pub fn parse(s: Option<&str>) -> std::result::Result<Self, String> {
        match s.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("auto") => Ok(Self::Auto),
            Some("direct") => Ok(Self::DirectOnly),
            Some("relay") => Ok(Self::RelayOnly),
            Some(other) => Err(format!("route {other:?} is not auto, direct or relay")),
        }
    }
}

/// `host:port` or `host` (port [`DIRECT_PORT`]) as the sending console will dial it: an IPv4
/// address, since that is what the helper's C2C_SEND carries.
pub fn parse_dial(s: &str) -> std::result::Result<(String, u16), String> {
    let s = s.trim();
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (
            h,
            p.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("port {p:?} is not 1 to 65535"))?,
        ),
        _ => (s, DIRECT_PORT),
    };
    if host.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(format!(
            "{host:?} is not an IPv4 address (the console dials by address)"
        ));
    }
    Ok((host.to_string(), port))
}

/// Copies `src` on `from` to `dest` on `to`: directly when it can, else through this computer.
/// `on_route` hears the route as soon as it is known (and again if it changes).
/// `bandwidth_cap` (bytes per second) paces the relay, which sends from this computer. The
/// direct route is not paced: its bytes never cross this computer, and the console-to-console
/// protocol carries no rate for the sending console to honour.
#[allow(clippy::too_many_arguments)]
pub fn ps5_to_ps5_routed(
    from: &str,
    src: &str,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    bandwidth_cap: Option<u64>,
    options: &RouteOptions,
    on_route: impl Fn(&Route),
) -> Result<(SendReport, Route)> {
    routed(
        pool(),
        from,
        src,
        pool(),
        to,
        dest,
        job_id,
        progress,
        cancel,
        bandwidth_cap,
        options,
        on_route,
    )
}

/// Pool injection for local two-console tests; the same body as production.
#[allow(clippy::too_many_arguments)]
pub fn ps5_to_ps5_routed_between(
    from_pool: &Pool,
    from: &str,
    src: &str,
    to_pool: &Pool,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    on_route: impl Fn(&Route),
) -> Result<(SendReport, Route)> {
    routed(
        from_pool,
        from,
        src,
        to_pool,
        to,
        dest,
        job_id,
        progress,
        cancel,
        None,
        &RouteOptions::default(),
        on_route,
    )
}

#[allow(clippy::too_many_arguments)]
fn routed(
    from_pool: &Pool,
    from: &str,
    src: &str,
    to_pool: &Pool,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    bandwidth_cap: Option<u64>,
    options: &RouteOptions,
    on_route: impl Fn(&Route),
) -> Result<(SendReport, Route)> {
    let dial = match options.dial.as_deref() {
        Some(d) => Some(parse_dial(d).map_err(|e| anyhow!("the direct route address: {e}"))?),
        None => None,
    };
    let reason = if options.mode == RouteMode::RelayOnly {
        "a send through this computer was asked for".to_string()
    } else {
        let direct = crate::block_on(direct(
            from_pool, from, src, to_pool, to, dest, job_id, &progress, &cancel, dial, &on_route,
        ));
        match direct {
            Direct::Done(r) => return Ok((r, Route::Direct)),
            Direct::Cancelled => return Err(anyhow!("transfer_cancelled")),
            Direct::Failed(why) => return Err(anyhow!("{why}")),
            Direct::Fallback(reason) if options.mode == RouteMode::DirectOnly => {
                // Nothing through this computer: the receiving console's half is still
                // ended, so the next try (or a relay, if asked for) is not refused as busy.
                crate::block_on(cancel_on(to_pool, to, job_id));
                return Err(anyhow!("direct_route_unavailable: {reason}"));
            }
            Direct::Fallback(reason) => reason,
        }
    };
    // The receiving console's half of the direct job may still be open (the sending console
    // that would have cancelled it may be the one that stopped answering); while it is, the
    // relay's open is refused as busy. The engine owns that job, so it ends it itself; its
    // journal stays, so the relay resumes what the direct send wrote.
    crate::block_on(cancel_on(to_pool, to, job_id));
    let route = Route::Relay {
        reason: reason.clone(),
    };
    on_route(&route);
    let r = crate::relay::ps5_to_ps5_capped(
        from_pool,
        from,
        src,
        to_pool,
        to,
        dest,
        job_id,
        progress,
        cancel,
        bandwidth_cap,
    )
    .map_err(|e| with_route_reason(e, &reason))?;
    Ok((r, route))
}

/// A relay refused as busy after a direct send was given up says why the direct send ended:
/// the two are usually the same story (the direct job still holding the destination).
fn with_route_reason(mut e: anyhow::Error, reason: &str) -> anyhow::Error {
    if let Some(f) = e.downcast_mut::<crate::upload::UploadFailure>() {
        if f.reason == "ava1_busy" {
            f.detail
                .push_str(&format!(" (after the direct route was given up: {reason})"));
        }
    }
    e
}

/// Why a direct send that has stopped reporting progress is given up, if it is: status calls
/// failing (or unreadable) for [`STATUS_GIVE_UP`], or no byte acknowledged for
/// [`DIRECT_STALL`].
fn gave_up(status_failing_for: Duration, still_for: Duration) -> Option<&'static str> {
    if status_failing_for > STATUS_GIVE_UP {
        Some("the sending console stopped answering")
    } else if still_for > DIRECT_STALL {
        Some("the direct send stopped moving")
    } else {
        None
    }
}

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).trim().to_string()
}

/// The first IPv4 address of `addr` (`host:port`), as the sending console must dial it.
fn ipv4_of(addr: &str) -> Option<(String, u16)> {
    addr.to_socket_addrs()
        .ok()?
        .find(|a| a.is_ipv4())
        .map(|a| (a.ip().to_string(), a.port()))
}

/// Codes a relay would meet as well: the job ends instead of falling back.
fn terminal(code: u16) -> bool {
    matches!(
        code,
        gen::ERR_NO_SPACE | gen::ERR_PATH | gen::ERR_EXISTS | gen::ERR_CROSS_DEVICE
    )
}

/// Best effort: stops a console's half of the direct job (on the sending console its watcher
/// then cancels it on the other console too). A job that is gone already is fine.
async fn cancel_on(pool: &Pool, console: &str, job_id: [u8; 16]) {
    if let (Ok(s), Ok(body)) = (pool.session(console).await, JobRef { job_id }.to_bytes()) {
        let _ = s.rpc(gen::METHOD_JOB_CANCEL, &body).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn direct(
    from_pool: &Pool,
    from: &str,
    src: &str,
    to_pool: &Pool,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: &Progress,
    cancel: &AtomicBool,
    dial: Option<(String, u16)>,
    on_route: &impl Fn(&Route),
) -> Direct {
    let sa = match from_pool.session(from).await {
        Ok(s) => s,
        Err(e) => return Direct::Fallback(format!("the sending console did not answer: {e}")),
    };
    let sb = match to_pool.session(to).await {
        Ok(s) => s,
        Err(e) => return Direct::Fallback(format!("the receiving console did not answer: {e}")),
    };
    let allow = C2cAllow {
        job_id,
        key: sa.peer_key(),
        root: dest.into(),
    };
    let Ok(body) = allow.to_bytes() else {
        return Direct::Fallback("the destination path is too long for a direct send".into());
    };
    let token = match sb.rpc(gen::METHOD_C2C_ALLOW, &body).await {
        Ok(r) if r.status == gen::STATUS_OK => match C2cTicket::decode(&r.body) {
            Ok(t) => t.token,
            Err(e) => return Direct::Fallback(format!("the receiving console's ticket: {e}")),
        },
        Ok(r) if r.status == gen::ERR_UNKNOWN_METHOD => {
            return Direct::Fallback("the receiving console's helper is older than 6.8".into())
        }
        Ok(r) => {
            return Direct::Fallback(format!(
                "the receiving console refused a direct send: {}",
                text(&r.body)
            ))
        }
        Err(e) => return Direct::Fallback(format!("the receiving console did not answer: {e}")),
    };
    let (host, mut port) = match dial {
        Some(d) => d,
        None => match ipv4_of(&to_pool.addr_for(to)) {
            Some(a) => a,
            None => {
                return Direct::Fallback("the receiving console has no IPv4 address to dial".into())
            }
        },
    };
    let test_port = TEST_PORT.load(Ordering::Relaxed);
    if test_port != 0 {
        port = test_port;
    }
    let send = C2cSend {
        job_id,
        host: host.clone(),
        port,
        key: sb.peer_key(),
        token,
        src: src.into(),
        dest: dest.into(),
        flags: 0,
    };
    let Ok(body) = send.to_bytes() else {
        return Direct::Fallback("the paths are too long for a direct send".into());
    };
    match sa
        .rpc_within(gen::METHOD_C2C_SEND, &body, SEND_TIMEOUT)
        .await
    {
        Ok(r) if r.status == gen::STATUS_OK => {}
        Ok(r) if r.status == gen::ERR_UNKNOWN_METHOD => {
            return Direct::Fallback("the sending console's helper is older than 6.8".into())
        }
        Ok(r) if terminal(r.status) => return Direct::Failed(text(&r.body)),
        Ok(r) => {
            return Direct::Fallback(format!(
                "the consoles could not connect to each other ({})",
                text(&r.body)
            ))
        }
        Err(e) => {
            // It may have started anyway: the relay must not run beside it.
            cancel_on(from_pool, from, job_id).await;
            return Direct::Fallback(format!("the sending console did not answer: {e}"));
        }
    }
    on_route(&Route::Direct);
    let reference = JobRef { job_id }.to_bytes().unwrap_or_default();
    let (mut last_bytes, mut last_move) = (0u64, Instant::now());
    let mut status_ok_at = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancel_on(from_pool, from, job_id).await;
            return Direct::Cancelled;
        }
        tokio::time::sleep(POLL).await;
        let reply = match from_pool.session(from).await {
            Ok(s) => s.rpc(gen::METHOD_JOB_STATUS, &reference).await,
            Err(e) => Err(e),
        };
        let st = match reply {
            // An unreadable status counts as no answer: it must not skip the give-up checks.
            Ok(r) if r.status == gen::STATUS_OK => Status::decode(&r.body).ok(),
            Ok(r) if r.status == gen::ERR_UNKNOWN_JOB => {
                return Direct::Fallback("the sending console lost the direct send".into())
            }
            _ => None,
        };
        let Some(st) = st else {
            if let Some(why) = gave_up(status_ok_at.elapsed(), last_move.elapsed()) {
                cancel_on(from_pool, from, job_id).await;
                return Direct::Fallback(why.into());
            }
            continue;
        };
        status_ok_at = Instant::now();
        progress
            .bytes_total
            .store(st.bytes_total, Ordering::Relaxed);
        progress
            .files_total
            .store(st.files_total as u64, Ordering::Relaxed);
        progress
            .bytes_sent
            .store(st.bytes_received.min(st.bytes_total), Ordering::Relaxed);
        progress.lanes.store(LANES, Ordering::Relaxed);
        if st.bytes_received != last_bytes {
            last_bytes = st.bytes_received;
            last_move = Instant::now();
        }
        match st.state {
            Some(1) => {
                progress.bytes_sent.store(st.bytes_total, Ordering::Relaxed);
                progress
                    .bytes_durable
                    .store(st.bytes_durable.max(st.bytes_total), Ordering::Relaxed);
                progress
                    .files_durable
                    .store(st.files_done as u64, Ordering::Relaxed);
                return Direct::Done(SendReport {
                    status: gen::STATUS_OK,
                    message: None,
                    files: st.files_done,
                    bytes: st.bytes_total,
                    resent: 0,
                    max_lanes: LANES,
                    bottleneck: 0,
                    sequential: false,
                });
            }
            Some(0) | None => {}
            Some(_) => {
                let why = st
                    .current
                    .unwrap_or_else(|| "the direct send failed".into());
                return match st.code {
                    Some(c) if c == gen::ERR_CANCELLED && cancel.load(Ordering::Relaxed) => {
                        Direct::Cancelled
                    }
                    Some(c) if terminal(c) => Direct::Failed(why),
                    _ => Direct::Fallback(format!("the direct send stopped: {why}")),
                };
            }
        }
        if let Some(why) = gave_up(Duration::ZERO, last_move.elapsed()) {
            cancel_on(from_pool, from, job_id).await;
            return Direct::Fallback(why.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_send_is_given_up_when_status_fails_or_nothing_moves() {
        let s = Duration::from_secs(1);
        assert_eq!(gave_up(s, s), None);
        assert_eq!(
            gave_up(STATUS_GIVE_UP + s, s),
            Some("the sending console stopped answering")
        );
        // Unreadable statuses forever, while nothing moves: still given up.
        assert_eq!(
            gave_up(s, DIRECT_STALL + s),
            Some("the direct send stopped moving")
        );
    }

    #[test]
    fn a_busy_relay_after_a_fallback_says_why_the_direct_route_ended() {
        let busy =
            crate::upload::busy_failure(3, "another transfer is writing to this destination");
        let e = with_route_reason(busy, "the sending console stopped answering");
        let f = e.downcast_ref::<crate::upload::UploadFailure>().unwrap();
        assert_eq!(f.reason, "ava1_busy");
        assert!(f.detail.contains("another transfer is writing"));
        assert!(f.detail.contains("the sending console stopped answering"));
        // Other failures are left as they are.
        let e = with_route_reason(anyhow!("disk full"), "x");
        assert_eq!(e.to_string(), "disk full");
    }
}
