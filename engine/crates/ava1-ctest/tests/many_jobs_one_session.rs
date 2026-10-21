#![cfg(unix)]
//! Issue #475: the Files screen uploads a pick of many files one job per file, all on the
//! engine's one shared session. The console's job table holds 32 jobs, and an upload that ended
//! stayed listed until its session ended, so the 33rd file was answered BUSY for 12 retries and
//! failed ("the console stayed busy"). Ended uploads must make room on a session that runs on.
mod common;

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1_ctest::{c_job_count, CServer, LogOpts};
use common::{dir, SECRET};
use ps5upload_ava1::upload::upload_file_in;
use ps5upload_ava1::Pool;
use ps5upload_core::transfer::TransferConfig;

fn cfg(addr: &str) -> TransferConfig {
    let mut c = TransferConfig::new(addr);
    c.progress_bytes = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.progress_bytes_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.cancel = Some(Arc::new(AtomicBool::new(false)));
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn forty_single_file_jobs_in_a_row_on_one_session_all_land() {
    let t = dir("many-jobs");
    let ava = t.join("ava");
    std::fs::create_dir_all(&ava).unwrap();
    let me = Identity::load_or_create(&ava.join("identity")).unwrap();
    PeerStore::load(&t.join("srv-peers"))
        .unwrap()
        .add(me.public(), "engine")
        .unwrap();
    PeerStore::load(&ava.join("peers"))
        .unwrap()
        .add(Identity::from_secret(SECRET).public(), "C receiver")
        .unwrap();
    let srv = CServer::start_data_opts(
        SECRET,
        &t.join("srv-peers"),
        &t.join("jobs"),
        200,
        2000,
        2000,
        0,
        0,
        LogOpts::ON,
    );
    let src = t.join("src");
    let dest = t.join("dest");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let pool = Arc::new(Pool::new(ava.clone()).with_addr(srv.addr()));
    let mut slowest = std::time::Duration::ZERO;
    for n in 0..40u32 {
        let f = src.join(format!("f{n}.nca"));
        std::fs::write(&f, vec![n as u8; 1000 + n as usize * 37]).unwrap();
        let mut id = [0x47u8; 16];
        id[..4].copy_from_slice(&n.to_le_bytes());
        let (p, c, d, s) = (
            pool.clone(),
            cfg(&srv.addr()),
            dest.join(format!("f{n}.nca")).to_str().unwrap().to_string(),
            f.clone(),
        );
        let started = std::time::Instant::now();
        tokio::task::spawn_blocking(move || upload_file_in(&p, &c, id, &d, &s))
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("file {n}: {e:#} (jobs listed: {})", c_job_count()));
        slowest = slowest.max(started.elapsed());
        assert!(c_job_count() <= 32);
    }
    for n in 0..40u32 {
        assert_eq!(
            std::fs::read(dest.join(format!("f{n}.nca"))).unwrap(),
            std::fs::read(src.join(format!("f{n}.nca"))).unwrap()
        );
    }
    // A BUSY answer costs the sender at least one backoff (250 ms and doubling); a job that
    // waited for a slot shows up as a slow file.
    assert!(
        slowest < std::time::Duration::from_secs(5),
        "a file waited for a table slot: slowest {slowest:?}"
    );
    // The session is still up (the pool holds it): the ended jobs leave anyway.
    ava1_ctest::c_reap_far();
    assert_eq!(
        c_job_count(),
        0,
        "ended jobs stayed listed on a live session"
    );
    drop(pool);
}
