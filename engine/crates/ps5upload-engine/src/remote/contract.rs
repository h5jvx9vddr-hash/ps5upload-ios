//! What every [`RemoteFs`] must do, checked against whatever tree the server has under `root`.
//! It only reads, so it can run against a real share, FTP or SFTP server.

use super::{RemoteError, RemoteFs};

pub(crate) async fn check(fs: &dyn RemoteFs, root: &str) {
    let page = fs.list(root, None).await.expect("list the root");
    assert!(
        !page.entries.is_empty(),
        "the test tree under {root} is empty"
    );
    let mut saw_file = false;
    for e in &page.entries {
        let p = super::path::join(root, &e.name).unwrap();
        let st = fs.stat(&p).await.expect("stat a listed entry");
        assert_eq!(st.is_dir, e.is_dir, "{p}");
        if e.is_dir {
            continue;
        }
        saw_file = true;
        assert_eq!(st.size, e.size, "{p}");
        let f = fs.open(&p).await.expect("open a listed file");
        assert_eq!(f.size(), e.size, "{p}");
        let n = e.size.min(16);
        let head = f.read_at(0, n).await.expect("read the head");
        assert_eq!(head.len() as u64, n, "{p}");
        if e.size > 4 {
            let tail = f.read_at(e.size - 2, 16).await.expect("read past the end");
            assert_eq!(tail.len(), 2, "a read past the end comes back short, {p}");
            let mid = f.read_at(2, 2).await.unwrap();
            assert_eq!(mid, head[2..4], "positioned reads agree, {p}");
        }
    }
    assert!(
        saw_file,
        "the test tree under {root} has no file at its top level"
    );
    let walked = fs.walk(root, 10_000).await.expect("walk");
    assert!(walked
        .iter()
        .all(|(rel, e)| !e.is_dir && !rel.starts_with('/')));
    assert!(matches!(
        fs.stat(&format!(
            "{}/definitely-not-here-7f3a",
            root.trim_end_matches('/')
        ))
        .await,
        Err(RemoteError::NotFound(_))
    ));
}

/// What every writable [`RemoteFs`] must do. `dir` holds two files `a` and `b` of different
/// sizes; the tree is left as found, except that a backend which never removes folders
/// (WebDAV: its DELETE is recursive) leaves the test's empty folders behind.
pub(crate) async fn check_writes(fs: &dyn RemoteFs, dir: &str) {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let at = |n: &str| super::path::join(dir, &format!("{n}-{run}")).unwrap();
    let file = |n: &str| super::path::join(dir, n).unwrap();
    let (a, b) = (file("a"), file("b"));
    let size_a = fs.stat(&a).await.expect("stat a").size;
    let size_b = fs.stat(&b).await.expect("stat b").size;
    assert_ne!(size_a, size_b, "a and b must differ in size");

    // Never overwrite: the rename fails and both files stay exactly as they were.
    let clash = fs.rename(&a, &b).await;
    assert!(
        matches!(clash, Err(RemoteError::Exists(_))),
        "rename onto an existing file must fail as Exists, got {clash:?}"
    );
    assert_eq!(fs.stat(&a).await.expect("a survives").size, size_a);
    assert_eq!(fs.stat(&b).await.expect("b survives").size, size_b);

    // A folder: made once, made again without complaint; a file's name is not a folder.
    let new = at("ps5u-new");
    fs.create_dir(&new).await.expect("create a folder");
    fs.create_dir(&new)
        .await
        .expect("a folder already there is fine");
    assert!(fs.stat(&new).await.expect("stat the new folder").is_dir);
    assert!(
        fs.create_dir(&a).await.is_err(),
        "a file is not a folder to create"
    );

    // Into it and back out, by rename; a full folder is not removed.
    let moved = super::path::join(&new, "a2").unwrap();
    fs.rename(&a, &moved).await.expect("rename into a folder");
    assert!(matches!(fs.stat(&a).await, Err(RemoteError::NotFound(_))));
    assert_eq!(fs.stat(&moved).await.expect("moved").size, size_a);
    let full = fs.remove_dir(&new).await;
    assert!(
        full.is_err(),
        "a folder with a file in it must not be removed"
    );
    let removes = !matches!(full, Err(RemoteError::Unsupported(_)));
    assert_eq!(fs.stat(&moved).await.expect("still there").size, size_a);
    fs.rename(&moved, &a).await.expect("rename back");
    if removes {
        fs.remove_dir(&new).await.expect("remove the empty folder");
        assert!(matches!(fs.stat(&new).await, Err(RemoteError::NotFound(_))));
    } else {
        assert!(fs.stat(&new).await.expect("left in place").is_dir);
    }

    // Into a folder that is not there: refused, the file untouched.
    let nowhere = format!("{}/x", at("ps5u-missing"));
    assert!(fs.rename(&a, &nowhere).await.is_err());
    assert_eq!(fs.stat(&a).await.expect("a untouched").size, size_a);
    assert!(fs.remove_dir(&at("ps5u-missing")).await.is_err());

    // Folders rename too.
    let (d1, d2) = (at("ps5u-d1"), at("ps5u-d2"));
    fs.create_dir(&d1).await.expect("create d1");
    fs.rename(&d1, &d2).await.expect("rename a folder");
    assert!(fs.stat(&d2).await.expect("d2").is_dir);
    assert!(matches!(fs.stat(&d1).await, Err(RemoteError::NotFound(_))));
    if removes {
        fs.remove_dir(&d2).await.expect("remove d2");
    }
}
