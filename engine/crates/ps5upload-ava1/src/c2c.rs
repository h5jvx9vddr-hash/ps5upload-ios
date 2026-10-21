//! Console to console (#433, SPEC.md §18): straight between the two consoles when they can
//! reach each other, else through this computer (the relay). The receiving console hands out a
//! ticket for the sending console's key (c2c.allow); the sending console dials it with that
//! ticket and pushes the job (c2c.send). The job on the receiving console is the engine's own,
//! with the same id as a relay would use, so either route resumes what the other left.
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Another device took the send over (#444): stop driving it, end nothing.
    Released(String),
    /// The job failed for a reason the relay would meet too (no space, a bad path).
    Failed(String),
}

/// The port a console dials for a direct send when the address names none: the helper's.
pub const DIRECT_PORT: u16 = gen::DEFAULT_PORT;

/// Who drives a send (#444). The engine that started it holds a lease in the shared record on
/// the receiving console; another device that takes the send over moves the lease to itself,
/// and this engine then stops driving the send without ending either console's half.
#[derive(Default)]
pub struct Lease {
    taken_by: Mutex<Option<String>>,
    /// A fresh read of the record: the device that holds the lease now when it is not this
    /// engine. It blocks (a management call), so it runs only outside async code.
    check: Option<Box<dyn Fn() -> Option<String> + Send + Sync>>,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("taken_by", &self.taken_by())
            .finish()
    }
}

impl Lease {
    pub fn new(check: impl Fn() -> Option<String> + Send + Sync + 'static) -> Lease {
        Lease {
            taken_by: Mutex::default(),
            check: Some(Box::new(check)),
        }
    }

    /// The device that took the send over, as last seen (no I/O).
    pub fn taken_by(&self) -> Option<String> {
        self.taken_by
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn mark_taken(&self, by: String) {
        *self.taken_by.lock().unwrap_or_else(|e| e.into_inner()) = Some(by);
    }

    /// Looks again before anything that would end a console's half or start the relay.
    fn confirm(&self) -> Option<String> {
        if let Some(by) = self.taken_by() {
            return Some(by);
        }
        let by = self.check.as_ref().and_then(|c| c())?;
        self.mark_taken(by.clone());
        Some(by)
    }
}

/// Why this engine stopped: another device drives the send now. Not a failure to retry.
pub fn taken_over(by: &str) -> anyhow::Error {
    crate::upload::UploadFailure {
        reason: "ava1_taken_over".into(),
        detail: format!("taken over by {by}"),
    }
    .into()
}

/// How a console-to-console copy may go (#444).
#[derive(Debug, Clone, Default)]
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
    /// Watch a direct send another device started (#444): no new send, this engine follows
    /// the sending console's job (and falls back as usual if it stops).
    pub attach: bool,
    /// The shared record's lease; `None` runs without one (tests, the lab).
    pub lease: Option<Arc<Lease>>,
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

/// What the sending console dials to reach `to`, as a send would: `dial` when given, else
/// `to`'s own IPv4 address on the helper's port.
pub fn dial_target(to: &str, dial: Option<&str>) -> std::result::Result<(String, u16), String> {
    let (host, mut port) = match dial {
        Some(d) => parse_dial(d).map_err(|e| format!("the direct route address: {e}"))?,
        None => ipv4_of(&pool().addr_for(to))
            .ok_or_else(|| "the receiving console has no IPv4 address to dial".to_string())?,
    };
    let test_port = TEST_PORT.load(Ordering::Relaxed);
    if test_port != 0 {
        port = test_port;
    }
    Ok((host, port))
}

/// Ends both consoles' halves of a job (best effort): a send another device takes over is
/// then free to resume from that device (#444).
pub fn end_both(from: &str, to: &str, job_id: [u8; 16]) {
    crate::block_on(async {
        cancel_on(pool(), from, job_id).await;
        cancel_on(pool(), to, job_id).await;
    })
}

/// Whether a console's helper lets any paired device read and cancel a transfer and take over
/// a parked one (`CAP_SHARED_JOBS`); `None` when the console cannot be asked.
pub fn shared_jobs(console: &str) -> Option<bool> {
    crate::block_on(async {
        let s = pool().session(console).await.ok()?;
        Some(s.peer_caps() & gen::CAP_SHARED_JOBS != 0)
    })
}

/// A console's half of a job as this engine may read it: `Ok(None)` when the console does not
/// list it (for this engine; an older helper lists another device's job for that device only).
pub fn status_on(console: &str, job_id: [u8; 16]) -> Result<Option<Status>> {
    crate::block_on(async {
        let s = pool().session(console).await?;
        let r = s
            .rpc(gen::METHOD_JOB_STATUS, &JobRef { job_id }.to_bytes()?)
            .await?;
        Ok(match r.status {
            gen::STATUS_OK => Some(Status::decode(&r.body)?),
            _ => None,
        })
    })
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
    ps5_to_ps5_routed_between_with(
        from_pool,
        from,
        src,
        to_pool,
        to,
        dest,
        job_id,
        progress,
        cancel,
        &RouteOptions::default(),
        on_route,
    )
}

/// [`ps5_to_ps5_routed_between`] with route options (a lease, watching).
#[allow(clippy::too_many_arguments)]
pub fn ps5_to_ps5_routed_between_with(
    from_pool: &Pool,
    from: &str,
    src: &str,
    to_pool: &Pool,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    options: &RouteOptions,
    on_route: impl Fn(&Route),
) -> Result<(SendReport, Route)> {
    routed(
        from_pool, from, src, to_pool, to, dest, job_id, progress, cancel, None, options, on_route,
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
    let lease = options.lease.as_deref();
    let taken = || lease.and_then(Lease::confirm);
    let reason = if options.mode == RouteMode::RelayOnly {
        "a send through this computer was asked for".to_string()
    } else {
        let direct = crate::block_on(direct(
            from_pool,
            from,
            src,
            to_pool,
            to,
            dest,
            job_id,
            &progress,
            &cancel,
            dial,
            options.attach,
            lease,
            &on_route,
        ));
        match direct {
            Direct::Done(r) => return Ok((r, Route::Direct)),
            Direct::Released(by) => return Err(taken_over(&by)),
            Direct::Cancelled => {
                // A pause ends the receiving console's half too, also when the consoles have
                // lost each other and the sending console's own cancel never reaches it: its
                // journal is then free at once for a resume from this or any device (#444).
                crate::block_on(cancel_on(to_pool, to, job_id));
                return Err(anyhow!("transfer_cancelled"));
            }
            Direct::Failed(why) => return Err(anyhow!("{why}")),
            Direct::Fallback(reason) => {
                // Another device's send now: neither half is this engine's to end or relay.
                if let Some(by) = taken() {
                    return Err(taken_over(&by));
                }
                if options.mode == RouteMode::DirectOnly {
                    // Nothing through this computer: the receiving console's half is still
                    // ended, so the next try (or a relay, if asked for) is not refused as busy.
                    crate::block_on(cancel_on(to_pool, to, job_id));
                    return Err(anyhow!("direct_route_unavailable: {reason}"));
                }
                reason
            }
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
        cancel.clone(),
        bandwidth_cap,
    );
    match r {
        Ok(r) => Ok((r, route)),
        Err(e) if cancel.load(Ordering::Relaxed) => {
            // Paused: both halves end, so any device can resume it at once (#444).
            crate::block_on(async {
                cancel_on(from_pool, from, job_id).await;
                cancel_on(to_pool, to, job_id).await;
            });
            Err(e)
        }
        // The receiving console ended the relay's job because another device took it over.
        Err(e) => match taken() {
            Some(by) => Err(taken_over(&by)),
            None => Err(with_route_reason(e, &reason)),
        },
    }
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

/// Ends the sending console's half before a fallback, unless another device drives it now.
async fn cancel_unless_taken(pool: &Pool, console: &str, job_id: [u8; 16], lease: Option<&Lease>) {
    if lease.and_then(Lease::taken_by).is_none() {
        cancel_on(pool, console, job_id).await;
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
    attach: bool,
    lease: Option<&Lease>,
    on_route: &impl Fn(&Route),
) -> Direct {
    if !attach {
        if let Err(d) =
            start_direct(from_pool, from, src, to_pool, to, dest, job_id, dial, lease).await
        {
            return d;
        }
    }
    on_route(&Route::Direct);
    follow(from_pool, from, job_id, progress, cancel, lease).await
}

/// c2c.allow on the receiving console, then c2c.send on the sending one. `Err` is how the
/// direct route ended before it ran.
#[allow(clippy::too_many_arguments)]
async fn start_direct(
    from_pool: &Pool,
    from: &str,
    src: &str,
    to_pool: &Pool,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    dial: Option<(String, u16)>,
    lease: Option<&Lease>,
) -> std::result::Result<(), Direct> {
    let sa = from_pool
        .session(from)
        .await
        .map_err(|e| Direct::Fallback(format!("the sending console did not answer: {e}")))?;
    let sb = to_pool
        .session(to)
        .await
        .map_err(|e| Direct::Fallback(format!("the receiving console did not answer: {e}")))?;
    let allow = C2cAllow {
        job_id,
        key: sa.peer_key(),
        root: dest.into(),
    };
    let Ok(body) = allow.to_bytes() else {
        return Err(Direct::Fallback(
            "the destination path is too long for a direct send".into(),
        ));
    };
    let token = match sb.rpc(gen::METHOD_C2C_ALLOW, &body).await {
        Ok(r) if r.status == gen::STATUS_OK => match C2cTicket::decode(&r.body) {
            Ok(t) => t.token,
            Err(e) => {
                return Err(Direct::Fallback(format!(
                    "the receiving console's ticket: {e}"
                )))
            }
        },
        Ok(r) if r.status == gen::ERR_UNKNOWN_METHOD => {
            return Err(Direct::Fallback(
                "the receiving console's helper is older than 6.8".into(),
            ))
        }
        Ok(r) => {
            return Err(Direct::Fallback(format!(
                "the receiving console refused a direct send: {}",
                text(&r.body)
            )))
        }
        Err(e) => {
            return Err(Direct::Fallback(format!(
                "the receiving console did not answer: {e}"
            )))
        }
    };
    let (host, mut port) = match dial {
        Some(d) => d,
        None => match ipv4_of(&to_pool.addr_for(to)) {
            Some(a) => a,
            None => {
                return Err(Direct::Fallback(
                    "the receiving console has no IPv4 address to dial".into(),
                ))
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
        return Err(Direct::Fallback(
            "the paths are too long for a direct send".into(),
        ));
    };
    match sa
        .rpc_within(gen::METHOD_C2C_SEND, &body, SEND_TIMEOUT)
        .await
    {
        Ok(r) if r.status == gen::STATUS_OK => Ok(()),
        Ok(r) if r.status == gen::ERR_UNKNOWN_METHOD => Err(Direct::Fallback(
            "the sending console's helper is older than 6.8".into(),
        )),
        Ok(r) if terminal(r.status) => Err(Direct::Failed(text(&r.body))),
        Ok(r) => Err(Direct::Fallback(format!(
            "the consoles could not connect to each other ({})",
            text(&r.body)
        ))),
        Err(e) => {
            // It may have started anyway: the relay must not run beside it.
            cancel_unless_taken(from_pool, from, job_id, lease).await;
            Err(Direct::Fallback(format!(
                "the sending console did not answer: {e}"
            )))
        }
    }
}

/// Follows the sending console's job until it ends, mirroring its progress.
async fn follow(
    from_pool: &Pool,
    from: &str,
    job_id: [u8; 16],
    progress: &Progress,
    cancel: &AtomicBool,
    lease: Option<&Lease>,
) -> Direct {
    let reference = JobRef { job_id }.to_bytes().unwrap_or_default();
    let (mut last_bytes, mut last_move) = (0u64, Instant::now());
    let mut status_ok_at = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancel_on(from_pool, from, job_id).await;
            return Direct::Cancelled;
        }
        if let Some(by) = lease.and_then(Lease::taken_by) {
            return Direct::Released(by);
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
                cancel_unless_taken(from_pool, from, job_id, lease).await;
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
            cancel_unless_taken(from_pool, from, job_id, lease).await;
            return Direct::Fallback(why.into());
        }
    }
}

/// What "Test connection" says about one connect from the sending console (#444): `reachable`,
/// `refused` (the address answers, nothing listens on the port), `timeout`, `unreachable` (no
/// route), `bad_address`, `helper_too_old` (no net.reach) or `error` (the probe could not run).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Reach {
    pub verdict: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The console's `net.reach` answer as a [`Reach`]. errno values are FreeBSD's (the console)
/// and Linux's (the host-built helper in tests).
pub fn reach_verdict(r: &Result<ps5upload_core::diagnostics::NetReach>) -> Reach {
    let v = |verdict, ms, detail| Reach {
        verdict,
        ms,
        detail,
    };
    match r {
        Ok(n) if n.ok => v("reachable", Some(n.ms), None),
        Ok(n) if n.timed_out => v("timeout", Some(n.ms), None),
        Ok(n) => {
            let err = n.err.to_ascii_lowercase();
            let verdict = match n.errno {
                61 | 111 => "refused",
                51 | 65 | 64 | 101 | 113 | 112 => "unreachable",
                60 | 110 => "timeout",
                _ if err.contains("refused") => "refused",
                _ if err.contains("unreachable") || err.contains("no route") => "unreachable",
                _ => "error",
            };
            v(
                verdict,
                Some(n.ms),
                Some(n.err.clone()).filter(|e| !e.is_empty()),
            )
        }
        Err(e) => {
            let unknown_method = e
                .downcast_ref::<ps5upload_core::mgmt::MgmtError>()
                .is_some_and(|m| m.status == gen::ERR_UNKNOWN_METHOD);
            let text = format!("{e:#}");
            if text.contains("bad_address") {
                v("bad_address", None, None)
            } else if unknown_method || text.contains(ps5upload_core::mgmt::HELPER_NOT_AVA1) {
                v("helper_too_old", None, Some(text))
            } else {
                v("error", None, Some(text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reach_answers_read_as_one_word_verdicts() {
        use ps5upload_core::diagnostics::NetReach;
        let n = |ok, timed_out, errno, err: &str| {
            Ok(NetReach {
                ok,
                timed_out,
                errno,
                err: err.into(),
                ms: 7,
            })
        };
        let verdict = |r: Result<NetReach>| reach_verdict(&r).verdict;
        assert_eq!(reach_verdict(&n(true, false, 0, "")).ms, Some(7));
        assert_eq!(verdict(n(true, false, 0, "")), "reachable");
        assert_eq!(verdict(n(false, true, 0, "timed out")), "timeout");
        assert_eq!(
            verdict(n(false, false, 61, "Connection refused")),
            "refused"
        );
        assert_eq!(
            verdict(n(false, false, 111, "Connection refused")),
            "refused"
        );
        assert_eq!(
            verdict(n(false, false, 65, "No route to host")),
            "unreachable"
        );
        assert_eq!(
            verdict(n(false, false, 101, "Network is unreachable")),
            "unreachable"
        );
        assert_eq!(
            verdict(n(false, false, 999, "No route to host")),
            "unreachable"
        );
        assert_eq!(verdict(n(false, false, 999, "Something else")), "error");
        let bad: anyhow::Error = ps5upload_core::mgmt::MgmtError {
            label: "NET_REACH".into(),
            status: gen::ERR_PROTOCOL,
            cause: "bad_address".into(),
        }
        .into();
        assert_eq!(verdict(Err(bad)), "bad_address");
        let old: anyhow::Error = ps5upload_core::mgmt::MgmtError {
            label: "NET_REACH".into(),
            status: gen::ERR_UNKNOWN_METHOD,
            cause: "unknown method".into(),
        }
        .into();
        assert_eq!(verdict(Err(old)), "helper_too_old");
        assert_eq!(
            verdict(Err(ps5upload_core::mgmt::helper_not_ava1("NET_REACH"))),
            "helper_too_old"
        );
        assert_eq!(verdict(Err(anyhow!("connection reset"))), "error");
    }

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

    #[test]
    fn a_lease_remembers_who_took_the_send_over_and_asks_only_until_it_knows() {
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = asked.clone();
        let lease = Lease::new(move || {
            n.fetch_add(1, Ordering::Relaxed);
            Some("Phone".to_string())
        });
        assert_eq!(lease.taken_by(), None, "no I/O until confirm");
        assert_eq!(lease.confirm().as_deref(), Some("Phone"));
        assert_eq!(lease.confirm().as_deref(), Some("Phone"));
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        assert_eq!(lease.taken_by().as_deref(), Some("Phone"));
        let mine = Lease::new(|| None);
        assert_eq!(mine.confirm(), None);
        let e = taken_over("Phone");
        let f = e.downcast_ref::<crate::upload::UploadFailure>().unwrap();
        assert_eq!(
            (f.reason.as_str(), f.detail.as_str()),
            ("ava1_taken_over", "taken over by Phone")
        );
    }
}
