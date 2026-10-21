#![cfg(unix)]
//! Console to console (SPEC.md §18): the receiving console hands out a ticket
//! (c2c.allow), the sending console dials it with that ticket and pushes the job
//! (c2c.send). Console A is the C server in this process; console B is another C server
//! in a child process (`c2c_peer`), since the C server is a process-wide singleton. The
//! test itself plays the engine, paired with both.
mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ava1::gen::{self, C2cAllow, C2cSend, C2cTicket, JobRef, Status};
use ava1::keys::Identity;
use ava1::manifest;
use ava1::peers::PeerStore;
use ava1::send::{send_job, SendError, SendOptions};
use ava1::session::{connect, Session};
use ava1::source::LocalSource;
use ava1::wire::Message;
use ava1_ctest::CServer;
use common::*;

const B_SECRET: [u8; 32] = [0x43; 32];

/// Console B: a C server in its own process, killed when dropped.
struct Peer {
    child: Child,
    port: u16,
}

impl Peer {
    fn start(peers: &Path, jobs: &Path, fsync_delay_us: u32) -> Peer {
        let mut child = Command::new(env!("CARGO_BIN_EXE_c2c_peer"))
            .arg(B_SECRET[0].to_string())
            .arg(peers)
            .arg(jobs)
            .arg(fsync_delay_us.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line.trim().strip_prefix("PORT ").unwrap().parse().unwrap();
        Peer { child, port }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The engine: one identity, paired with both consoles.
fn engine(a_peers: &Path, b_peers: &Path) -> (Arc<Identity>, Arc<Mutex<PeerStore>>) {
    let me = Arc::new(Identity::generate().unwrap());
    for p in [a_peers, b_peers] {
        PeerStore::load(p)
            .unwrap()
            .add(me.public(), "engine")
            .unwrap();
    }
    let mut mine = PeerStore::in_memory();
    mine.add(Identity::from_secret(SECRET).public(), "A")
        .unwrap();
    mine.add(Identity::from_secret(B_SECRET).public(), "B")
        .unwrap();
    (me, Arc::new(Mutex::new(mine)))
}

async fn allow(b: &Session, job: [u8; 16], root: &Path) -> [u8; 16] {
    let body = C2cAllow {
        job_id: job,
        key: Identity::from_secret(SECRET).public(),
        root: root.to_str().unwrap().into(),
    };
    let r = b
        .rpc(gen::METHOD_C2C_ALLOW, &body.to_bytes().unwrap())
        .await
        .unwrap();
    assert_eq!(r.status, gen::STATUS_OK);
    C2cTicket::decode(&r.body).unwrap().token
}

/// c2c.send on A: (status, the error text when refused).
async fn send(
    a: &Session,
    job: [u8; 16],
    port: u16,
    key: [u8; 32],
    token: [u8; 16],
    src: &Path,
    dest: &Path,
) -> (u16, String) {
    let body = C2cSend {
        job_id: job,
        host: "127.0.0.1".into(),
        port,
        key,
        token,
        src: src.to_str().unwrap().into(),
        dest: dest.to_str().unwrap().into(),
        flags: 0,
    };
    let r = a
        .rpc(gen::METHOD_C2C_SEND, &body.to_bytes().unwrap())
        .await
        .unwrap();
    let text = if r.status == gen::STATUS_OK {
        String::new()
    } else {
        String::from_utf8_lossy(&r.body).into_owned()
    };
    (r.status, text)
}

async fn status(s: &Session, job: [u8; 16]) -> Option<Status> {
    let r = s
        .rpc(
            gen::METHOD_JOB_STATUS,
            &JobRef { job_id: job }.to_bytes().unwrap(),
        )
        .await
        .unwrap();
    (r.status == gen::STATUS_OK).then(|| Status::decode(&r.body).unwrap())
}

async fn wait_end(s: &Session, job: [u8; 16]) -> Status {
    for _ in 0..1200 {
        if let Some(st) = status(s, job).await {
            if st.state.unwrap_or(0) != 0 {
                return st;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the send did not end");
}

struct Pair {
    _srv: CServer,
    peer: Peer,
    a: Session,
    b: Session,
}

async fn pair(d: &Path, b_fsync_delay_us: u32) -> Pair {
    pair_with(d, b_fsync_delay_us, false).await
}

/// `a_paired_on_b`: B also knows A's key as a paired device.
async fn pair_with(d: &Path, b_fsync_delay_us: u32, a_paired_on_b: bool) -> Pair {
    ava1_ctest::c_set_read_allowed(true);
    let (me, mine) = engine(&d.join("a-peers"), &d.join("b-peers"));
    if a_paired_on_b {
        PeerStore::load(&d.join("b-peers"))
            .unwrap()
            .add(Identity::from_secret(SECRET).public(), "A")
            .unwrap();
    }
    let srv = CServer::start_data(
        SECRET,
        &d.join("a-peers"),
        &d.join("a-jobs"),
        200,
        2000,
        2000,
        0,
    );
    let peer = Peer::start(&d.join("b-peers"), &d.join("b-jobs"), b_fsync_delay_us);
    let a = connect(&srv.addr(), me.clone(), mine.clone(), "engine", calm())
        .await
        .unwrap();
    let b = connect(
        &format!("127.0.0.1:{}", peer.port),
        me,
        mine,
        "engine",
        calm(),
    )
    .await
    .unwrap();
    Pair {
        _srv: srv,
        peer,
        a,
        b,
    }
}

fn b_key() -> [u8; 32] {
    Identity::from_secret(B_SECRET).public()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_goes_straight_from_one_console_to_the_other() {
    let d = dir("c2c-folder");
    let src = d.join("a/game");
    write_tree(&src, 300, |i| {
        if i % 50 == 0 {
            (3 << 20) + i
        } else {
            1000 + i
        }
    });
    let dest = d.join("b/game");
    let p = pair(&d, 0).await;
    let job = [0xC1; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    let end = wait_end(&p.a, job).await;
    assert_eq!(end.state, Some(1), "{:?}", end.current);
    assert!(same_tree(&src, &dest));
    // The job on B is the engine's own: it can read it (and resume it through itself).
    let on_b = wait_end(&p.b, job).await;
    assert_eq!(on_b.state, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_file_goes_straight_across() {
    let d = dir("c2c-file");
    std::fs::create_dir_all(d.join("a")).unwrap();
    let src = d.join("a/app.ffpfsc");
    std::fs::write(&src, vec![7u8; (5 << 20) + 3]).unwrap();
    std::fs::create_dir_all(d.join("b")).unwrap();
    let dest = d.join("b/app.ffpfsc");
    let p = pair(&d, 0).await;
    let job = [0xC2; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    assert_eq!(wait_end(&p.a, job).await.state, Some(1));
    assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dest).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_token_or_key_is_refused_before_anything_is_sent() {
    let d = dir("c2c-refused");
    let src = d.join("a/game");
    write_tree(&src, 3, |_| 100);
    let dest = d.join("b/game");
    let p = pair(&d, 0).await;
    let job = [0xC3; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), [9; 16], &src, &dest).await;
    assert_eq!(st, gen::ERR_IO);
    assert!(why.contains("refused"), "{why}");
    let (st, why) = send(&p.a, job, p.peer.port, [5; 32], token, &src, &dest).await;
    assert_eq!(st, gen::ERR_IO);
    assert!(why.contains("not the one expected"), "{why}");
    assert!(!dest.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_covers_its_one_destination_only() {
    let d = dir("c2c-root");
    let src = d.join("a/game");
    write_tree(&src, 3, |_| 100);
    let p = pair(&d, 0).await;
    let job = [0xC4; 16];
    let token = allow(&p.b, job, &d.join("b/allowed")).await;
    let elsewhere = d.join("b/elsewhere");
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &elsewhere).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    let end = wait_end(&p.a, job).await;
    assert_eq!(end.state, Some(2));
    assert!(!elsewhere.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn nobody_listening_fails_at_once_with_the_reason() {
    let d = dir("c2c-unreachable");
    let src = d.join("a/game");
    write_tree(&src, 1, |_| 10);
    let p = pair(&d, 0).await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (st, why) = send(
        &p.a,
        [0xC5; 16],
        port,
        b_key(),
        [1; 16],
        &src,
        &d.join("b/x"),
    )
    .await;
    assert_eq!(st, gen::ERR_IO);
    assert!(why.contains("cannot reach"), "{why}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_send_resumes_with_the_same_job() {
    let d = dir("c2c-resume");
    let src = d.join("a/game");
    write_tree(&src, 400, |i| 2000 + i);
    let dest = d.join("b/game");
    // B syncs slowly, so the cancel lands mid-way.
    let p = pair(&d, 3000).await;
    let job = [0xC6; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let r =
        p.a.rpc(
            gen::METHOD_JOB_CANCEL,
            &JobRef { job_id: job }.to_bytes().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status, gen::STATUS_OK);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // Again, the same job: B picks up what it already holds.
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    let end = wait_end(&p.a, job).await;
    assert_eq!(end.state, Some(1), "{:?}", end.current);
    assert!(same_tree(&src, &dest));
}

/// The engine's pool directories for A and B, paired with each console (before the consoles
/// start: they read their peers file once).
fn pair_engine(d: &Path) {
    for (tag, console_peers, key) in [
        (
            "ava-a",
            d.join("a-peers"),
            Identity::from_secret(SECRET).public(),
        ),
        ("ava-b", d.join("b-peers"), b_key()),
    ] {
        let ava = d.join(tag);
        std::fs::create_dir_all(&ava).unwrap();
        let me = Identity::load_or_create(&ava.join("identity")).unwrap();
        PeerStore::load(&console_peers)
            .unwrap()
            .add(me.public(), "engine")
            .unwrap();
        PeerStore::load(&ava.join("peers"))
            .unwrap()
            .add(key, "console")
            .unwrap();
    }
}

/// Engine pools for A and B, as the app's engine holds them.
fn pools(d: &Path, b_port: u16, a_addr: &str) -> (ps5upload_ava1::Pool, ps5upload_ava1::Pool) {
    (
        ps5upload_ava1::Pool::new(d.join("ava-a")).with_addr(a_addr),
        ps5upload_ava1::Pool::new(d.join("ava-b")).with_addr(format!("127.0.0.1:{b_port}")),
    )
}

fn routed(
    d: &Path,
    pa: &ps5upload_ava1::Pool,
    pb: &ps5upload_ava1::Pool,
    src: &Path,
    dest: &Path,
    job: [u8; 16],
) -> (ps5upload_ava1::c2c::Route, Vec<ps5upload_ava1::c2c::Route>) {
    let _ = d;
    let seen = Mutex::new(Vec::new());
    let (_, route) = ps5upload_ava1::c2c::ps5_to_ps5_routed_between(
        pa,
        "192.0.2.1",
        src.to_str().unwrap(),
        pb,
        "192.0.2.2",
        dest.to_str().unwrap(),
        job,
        Arc::default(),
        Arc::default(),
        |r| seen.lock().unwrap().push(r.clone()),
    )
    .unwrap();
    (route, seen.into_inner().unwrap())
}

#[test]
fn the_engine_sends_directly_when_the_consoles_reach_each_other() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("c2c-engine-direct");
    let src = d.join("a/game");
    write_tree(&src, 120, |i| if i == 0 { 9 << 20 } else { 500 + i });
    let dest = d.join("b/game");
    pair_engine(&d);
    let srv = CServer::start_data(
        SECRET,
        &d.join("a-peers"),
        &d.join("a-jobs"),
        200,
        2000,
        2000,
        0,
    );
    let peer = Peer::start(&d.join("b-peers"), &d.join("b-jobs"), 0);
    let (pa, pb) = pools(&d, peer.port, &srv.addr());
    let (route, seen) = routed(&d, &pa, &pb, &src, &dest, [0xD1; 16]);
    assert_eq!(route, ps5upload_ava1::c2c::Route::Direct);
    assert_eq!(seen, vec![ps5upload_ava1::c2c::Route::Direct]);
    assert!(same_tree(&src, &dest));
}

#[test]
fn the_engine_carries_it_itself_when_the_consoles_cannot_connect_and_says_why() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("c2c-engine-fallback");
    let src = d.join("a/game");
    write_tree(&src, 60, |i| 700 + i);
    let dest = d.join("b/game");
    pair_engine(&d);
    let srv = CServer::start_data(
        SECRET,
        &d.join("a-peers"),
        &d.join("a-jobs"),
        200,
        2000,
        2000,
        0,
    );
    let peer = Peer::start(&d.join("b-peers"), &d.join("b-jobs"), 0);
    let (pa, pb) = pools(&d, peer.port, &srv.addr());
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    ps5upload_ava1::c2c::set_dial_port_for_tests(port);
    let (route, _) = routed(&d, &pa, &pb, &src, &dest, [0xD2; 16]);
    ps5upload_ava1::c2c::set_dial_port_for_tests(0);
    match route {
        ps5upload_ava1::c2c::Route::Relay { reason } => {
            assert!(reason.contains("could not connect"), "{reason}")
        }
        r => panic!("{r:?}"),
    }
    assert!(same_tree(&src, &dest));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sends_to_the_same_console_run_side_by_side() {
    let d = dir("c2c-two");
    let (src1, src2) = (d.join("a/one"), d.join("a/two"));
    write_tree(&src1, 300, |i| 3000 + i);
    write_tree(&src2, 300, |i| 4000 + i);
    let (dest1, dest2) = (d.join("b/one"), d.join("b/two"));
    // B syncs slowly, so the two sends overlap.
    let p = pair(&d, 2000).await;
    let (j1, j2) = ([0xC7; 16], [0xC8; 16]);
    let t1 = allow(&p.b, j1, &dest1).await;
    let t2 = allow(&p.b, j2, &dest2).await;
    let (st, why) = send(&p.a, j1, p.peer.port, b_key(), t1, &src1, &dest1).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    let (st, why) = send(&p.a, j2, p.peer.port, b_key(), t2, &src2, &dest2).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    let (e1, e2) = (wait_end(&p.a, j1).await, wait_end(&p.a, j2).await);
    assert_eq!(e1.state, Some(1), "{:?}", e1.current);
    assert_eq!(e2.state, Some(1), "{:?}", e2.current);
    assert!(same_tree(&src1, &dest1) && same_tree(&src2, &dest2));
}

/// The engine took the job over on B (a relay re-attached it to its own session) while A's
/// direct send was still running: A's late JobCancel, on the old session, must not cancel the
/// job the relay now drives.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_cancel_from_the_old_direct_session_spares_the_relay() {
    let d = dir("c2c-late-cancel");
    let src = d.join("a/game");
    write_tree(&src, 600, |i| 2000 + i);
    let dest = d.join("b/game");
    // B syncs slowly, so both sends are still running when A's cancel lands.
    let p = pair(&d, 3000).await;
    let job = [0xC9; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The relay: the engine opens the same job on B over its own session.
    let s = LocalSource::new(src.clone());
    let m = manifest::walk(&s, &|_: &str| false).unwrap();
    let mut link = p.b.job(job);
    let relay = send_job(
        &mut link,
        Arc::new(m),
        Arc::new(s),
        SendOptions::upload(dest.to_str().unwrap()),
    );
    let cancel_a = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let r =
            p.a.rpc(
                gen::METHOD_JOB_CANCEL,
                &JobRef { job_id: job }.to_bytes().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status, gen::STATUS_OK);
    };
    let (report, ()) = tokio::join!(
        async {
            tokio::time::timeout(Duration::from_secs(60), relay)
                .await
                .expect("the relay's send did not end")
        },
        cancel_a
    );
    let report = report.expect("the relay's send");
    assert_eq!(report.status, gen::STATUS_OK, "{:?}", report.message);
    assert!(same_tree(&src, &dest));
}

/// c2c.send replaces only an earlier send this device started under the same id: any other job
/// (here an upload into A) is left alone and the send is refused before anything is dialled.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_never_replaces_another_job_under_its_id() {
    let d = dir("c2c-same-id");
    let src = d.join("a/game");
    write_tree(&src, 20, |i| 300 + i);
    let into_a = d.join("a/uploaded");
    let p = pair(&d, 0).await;
    let job = [0xCA; 16];
    let s = LocalSource::new(src.clone());
    let m = manifest::walk(&s, &|_: &str| false).unwrap();
    let mut link = p.a.job(job);
    let r = send_job(
        &mut link,
        Arc::new(m),
        Arc::new(s),
        SendOptions::upload(into_a.to_str().unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(r.status, gen::STATUS_OK, "{:?}", r.message);
    let before = status(&p.a, job).await.expect("the upload is listed");
    let dest = d.join("b/game");
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::ERR_BUSY, "{why}");
    let after = status(&p.a, job).await.expect("the upload is still listed");
    assert_eq!(after.state, before.state);
    assert_eq!(after.bytes_total, before.bytes_total);
    assert!(!dest.exists());
}

/// A source the read policy denies is refused the same whether or not it exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_denied_source_does_not_say_whether_it_exists() {
    let d = dir("c2c-denied");
    let there = d.join("ps5-denied/there");
    write_tree(&there, 1, |_| 10);
    let p = pair(&d, 0).await;
    let mut texts = Vec::new();
    for src in [there, d.join("ps5-denied/missing")] {
        let (st, why) = send(
            &p.a,
            [0xCB; 16],
            p.peer.port,
            b_key(),
            [1; 16],
            &src,
            &d.join("b/x"),
        )
        .await;
        assert_eq!(st, gen::ERR_PATH, "{why}");
        assert!(why.contains("not allowed"), "{why}");
        texts.push(why.replace(src.to_str().unwrap(), "<src>"));
    }
    assert_eq!(texts[0], texts[1]);
}

/// A console B already knows as a paired device still gets only the ticket's session when it
/// dials with a ticket: the job on B is the ticket owner's, and it may write only the ticket's root.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_decides_even_for_a_paired_key() {
    let d = dir("c2c-ticket-paired");
    let src = d.join("a/game");
    write_tree(&src, 5, |i| 100 + i);
    let p = pair_with(&d, 0, true).await;
    // Another destination than the ticket's: refused there.
    let job = [0xCC; 16];
    let token = allow(&p.b, job, &d.join("b/allowed")).await;
    let elsewhere = d.join("b/elsewhere");
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &elsewhere).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    assert_eq!(wait_end(&p.a, job).await.state, Some(2));
    assert!(!elsewhere.exists());
    // The ticket's own destination: the job on B is the engine's (the ticket's owner).
    let job = [0xCD; 16];
    let dest = d.join("b/allowed2");
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    assert_eq!(wait_end(&p.a, job).await.state, Some(1));
    assert_eq!(wait_end(&p.b, job).await.state, Some(1));
    assert!(same_tree(&src, &dest));
}

// ---- another device (#444) -----------------------------------------------------------------

/// The engine and a second device (a phone, another computer), each paired with both consoles.
struct Pair2 {
    p: Pair,
    a2: Session,
    b2: Session,
}

async fn pair2(d: &Path, b_fsync_delay_us: u32) -> Pair2 {
    // Paired before the consoles start: they read their peers file once.
    let (me2, mine2) = engine(&d.join("a-peers"), &d.join("b-peers"));
    let p = pair(d, b_fsync_delay_us).await;
    let a2 = connect(&p._srv.addr(), me2.clone(), mine2.clone(), "phone", calm())
        .await
        .unwrap();
    let b2 = connect(
        &format!("127.0.0.1:{}", p.peer.port),
        me2,
        mine2,
        "phone",
        calm(),
    )
    .await
    .unwrap();
    Pair2 { p, a2, b2 }
}

async fn cancel(s: &Session, job: [u8; 16]) -> u16 {
    s.rpc(
        gen::METHOD_JOB_CANCEL,
        &JobRef { job_id: job }.to_bytes().unwrap(),
    )
    .await
    .unwrap()
    .status
}

/// A send that a just-cancelled half may still refuse for a moment (busy on A, "still closing;
/// try again" on B; the engine waits for both the same way): sent again until it runs to its
/// end.
async fn send_to_end(
    a: &Session,
    b: &Session,
    job: [u8; 16],
    port: u16,
    src: &Path,
    dest: &Path,
) -> Status {
    for _ in 0..20 {
        let token = allow(b, job, dest).await;
        let (st, why) = send(a, job, port, b_key(), token, src, dest).await;
        if st == gen::ERR_BUSY {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        assert_eq!(st, gen::STATUS_OK, "{why}");
        let end = wait_end(a, job).await;
        let closing = end
            .current
            .as_deref()
            .is_some_and(|c| c.contains("try again"));
        if end.state == Some(1) || !closing {
            return end;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!("the send kept being refused as closing");
}

/// The job is no longer listed for `s` (within 5 s).
async fn gone(s: &Session, job: [u8; 16]) -> bool {
    for _ in 0..100 {
        if status(s, job).await.is_none() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_send_resumes_from_another_device() {
    let d = dir("c2c-handover");
    let src = d.join("a/game");
    write_tree(&src, 2000, |i| 2000 + i);
    let dest = d.join("b/game");
    // B syncs slowly, so the pause lands mid-way.
    let q = pair2(&d, 3000).await;
    let p = &q.p;
    let job = [0xE1; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    // The engine pauses it: both halves end (A's watcher may have ended B's already).
    assert_eq!(cancel(&p.a, job).await, gen::STATUS_OK);
    let b_end = cancel(&p.b, job).await;
    assert!(
        matches!(b_end, gen::STATUS_OK | gen::ERR_UNKNOWN_JOB),
        "{b_end}"
    );
    assert!(gone(&p.b, job).await, "B's half is ended");
    // The other device resumes it under the same id: B continues from its journal.
    let end = send_to_end(&q.a2, &q.b2, job, p.peer.port, &src, &dest).await;
    assert_eq!(end.state, Some(1), "{:?}", end.current);
    assert!(same_tree(&src, &dest));
    // Both halves are the other device's now.
    assert_eq!(wait_end(&q.b2, job).await.state, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn another_paired_device_reads_and_cancels_a_c2c_send() {
    let d = dir("c2c-shared");
    let src = d.join("a/game");
    write_tree(&src, 2000, |i| 2000 + i);
    let dest = d.join("b/game");
    let q = pair2(&d, 3000).await;
    let p = &q.p;
    for s in [&p.a, &q.b2] {
        assert_ne!(s.peer_caps() & gen::CAP_SHARED_JOBS, 0, "advertised");
    }
    let job = [0xE2; 16];
    let token = allow(&p.b, job, &dest).await;
    let (st, why) = send(&p.a, job, p.peer.port, b_key(), token, &src, &dest).await;
    assert_eq!(st, gen::STATUS_OK, "{why}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The other device sees both halves: the send on A and the upload on B.
    let on_a = status(&q.a2, job)
        .await
        .expect("A's half, read by the other device");
    assert_eq!(on_a.state.unwrap_or(0), 0);
    assert!(status(&q.b2, job).await.is_some(), "B's half");
    // ...and ends them, as a take-over does.
    assert_eq!(cancel(&q.a2, job).await, gen::STATUS_OK);
    let b_end = cancel(&q.b2, job).await;
    assert!(
        matches!(b_end, gen::STATUS_OK | gen::ERR_UNKNOWN_JOB),
        "{b_end}"
    );
    assert!(gone(&p.a, job).await, "ended on A for its own device too");
    assert!(gone(&p.b, job).await, "ended on B for its own device too");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_job_moves_to_the_device_that_reopens_it() {
    let d = dir("c2c-parked");
    let src = d.join("a/game");
    write_tree(&src, 2000, |i| 2000 + i);
    let dest = d.join("b/game");
    let Pair2 { p, b2, .. } = pair2(&d, 3000).await;
    let Pair { _srv, peer, b, .. } = p;
    let job = [0xE3; 16];
    // The engine uploads into B and goes away mid-way: B parks the job under the engine.
    let s = LocalSource::new(src.clone());
    let m = manifest::walk(&s, &|_: &str| false).unwrap();
    {
        let mut link = b.job(job);
        let cut = tokio::time::timeout(
            Duration::from_millis(400),
            send_job(
                &mut link,
                Arc::new(m.clone()),
                Arc::new(s),
                SendOptions::upload(dest.to_str().unwrap()),
            ),
        )
        .await;
        assert!(cut.is_err(), "the upload was cut mid-way");
    }
    b.close().await;
    // The other device reopens the same job: it moves to it and finishes from B's journal.
    let mut report = None;
    for _ in 0..40 {
        let mut link = b2.job(job);
        match send_job(
            &mut link,
            Arc::new(m.clone()),
            Arc::new(LocalSource::new(src.clone())),
            SendOptions::upload(dest.to_str().unwrap()),
        )
        .await
        {
            Ok(r) => {
                report = Some(r);
                break;
            }
            // Until B notices the engine's session is gone, the job is still attached to it.
            Err(SendError::Refused { status, .. })
                if status == gen::ERR_UNKNOWN_JOB || status == gen::ERR_BUSY =>
            {
                tokio::time::sleep(Duration::from_millis(250)).await
            }
            Err(e) => panic!("{e}"),
        }
    }
    let r = report.expect("the other device reopened the parked job");
    assert_eq!(r.status, gen::STATUS_OK, "{:?}", r.message);
    assert!(same_tree(&src, &dest));
    drop(peer);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attached_job_of_another_device_is_still_refused() {
    let d = dir("c2c-attached");
    let src = d.join("a/game");
    write_tree(&src, 2000, |i| 2000 + i);
    let dest = d.join("b/game");
    let q = pair2(&d, 3000).await;
    let job = [0xE4; 16];
    let s = LocalSource::new(src.clone());
    let m = Arc::new(manifest::walk(&s, &|_: &str| false).unwrap());
    let mut link = q.p.b.job(job);
    let running = send_job(
        &mut link,
        m.clone(),
        Arc::new(s),
        SendOptions::upload(dest.to_str().unwrap()),
    );
    let other = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut link = q.b2.job(job);
        send_job(
            &mut link,
            m.clone(),
            Arc::new(LocalSource::new(src.clone())),
            SendOptions::upload(dest.to_str().unwrap()),
        )
        .await
    };
    let (first, second) = tokio::join!(running, other);
    match second {
        Err(SendError::Refused { status, message }) => {
            assert_eq!(status, gen::ERR_UNKNOWN_JOB);
            assert!(message.contains("another device"), "{message}");
        }
        r => panic!("the other device's open was not refused: {r:?}"),
    }
    let first = first.unwrap();
    assert_eq!(first.status, gen::STATUS_OK, "{:?}", first.message);
    assert!(same_tree(&src, &dest));
}

#[tokio::test(flavor = "multi_thread")]
async fn operation_jobs_stay_owner_only() {
    let d = dir("c2c-op-owner");
    let q = pair2(&d, 0).await;
    let job = [0xE5; 16];
    let body = gen::JobRun {
        job_id: job,
        op: gen::JOB_OP_FSCK,
        args: br#"{"device":"/dev/md1","loops":30}"#.to_vec(),
    };
    let r =
        q.p.a
            .rpc(gen::METHOD_JOB_RUN, &body.to_bytes().unwrap())
            .await
            .unwrap();
    assert_eq!(r.status, gen::STATUS_OK);
    // Unlike a transfer, another device neither sees nor cancels it.
    assert!(status(&q.a2, job).await.is_none());
    assert_eq!(cancel(&q.a2, job).await, gen::ERR_UNKNOWN_JOB);
    assert_eq!(wait_end(&q.p.a, job).await.state, Some(1));
}

#[test]
fn cancelling_a_routed_send_ends_both_halves() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("c2c-engine-pause");
    let src = d.join("a/game");
    write_tree(&src, 3000, |i| 2000 + i);
    let dest = d.join("b/game");
    pair_engine(&d);
    let srv = CServer::start_data(
        SECRET,
        &d.join("a-peers"),
        &d.join("a-jobs"),
        200,
        2000,
        2000,
        0,
    );
    // B syncs slowly, so the pause lands mid-way.
    let peer = Peer::start(&d.join("b-peers"), &d.join("b-jobs"), 3000);
    let (pa, pb) = pools(&d, peer.port, &srv.addr());
    let job = [0xD3; 16];
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pause = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    };
    let r = ps5upload_ava1::c2c::ps5_to_ps5_routed_between(
        &pa,
        "192.0.2.1",
        src.to_str().unwrap(),
        &pb,
        "192.0.2.2",
        dest.to_str().unwrap(),
        job,
        Arc::default(),
        cancel,
        |_| {},
    );
    pause.join().unwrap();
    let e = r.expect_err("paused");
    assert!(format!("{e:#}").contains("transfer_cancelled"), "{e:#}");
    // At once, not whenever A's watcher gets round to it: B lists the job no more.
    let on_b = ps5upload_ava1::block_on(async {
        let s = pb.session("192.0.2.2").await.unwrap();
        s.rpc(
            gen::METHOD_JOB_STATUS,
            &JobRef { job_id: job }.to_bytes().unwrap(),
        )
        .await
        .unwrap()
        .status
    });
    assert_eq!(on_b, gen::ERR_UNKNOWN_JOB);
}
