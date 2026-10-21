//! A package this crate builds reads back, through `pkg_source`, as the folder it was built from:
//! every layout the builder writes, names, sizes, bytes (whole and by range) and empty
//! directories. Packages it cannot read are refused with the reason in plain words.

use std::path::{Path, PathBuf};

use ps5upload_fpkg::build::{self, BuildRequest};
use ps5upload_fpkg::crypto::DEFAULT_PASSCODE;
use ps5upload_fpkg::inner::MetaCodec;
use ps5upload_fpkg::kraken::Level;
use ps5upload_fpkg::pkg_source::{self, PkgSource};
use ps5upload_fpkg::source::{self, SourceTree};
use ps5upload_fpkg::{inner, plan, ImageMode};

const CONTENT_ID: &str = "UP0000-PPSA04690_00-PKGSOURCETEST000";

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("fpkg-pkgsource-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Bytes that are neither all one value nor random: some runs compress, some do not.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|i| {
            if (i / 4096) % 3 == 0 {
                (i % 7) as u8
            } else {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            }
        })
        .collect()
}

/// A small game: a module, the `sce_sys` files the container takes over, a 0-byte file, a file
/// over several 256 KiB blocks, nested directories and two empty ones (one nested).
fn write_game(root: &Path, big: usize) {
    let put = |path: &str, data: &[u8]| {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, data).unwrap();
    };
    put("eboot.bin", &pattern(5000, 1));
    put(
        "sce_sys/param.json",
        format!(
            "{{\"contentId\":\"{CONTENT_ID}\",\"contentVersion\":\"01.001.000\",\"titleId\":\"PPSA04690\",\"applicationCategoryType\":0,\"attributePub\":0,\"applicationDrmType\":\"standard\"}}"
        )
        .as_bytes(),
    );
    put("sce_sys/icon0.png", &pattern(3000, 2));
    put("sce_sys/pic0.png", &pattern(7000, 3));
    put("sce_sys/trophy2/trophy00.ucp", &pattern(900, 4));
    put("data/zero.bin", &[]);
    put("data/big.bin", &pattern(big, 5));
    put("data/nested/deep/leaf.txt", b"a leaf three levels down\n");
    put("data/nested/odd.bin", &pattern(65_537, 6));
    std::fs::create_dir_all(root.join("data/empty")).unwrap();
    std::fs::create_dir_all(root.join("data/nested/hollow")).unwrap();
}

fn request(src: &Path, out: &Path) -> BuildRequest {
    let mut r = BuildRequest::new(src, out);
    r.time = Some((1_700_000_000, 0));
    r.seed = Some([0x5A; 16]);
    // Pinned against the environment's overrides, so each test builds exactly what it names.
    r.kraken = true;
    r.level = Level::Balanced;
    r.metadata_codec = MetaCodec::Stored;
    r.image_mode = ImageMode::PlaintextNoAuth;
    r.firmware = None;
    r
}

fn build(req: &BuildRequest) -> PathBuf {
    let report = build::build(req, &mut |_| {}).unwrap();
    assert!(report.verify.ok(), "{}", report.verify);
    report.path
}

/// The package's tree is the source's: the same files (plus the keystone the build generates),
/// the same bytes — whole, by odd ranges and read in odd-sized pieces — and the same empty
/// directories. `param.json` is the one file the build rewrites, so it is compared by content id.
fn assert_matches_source(tree: &mut dyn SourceTree, src: &Path) {
    let (mut want, want_empty) = source::scan_tree(src).unwrap();
    want.push(source::SourceFile {
        path: plan::KEYSTONE.to_string(),
        size: plan::KEYSTONE_LEN,
    });
    want.sort_by(|a, b| a.path.cmp(&b.path));
    let got: Vec<_> = tree.files().to_vec();
    let names = |v: &[source::SourceFile]| v.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    assert_eq!(names(&got), names(&want), "file names");
    assert_eq!(tree.empty_dirs(), &want_empty[..], "empty directories");

    for f in &got {
        let data = tree.read(&f.path).unwrap();
        assert_eq!(data.len() as u64, f.size, "{} size", f.path);
        if f.path == "sce_sys/param.json" {
            assert_eq!(source::content_id(&data).as_deref(), Some(CONTENT_ID));
            continue;
        }
        let expected = if f.path == plan::KEYSTONE {
            inner::keystone(DEFAULT_PASSCODE).to_vec()
        } else {
            std::fs::read(src.join(&f.path)).unwrap()
        };
        assert_eq!(data.len(), expected.len(), "{} size", f.path);
        assert!(data == expected, "{} bytes", f.path);

        // Odd ranges: a byte in, across 64 KiB and 256 KiB boundaries, and past the end.
        let n = expected.len() as u64;
        for (off, len) in [
            (1u64, 3usize),
            (n.saturating_sub(5), 11),
            (65_535, 70_001),
            (262_141, 7),
            (n, 10),
            (n + 100, 10),
        ] {
            let got = tree.read_range(&f.path, off, len).unwrap();
            let a = (off as usize).min(expected.len());
            let b = (a + len).min(expected.len());
            assert!(got == expected[a..b], "{} range {off}+{len}", f.path);
        }
        let mut pieced = Vec::new();
        while (pieced.len() as u64) < n {
            pieced.extend(
                tree.read_range(&f.path, pieced.len() as u64, 100_003)
                    .unwrap(),
            );
        }
        assert!(pieced == expected, "{} read in pieces", f.path);
    }
}

fn round_trip(name: &str, adjust: impl FnOnce(&mut BuildRequest)) -> String {
    let src = TempDir::new(&format!("{name}-src"));
    let out = TempDir::new(&format!("{name}-out"));
    write_game(src.path(), 700 * 1024);
    let mut req = request(src.path(), out.path());
    adjust(&mut req);
    let pkg = build(&req);
    let mut tree = source::open(&pkg).unwrap();
    assert_matches_source(tree.as_mut(), src.path());
    tree.describe()
}

#[test]
fn a_kraken_package_at_each_level_reads_back_as_its_folder() {
    for level in [Level::Fast, Level::Balanced, Level::Smallest] {
        let described = round_trip(&format!("kraken-{level:?}"), |r| r.level = level);
        assert!(described.contains("Kraken block layout"), "{described}");
    }
}

#[test]
fn a_stored_block_package_reads_back_as_its_folder() {
    let described = round_trip("blocks-stored", |r| r.level = Level::Stored);
    assert!(described.contains("stored block layout"), "{described}");
}

#[test]
fn a_flat_package_reads_back_with_either_metadata_codec() {
    let described = round_trip("flat-stored", |r| r.kraken = false);
    assert!(described.contains("flat layout"), "{described}");
    let described = round_trip("flat-zlib", |r| {
        r.kraken = false;
        r.metadata_codec = MetaCodec::Zlib;
    });
    assert!(described.contains("zlib metadata"), "{described}");
}

#[test]
fn a_native_package_with_the_debug_passcode_reads_back() {
    round_trip("native-blocks", |r| r.image_mode = ImageMode::Native);
    round_trip("native-flat", |r| {
        r.image_mode = ImageMode::Native;
        r.kraken = false;
    });
}

/// A native package made with any other passcode cannot be opened with the debug one, which is
/// what a retail package looks like from here.
#[test]
fn a_package_made_with_another_passcode_is_refused_as_retail() {
    let src = TempDir::new("retail-src");
    let out = TempDir::new("retail-out");
    write_game(src.path(), 300 * 1024);
    let mut req = request(src.path(), out.path());
    req.image_mode = ImageMode::Native;
    req.passcode = "0123456789ABCDEF0123456789ABCDEF".into();
    let pkg = build(&req);
    let Err(e) = source::open(&pkg) else {
        panic!("a package made with another passcode must not open");
    };
    assert!(e.to_string().contains(pkg_source::RETAIL_MESSAGE), "{e}");
    // With its own passcode it reads.
    let tree = pkg_source::open(&pkg, &req.passcode).unwrap();
    assert!(!tree.files().is_empty());
}

#[test]
fn a_truncated_package_is_refused() {
    let src = TempDir::new("trunc-src");
    let out = TempDir::new("trunc-out");
    write_game(src.path(), 300 * 1024);
    let pkg = build(&request(src.path(), out.path()));
    let len = std::fs::metadata(&pkg).unwrap().len();
    for keep in [len / 2, len * 3 / 4, 0x10000 + 0x100, 0x80] {
        let cut = out.path().join(format!("cut-{keep}.pkg"));
        let mut bytes = std::fs::read(&pkg).unwrap();
        bytes.truncate(keep as usize);
        std::fs::write(&cut, bytes).unwrap();
        let Err(e) = source::open(&cut) else {
            panic!("a package cut to {keep} bytes must not open");
        };
        assert!(e.to_string().contains("incomplete"), "{keep}: {e}");
    }
}

/// A package whose image bytes were damaged is refused when the damaged block is read, not
/// handed out as if it were the game.
#[test]
fn a_damaged_block_is_an_error_not_bad_bytes() {
    let src = TempDir::new("damage-src");
    let out = TempDir::new("damage-out");
    write_game(src.path(), 300 * 1024);
    let mut req = request(src.path(), out.path());
    req.kraken = false;
    let pkg = build(&req);
    let mut bytes = std::fs::read(&pkg).unwrap();
    // The first outer block holds the start of the inner image (the keystone, then sce_sys).
    bytes[0x10000 + 0x10] ^= 0xFF;
    let bad = out.path().join("bad.pkg");
    std::fs::write(&bad, bytes).unwrap();
    let opened = source::open(&bad);
    let err = match opened {
        Err(e) => e.to_string(),
        Ok(mut tree) => tree.read(plan::KEYSTONE).unwrap_err().to_string(),
    };
    assert!(err.contains("damaged"), "{err}");
}

/// The extension is matched in any case, and Convert's check step accepts a package.
#[test]
fn a_package_is_a_source_convert_can_check() {
    let src = TempDir::new("inspect-src");
    let out = TempDir::new("inspect-out");
    write_game(src.path(), 300 * 1024);
    let pkg = build(&request(src.path(), out.path()));
    let upper = out.path().join("GAME.PKG");
    std::fs::rename(&pkg, &upper).unwrap();
    let inspection = build::inspect(&upper, out.path()).unwrap();
    assert_eq!(inspection.content_id.as_deref(), Some(CONTENT_ID));
    assert_eq!(
        inspection.files,
        source::open(&upper).unwrap().files().len()
    );
}

/// A package becomes an image (exFAT here) and another package through the existing writers,
/// and both hold the same tree.
#[test]
fn a_package_becomes_an_image_and_another_package() {
    let src = TempDir::new("convert-src");
    let out = TempDir::new("convert-out");
    write_game(src.path(), 300 * 1024);
    let pkg = build(&request(src.path(), out.path()));
    let mut tree = source::open(&pkg).unwrap();

    let image = out.path().join("game.exfat");
    ps5upload_fpkg::exfat_write::build_exfat(tree.as_mut(), &image, None, &mut |_, _| {}, &|| {
        false
    })
    .unwrap();
    let mut from_image = source::open(&image).unwrap();
    assert_eq!(from_image.files(), tree.files());
    assert_eq!(from_image.empty_dirs(), tree.empty_dirs());
    for f in tree.files().to_vec() {
        assert!(from_image.read(&f.path).unwrap() == tree.read(&f.path).unwrap());
    }

    let again = TempDir::new("convert-again");
    let mut req = request(&pkg, again.path());
    req.file_name = Some("again".into());
    let second = build(&req);
    let mut rebuilt = source::open(&second).unwrap();
    assert_eq!(rebuilt.files(), tree.files());
    assert_eq!(rebuilt.empty_dirs(), tree.empty_dirs());
    for f in tree.files().to_vec() {
        assert!(rebuilt.read(&f.path).unwrap() == tree.read(&f.path).unwrap());
    }
}

/// A large file streams: reading all of it in pieces holds a few blocks, never the file.
#[test]
fn a_large_file_streams_in_bounded_memory() {
    const BIG: usize = 40 << 20;
    for (name, kraken) in [("stream-flat", false), ("stream-blocks", true)] {
        let src = TempDir::new(&format!("{name}-src"));
        let out = TempDir::new(&format!("{name}-out"));
        write_game(src.path(), BIG);
        let mut req = request(src.path(), out.path());
        req.kraken = kraken;
        req.level = Level::Fast;
        let pkg = build(&req);
        let mut tree = pkg_source::open(&pkg, DEFAULT_PASSCODE).unwrap();
        let want = std::fs::read(src.path().join("data/big.bin")).unwrap();
        let mut at = 0usize;
        let mut peak = 0usize;
        while at < want.len() {
            let piece = tree.read_range("data/big.bin", at as u64, 1 << 20).unwrap();
            assert!(piece == want[at..at + piece.len()], "{name} at {at}");
            at += piece.len();
            peak = peak.max(tree.cached_bytes());
        }
        assert!(
            peak <= 2 << 20,
            "{name}: {peak} bytes cached for a {BIG}-byte file"
        );
        let _: &PkgSource = &tree;
    }
}
