//! #473: `appdb_query` pages through app.db instead of trusting one capped reply.
//!
//! The helper used to answer at most 256 rows (and fewer when its buffer filled) with nothing
//! saying the list was cut. It now answers a window (`offset`/`limit` in, `next`/`more`/`truncated`
//! out); the engine follows it to the end, bounded, and says when the list is not whole.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use ps5upload_core::diagnostics::{appdb_query, APPDB_MAX_PAGES};
use ps5upload_core::mgmt::{self, Method, MgmtTransport};
use serde_json::{json, Value};

type Script = Box<dyn Fn(&Value) -> Result<Option<Vec<u8>>> + Send + Sync>;

struct Fake {
    seen: Mutex<Vec<Value>>,
    reply: Script,
}

impl MgmtTransport for Fake {
    fn call(
        &self,
        _addr: &str,
        method: Method,
        _label: &str,
        body: &[u8],
        _timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        assert_eq!(method.id, 56, "app.db_query");
        let req: Value = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(body).expect("the request is JSON")
        };
        self.seen.lock().unwrap().push(req.clone());
        (self.reply)(&req)
    }
}

fn fake(
    f: impl Fn(&Value) -> Result<Option<Vec<u8>>> + Send + Sync + 'static,
) -> (Arc<Fake>, mgmt::ScopedTransport) {
    let t = Arc::new(Fake {
        seen: Mutex::default(),
        reply: Box::new(f),
    });
    let g = mgmt::scoped_transport(t.clone());
    (t, g)
}

fn title(i: usize) -> String {
    format!("CUSA{i:05}")
}

/// A helper that serves `rows` titles (plus a late-sorting ZZZZ99999) `page` at a time.
fn paged_helper(rows: usize, page: usize) -> impl Fn(&Value) -> Result<Option<Vec<u8>>> {
    move |req| {
        let mut all: Vec<String> = (0..rows).map(title).collect();
        all.push("ZZZZ99999".into());
        let off = req["offset"].as_u64().unwrap_or(0) as usize;
        let end = (off + page).min(all.len());
        let apps: Vec<Value> = all[off.min(all.len())..end]
            .iter()
            .map(|t| json!({"title_id": t, "app_id": 1, "name": format!("Game {t}")}))
            .collect();
        Ok(Some(
            serde_json::to_vec(&json!({
                "apps": apps,
                "source": "sqlite",
                "offset": off,
                "next": end,
                "more": end < all.len(),
                "truncated": false,
                "total": all.len(),
            }))
            .unwrap(),
        ))
    }
}

const A: &str = "10.0.0.5";

#[test]
fn pages_until_the_helper_says_there_is_no_more() {
    for rows in [255usize, 256, 257, 1000, 1500] {
        let (t, _g) = fake(paged_helper(rows, 400));
        let d = appdb_query(A).unwrap();
        assert_eq!(d.apps.len(), rows + 1, "{rows} rows");
        assert!(!d.truncated, "{rows} rows");
        assert!(d.err.is_none());
        assert_eq!(d.apps.last().unwrap().title_id, "ZZZZ99999");
        assert_eq!(d.source.as_deref(), Some("sqlite"));
        let seen = t.seen.lock().unwrap();
        assert_eq!(seen.len(), (rows + 1).div_ceil(400), "{rows} rows");
        for (i, req) in seen.iter().enumerate() {
            assert_eq!(req["offset"], json!(i * 400), "page {i}");
        }
    }
}

#[test]
fn an_older_helper_answers_once_and_a_full_256_is_not_called_whole() {
    // No next/more: the helper predates paging, so one reply is all there is.
    let (t, _g) = fake(|_| {
        let apps: Vec<Value> = (0..256)
            .map(|i| json!({"title_id": title(i), "app_id": 1, "name": "n"}))
            .collect();
        Ok(Some(
            serde_json::to_vec(&json!({"apps": apps, "source": "sqlite"})).unwrap(),
        ))
    });
    let d = appdb_query(A).unwrap();
    assert_eq!(d.apps.len(), 256);
    assert!(d.truncated, "256 rows from an older helper may be its cap");
    assert_eq!(t.seen.lock().unwrap().len(), 1);
    drop(_g);

    let (_t, _g) = fake(|_| {
        Ok(Some(
            br#"{"apps":[{"title_id":"CUSA00001","app_id":1,"name":"n"}],"source":"scan"}"#
                .to_vec(),
        ))
    });
    let d = appdb_query(A).unwrap();
    assert_eq!(d.apps.len(), 1);
    assert!(!d.truncated);
}

#[test]
fn a_truncated_page_marks_the_whole_list_truncated() {
    let (_t, _g) = fake(|req| {
        let off = req["offset"].as_u64().unwrap_or(0);
        Ok(Some(
            serde_json::to_vec(&if off == 0 {
                json!({"apps":[{"title_id":"CUSA00001","app_id":1,"name":"a"}],
                       "offset":0,"next":1,"more":true,"truncated":false})
            } else {
                json!({"apps":[],"offset":1,"next":1,"more":false,"truncated":true})
            })
            .unwrap(),
        ))
    });
    let d = appdb_query(A).unwrap();
    assert_eq!(d.apps.len(), 1);
    assert!(d.truncated);
}

#[test]
fn a_page_that_makes_no_progress_stops_the_loop() {
    let (t, _g) = fake(|_| {
        Ok(Some(
            br#"{"apps":[],"offset":0,"next":0,"more":true,"truncated":false}"#.to_vec(),
        ))
    });
    let d = appdb_query(A).unwrap();
    assert!(d.truncated);
    assert_eq!(t.seen.lock().unwrap().len(), 1);
}

#[test]
fn the_page_count_is_bounded() {
    // A helper that always has more: the engine gives up after APPDB_MAX_PAGES calls.
    let (t, _g) = fake(|req| {
        let off = req["offset"].as_u64().unwrap_or(0);
        Ok(Some(
            serde_json::to_vec(&json!({
                "apps":[{"title_id": title(off as usize), "app_id": 1, "name": "n"}],
                "offset": off, "next": off + 1, "more": true, "truncated": false
            }))
            .unwrap(),
        ))
    });
    let d = appdb_query(A).unwrap();
    assert!(d.truncated);
    assert_eq!(d.apps.len(), APPDB_MAX_PAGES);
    assert_eq!(t.seen.lock().unwrap().len(), APPDB_MAX_PAGES);
}

#[test]
fn errors_keep_their_meaning() {
    // The helper's err envelope on page one is the answer, as before.
    let (_t, _g) = fake(|_| Ok(Some(br#"{"err":"cannot read app.db","apps":[]}"#.to_vec())));
    let d = appdb_query(A).unwrap();
    assert_eq!(d.err.as_deref(), Some("cannot read app.db"));
    assert!(d.apps.is_empty());
    drop(_g);

    // A transport failure on page one is still an error.
    let (_t, _g) = fake(|_| Err(anyhow!("connection reset")));
    assert!(appdb_query(A).is_err());
    drop(_g);

    // A failure after page one keeps the rows read so far, flagged incomplete with the reason,
    // so a caller never mistakes the partial list for the whole one.
    let (_t, _g) = fake(|req| {
        if req["offset"].as_u64().unwrap_or(0) == 0 {
            Ok(Some(
                br#"{"apps":[{"title_id":"CUSA00001","app_id":1,"name":"a"}],"offset":0,"next":1,"more":true,"truncated":false}"#
                    .to_vec(),
            ))
        } else {
            Err(anyhow!("connection reset"))
        }
    });
    let d = appdb_query(A).unwrap();
    assert_eq!(d.apps.len(), 1);
    assert!(d.truncated);
    assert!(d.err.as_deref().unwrap_or("").contains("connection reset"));
}

#[test]
fn a_row_count_that_changes_between_pages_marks_the_list_truncated() {
    // A row deleted between pages shifts the window: the first row of page two was skipped.
    let (_t, _g) = fake(|req| {
        let off = req["offset"].as_u64().unwrap_or(0);
        Ok(Some(
            serde_json::to_vec(&if off == 0 {
                json!({"apps":[{"title_id":"CUSA00001","app_id":1,"name":"a"}],
                       "offset":0,"next":1,"more":true,"truncated":false,"total":3})
            } else {
                json!({"apps":[{"title_id":"CUSA00003","app_id":1,"name":"c"}],
                       "offset":1,"next":2,"more":false,"truncated":false,"total":2})
            })
            .unwrap(),
        ))
    });
    let d = appdb_query(A).unwrap();
    assert_eq!(d.apps.len(), 2);
    assert!(d.truncated);
}
