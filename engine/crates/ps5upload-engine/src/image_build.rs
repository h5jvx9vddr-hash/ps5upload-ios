//! Game images from a game folder: `.exfat`, `.ffpkg` (UFS2) or `.ffpfs` (PFS), optionally
//! compressed into a `.ffpfsc` as they are written.
//!
//! Who writes what. exFAT is ps5upload's own writer (`ps5upload_fpkg::exfat_write`). UFS2 and
//! PFS are quer3q's, from PS5 Dump Forge (`ps5-dump-forge-ufs2`, `ps5-dump-forge-pfs`): each
//! reproduces the layout of the reference tool (newfs/makefs, MkPFS), which is the thing that
//! matters for a filesystem the console mounts. The `.ffpfsc` container a folder is compressed
//! into is Dump Forge's streaming one, so the image never exists uncompressed on disk;
//! compressing an image file that already exists stays with our own `ps5upload_fpkg::ffpfsc`.
//!
//! The ideas taken from Dump Forge, implemented here: every image is planned (names checked,
//! size known) before a byte is written; it is written to a `.partial` and read back through
//! its own reader before it gets its name; and the read-back compares every file's BLAKE3
//! hash, not only the list of names and sizes. The source is hashed while the writer reads it,
//! so it is normally read once.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ps5upload_fpkg::source::{SourceFile, SourceTree};

/// The filesystems a game image can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Exfat,
    /// UFS2: what ShadowMountPlus recommends.
    Ffpkg,
    /// PFS: experimental in ShadowMountPlus 1.7; ASCII names only.
    Ffpfs,
}

impl ImageFormat {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("exfat") => Ok(Self::Exfat),
            Some("ffpkg") | Some("ufs") | Some("ufs2") => Ok(Self::Ffpkg),
            Some("ffpfs") | Some("pfs") => Ok(Self::Ffpfs),
            Some(other) => Err(format!(
                "unknown image format {other:?}: use exfat, ffpkg or ffpfs"
            )),
        }
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Exfat => "exfat",
            Self::Ffpkg => "ffpkg",
            Self::Ffpfs => "ffpfs",
        }
    }
}

/// ShadowMountPlus does not mount a PFS-based image (`.ffpfs`, `.ffpfsc`) whose file name is
/// longer than this, in bytes.
pub(crate) const PFS_NAME_MAX: usize = 63;

/// A safe file stem from a game folder's name.
/// Image files a build may read as its source (an .exfat into a .ffpkg, say).
pub(crate) fn is_image_file(path: &Path) -> bool {
    path.is_file()
        && path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "exfat" | "ffpkg" | "ufs2" | "ffpfs" | "ffpfsc"
            )
        })
}

fn stem_of(source: &Path) -> String {
    let name = source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // An image source names its output after itself without the image extension.
    let name = match name.rsplit_once('.') {
        Some((base, ext))
            if !base.is_empty()
                && matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "exfat" | "ffpkg" | "ufs2" | "ffpfs" | "ffpfsc"
                ) =>
        {
            base.to_string()
        }
        _ => name,
    };
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "\"*/:<>?\\|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = stem.trim().trim_matches('.').to_string();
    if stem.is_empty() {
        "game".into()
    } else {
        stem
    }
}

/// `stem.ext`, the stem cut at a character boundary so the whole name fits `max` bytes.
fn fit(stem: &str, ext: &str, max: Option<usize>) -> String {
    let mut stem = stem.to_string();
    if let Some(max) = max {
        let room = max.saturating_sub(ext.len() + 1);
        while stem.len() > room {
            stem.pop();
        }
        let trimmed = stem.trim_end().trim_end_matches('.').to_string();
        stem = if trimmed.is_empty() {
            "game".into()
        } else {
            trimmed
        };
    }
    format!("{stem}.{ext}")
}

/// The output file name, and for a `.ffpfsc` the name the image carries inside it (which
/// tells ShadowMountPlus its filesystem).
pub(crate) fn output_names(
    source: &Path,
    format: ImageFormat,
    compress: bool,
) -> (String, Option<String>) {
    let stem = stem_of(source);
    if compress {
        let outer = fit(&stem, "ffpfsc", Some(PFS_NAME_MAX));
        let inner = fit(&stem, format.extension(), Some(PFS_NAME_MAX));
        (outer, Some(inner))
    } else {
        let max = (format == ImageFormat::Ffpfs).then_some(PFS_NAME_MAX);
        (fit(&stem, format.extension(), max), None)
    }
}

/// Wraps a source and hashes each file as a writer reads it front to back, so verifying does
/// not read the source a second time. A file read any other way is hashed afterwards.
struct HashingTree<'a> {
    inner: &'a mut dyn SourceTree,
    sizes: HashMap<String, u64>,
    in_flight: HashMap<String, (u64, blake3::Hasher)>,
    done: HashMap<String, blake3::Hash>,
}

impl<'a> HashingTree<'a> {
    fn new(inner: &'a mut dyn SourceTree) -> Self {
        let sizes = inner
            .files()
            .iter()
            .map(|f| (f.path.clone(), f.size))
            .collect();
        Self {
            inner,
            sizes,
            in_flight: HashMap::new(),
            done: HashMap::new(),
        }
    }

    fn observe(&mut self, path: &str, offset: u64, bytes: &[u8]) {
        let Some(&size) = self.sizes.get(path) else {
            return;
        };
        if offset == 0 {
            self.in_flight
                .insert(path.to_string(), (0, blake3::Hasher::new()));
        }
        let finished = match self.in_flight.get_mut(path) {
            Some((next, hasher)) if *next == offset => {
                hash_update(hasher, bytes);
                *next += bytes.len() as u64;
                *next >= size
            }
            // Out of order: forget it, it is hashed afterwards.
            Some(_) => {
                self.in_flight.remove(path);
                return;
            }
            None => return,
        };
        if finished {
            if let Some((_, h)) = self.in_flight.remove(path) {
                self.done.insert(path.to_string(), h.finalize());
            }
        }
    }

    /// Every file's hash: the ones seen whole while writing, the rest read now.
    fn hashes(mut self, cancel: &AtomicBool) -> Result<HashMap<String, blake3::Hash>, String> {
        let missing: Vec<(String, u64)> = self
            .sizes
            .iter()
            .filter(|(p, _)| !self.done.contains_key(*p))
            .map(|(p, s)| (p.clone(), *s))
            .collect();
        for (path, size) in missing {
            let h = hash_file(self.inner, &path, size, DEFAULT_VERIFY_CHUNK, cancel)?;
            self.done.insert(path, h);
        }
        Ok(self.done)
    }
}

impl SourceTree for HashingTree<'_> {
    fn files(&self) -> &[SourceFile] {
        self.inner.files()
    }
    fn read(&mut self, path: &str) -> ps5upload_fpkg::Result<Vec<u8>> {
        let all = self.inner.read(path)?;
        self.observe(path, 0, &all);
        Ok(all)
    }
    fn read_range(
        &mut self,
        path: &str,
        offset: u64,
        len: usize,
    ) -> ps5upload_fpkg::Result<Vec<u8>> {
        let bytes = self.inner.read_range(path, offset, len)?;
        self.observe(path, offset, &bytes);
        Ok(bytes)
    }
    fn empty_dirs(&self) -> &[String] {
        self.inner.empty_dirs()
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// How much of a file the read-back check reads per call when the caller does not say.
/// Measured on a 10 GB exFAT image in page cache: reads alone go 7 GiB/s at 1–8 MiB, hashing
/// on all cores peaks at 8 MiB (3.7 GiB/s against 1.3 single-threaded); 1–32 MiB are within
/// 15% of each other, and the FAT-walk fix in the exFAT reader mattered far more.
pub(crate) const DEFAULT_VERIFY_CHUNK: u64 = 8 << 20;
/// What a request may ask for, in KiB: 1 MiB to 64 MiB.
pub(crate) const VERIFY_CHUNK_KIB_RANGE: std::ops::RangeInclusive<u32> = 1024..=65536;

/// The read-back chunk a request asks for (`verify_chunk_kib`), or the default.
pub(crate) fn verify_chunk(kib: Option<u32>) -> Result<u64, String> {
    match kib {
        None => Ok(DEFAULT_VERIFY_CHUNK),
        Some(k) if VERIFY_CHUNK_KIB_RANGE.contains(&k) => Ok(u64::from(k) << 10),
        Some(k) => Err(format!(
            "verify_chunk_kib {k} is not {} to {}",
            VERIFY_CHUNK_KIB_RANGE.start(),
            VERIFY_CHUNK_KIB_RANGE.end()
        )),
    }
}

/// Feeds `bytes` to a hasher, on every core when there is enough to split (BLAKE3's own
/// advice: multi-threading pays from about 128 KiB).
fn hash_update(h: &mut blake3::Hasher, bytes: &[u8]) {
    if bytes.len() >= 128 << 10 {
        h.update_rayon(bytes);
    } else {
        h.update(bytes);
    }
}

fn hash_file(
    tree: &mut dyn SourceTree,
    path: &str,
    size: u64,
    chunk: u64,
    cancel: &AtomicBool,
) -> Result<blake3::Hash, String> {
    let mut h = blake3::Hasher::new();
    let mut at = 0u64;
    while at < size {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let want = (size - at).min(chunk) as usize;
        let buf = tree
            .read_range(path, at, want)
            .map_err(|e| format!("{path}: {e}"))?;
        if buf.is_empty() {
            return Err(format!("{path} ended at {at} of its {size} bytes"));
        }
        hash_update(&mut h, &buf);
        at += buf.len() as u64;
    }
    Ok(h.finalize())
}

/// Opens a finished image through the reader for its kind.
fn open_image(
    path: &Path,
    format: ImageFormat,
    compressed: bool,
) -> Result<Box<dyn SourceTree>, String> {
    let label = path.display().to_string();
    let opened: ps5upload_fpkg::Result<Box<dyn SourceTree>> = if compressed {
        std::fs::File::open(path)
            .map_err(ps5upload_fpkg::Error::from)
            .and_then(|f| {
                ps5_dump_forge_pfs::open_ffpfsc(Box::new(std::io::BufReader::new(f)), &label)
                    .map(|(tree, _)| tree)
            })
    } else {
        match format {
            ImageFormat::Exfat => ps5upload_fpkg::exfat::ExFatSource::open(path)
                .map(|t| Box::new(t) as Box<dyn SourceTree>),
            ImageFormat::Ffpkg => ps5upload_fpkg::ufs2_source::Ufs2Source::open(path)
                .map(|t| Box::new(t) as Box<dyn SourceTree>),
            ImageFormat::Ffpfs => ps5_dump_forge_pfs::PfsSource::open(path)
                .map(|t| Box::new(t) as Box<dyn SourceTree>),
        }
    };
    opened.map_err(|e| format!("the image could not be read back: {e}"))
}

/// Reads the image back and compares it with the source: the same files at the same sizes,
/// the same empty folders, and every file's BLAKE3 hash. `chunk` bytes per read.
fn verify(
    image: &mut dyn SourceTree,
    want: &HashMap<String, blake3::Hash>,
    want_sizes: &HashMap<String, u64>,
    want_empty: &[String],
    chunk: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    let got: Vec<SourceFile> = image.files().to_vec();
    if got.len() != want_sizes.len() {
        return Err(format!(
            "the image read back with {} files where the folder has {}",
            got.len(),
            want_sizes.len()
        ));
    }
    let mut got_empty: Vec<String> = image
        .empty_dirs()
        .iter()
        .map(|d| d.trim_matches('/').to_string())
        .collect();
    let mut want_empty: Vec<String> = want_empty
        .iter()
        .map(|d| d.trim_matches('/').to_string())
        .collect();
    got_empty.sort();
    want_empty.sort();
    if got_empty != want_empty {
        return Err(format!(
            "the image read back with empty folders {got_empty:?} where the folder has {want_empty:?}"
        ));
    }
    let total: u64 = want_sizes.values().sum();
    let mut done = 0u64;
    progress(0, total);
    for f in &got {
        let Some(&size) = want_sizes.get(&f.path) else {
            return Err(format!(
                "the image holds {} which the folder does not",
                f.path
            ));
        };
        if size != f.size {
            return Err(format!(
                "{} is {} bytes in the image and {size} in the folder",
                f.path, f.size
            ));
        }
        let h = hash_file(image, &f.path, f.size, chunk, cancel)?;
        if Some(&h) != want.get(&f.path) {
            return Err(format!(
                "{} differs between the image and the folder",
                f.path
            ));
        }
        done += f.size;
        progress(done, total);
    }
    Ok(())
}

/// A libSceAmpr game's folder with its `ampr_emu.index` regenerated from the files that go
/// into the image (#449). The index ampr_emu reads must list every file at its size; a dump's
/// copy is often stale (files added, trimmed or resized since), and a plain image used to
/// carry it as it was, which crashes the game once it touches a file the index has wrong.
/// LZ4 builds regenerate theirs in `ampr_pack::image::prepare`; this is the plain build's.
pub(crate) struct AmprIndexed<'a> {
    inner: &'a mut dyn SourceTree,
    files: Vec<SourceFile>,
    index: Vec<u8>,
}

impl<'a> AmprIndexed<'a> {
    /// `None` when the game does not read through libSceAmpr and ships no index: nothing to
    /// regenerate, the folder goes in as it is.
    pub(crate) fn wrap(inner: &'a mut dyn SourceTree) -> Result<Option<Self>, String> {
        let name = ps5upload_fpkg::ampr_index::NAME;
        let had = inner
            .files()
            .iter()
            .any(|f| f.path.eq_ignore_ascii_case(name));
        if !had && !ps5upload_fpkg::source::imports_ampr(inner, "eboot.bin") {
            return Ok(None);
        }
        let mut files: Vec<SourceFile> = inner
            .files()
            .iter()
            .filter(|f| !f.path.eq_ignore_ascii_case(name))
            .cloned()
            .collect();
        let listed: Vec<(String, u64)> = files.iter().map(|f| (f.path.clone(), f.size)).collect();
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let index = ps5upload_fpkg::ampr_index::build(&listed, mtime)?;
        files.push(SourceFile {
            path: name.to_string(),
            size: index.len() as u64,
        });
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Some(Self {
            inner,
            files,
            index,
        }))
    }
}

impl SourceTree for AmprIndexed<'_> {
    fn files(&self) -> &[SourceFile] {
        &self.files
    }
    fn empty_dirs(&self) -> &[String] {
        self.inner.empty_dirs()
    }
    fn read(&mut self, path: &str) -> ps5upload_fpkg::Result<Vec<u8>> {
        if path.eq_ignore_ascii_case(ps5upload_fpkg::ampr_index::NAME) {
            return Ok(self.index.clone());
        }
        self.inner.read(path)
    }
    fn read_range(
        &mut self,
        path: &str,
        offset: u64,
        len: usize,
    ) -> ps5upload_fpkg::Result<Vec<u8>> {
        if path.eq_ignore_ascii_case(ps5upload_fpkg::ampr_index::NAME) {
            let start = (offset as usize).min(self.index.len());
            let end = start.saturating_add(len).min(self.index.len());
            return Ok(self.index[start..end].to_vec());
        }
        self.inner.read_range(path, offset, len)
    }
    fn describe(&self) -> String {
        format!("{} (ampr_emu.index regenerated)", self.inner.describe())
    }
}

/// What a build wrote.
pub(crate) struct ImageBuilt {
    pub image_bytes: u64,
    pub files: u64,
    pub detail: String,
}

/// A writer for one planned image, so the same code fills a file or a `.ffpfsc` stream.
enum Planned {
    Exfat(ps5upload_fpkg::exfat_write::ExfatPlan),
    Ffpkg(ps5_dump_forge_ufs2::Layout),
    Ffpfs(ps5_dump_forge_pfs::Layout),
}

impl Planned {
    fn plan(
        format: ImageFormat,
        tree: &dyn SourceTree,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let e = |e: ps5upload_fpkg::Error| e.to_string();
        Ok(match format {
            ImageFormat::Exfat => {
                Self::Exfat(ps5upload_fpkg::exfat_write::plan_exfat(tree, None, 0).map_err(e)?)
            }
            ImageFormat::Ffpkg => Self::Ffpkg(
                ps5_dump_forge_ufs2::plan(tree, &Default::default(), cancel).map_err(e)?,
            ),
            ImageFormat::Ffpfs => {
                Self::Ffpfs(ps5_dump_forge_pfs::plan(tree, &Default::default(), cancel).map_err(e)?)
            }
        })
    }

    fn image_size(&self) -> u64 {
        match self {
            Self::Exfat(p) => p.image_bytes,
            Self::Ffpkg(l) => l.image_size,
            Self::Ffpfs(l) => l.image_size,
        }
    }

    fn free_bytes(&self) -> Option<u64> {
        match self {
            Self::Exfat(p) => Some(p.free_bytes),
            Self::Ffpkg(l) => Some(l.free_bytes),
            Self::Ffpfs(_) => None,
        }
    }

    fn write<W: Write + std::io::Seek>(
        &self,
        tree: &mut dyn SourceTree,
        out: &mut W,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> ps5upload_fpkg::Result<u64> {
        match self {
            Self::Exfat(p) => {
                ps5upload_fpkg::exfat_write::write_exfat(p, tree, out, progress, &|| {
                    cancel.load(Ordering::Relaxed)
                })
            }
            Self::Ffpkg(l) => {
                ps5_dump_forge_ufs2::write(tree, l, out, cancel, progress).map(|r| r.image_size)
            }
            Self::Ffpfs(l) => {
                ps5_dump_forge_pfs::write(tree, l, out, cancel, progress).map(|r| r.image_size)
            }
        }
    }
}

/// Builds `source` as an image at `output`: planned, written to `<output>.partial` (straight
/// into a `.ffpfsc` container when `inner_name` is given), read back and hash-checked, then
/// renamed. `stage(id, done, total)` reports `plan`, `write` (or `compress`) and `verify`.
/// `verify_chunk`: how much the read-back check reads per call (see [`verify_chunk`]).
pub(crate) fn build(
    format: ImageFormat,
    inner_name: Option<&str>,
    source: &mut dyn SourceTree,
    output: &Path,
    verify_chunk: u64,
    cancel: &AtomicBool,
    stage: &mut dyn FnMut(&str, u64, u64),
) -> Result<ImageBuilt, String> {
    if source.files().is_empty() {
        return Err(format!("{} has no files in it", source.describe()));
    }
    stage("plan", 0, 0);
    let planned = Planned::plan(format, &*source, cancel)?;
    let want_sizes: HashMap<String, u64> = source
        .files()
        .iter()
        .map(|f| (f.path.clone(), f.size))
        .collect();
    let want_empty: Vec<String> = source.empty_dirs().to_vec();
    let data: u64 = want_sizes.values().sum();

    // The caller has claimed this name (`fpkg_api::Writing`), clearing any leftover; a file
    // here now is another build's, and `create_new` refuses it rather than wreck it.
    let partial = PathBuf::from(format!("{}.partial", output.display()));
    // Opened before the cleanup below applies: a file that is not ours is never removed.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(|e| format!("{}: {e}", partial.display()))?;
    let result = (|| -> Result<ImageBuilt, String> {
        let mut hashing = HashingTree::new(source);
        let write_stage = if inner_name.is_some() {
            "compress"
        } else {
            "write"
        };
        let mut report = |done: u64, total: u64| stage(write_stage, done, total);
        let (image_bytes, stored) = match inner_name {
            None => {
                let mut out = BufWriter::with_capacity(8 << 20, &mut file);
                let n = planned
                    .write(&mut hashing, &mut out, cancel, &mut report)
                    .map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
                (n, n)
            }
            Some(name) => {
                let (n, wrapped) = ps5_dump_forge_pfs::wrap(
                    name,
                    planned.image_size(),
                    &mut file,
                    &ps5_dump_forge_pfs::WrapOptions::default(),
                    cancel,
                    |s| planned.write(&mut hashing, s, cancel, &mut report),
                )
                .map_err(|e| e.to_string())?;
                (n, wrapped.image_size)
            }
        };
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        let hashes = hashing.hashes(cancel)?;

        let mut image = open_image(&partial, format, inner_name.is_some())?;
        verify(
            &mut *image,
            &hashes,
            &want_sizes,
            &want_empty,
            verify_chunk,
            cancel,
            &mut |d, t| stage("verify", d, t),
        )?;
        drop(image);
        std::fs::rename(&partial, output).map_err(|e| e.to_string())?;
        let mut detail = format!(
            "{}, {} files, {data} data bytes, hash-checked",
            format.extension(),
            want_sizes.len()
        );
        if let Some(free) = planned.free_bytes() {
            detail.push_str(&format!(", {free} bytes free"));
        }
        if inner_name.is_some() {
            detail.push_str(&format!(", {image_bytes} bytes before compression"));
        }
        Ok(ImageBuilt {
            image_bytes: stored,
            files: want_sizes.len() as u64,
            detail,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    settle(result, cancel.load(Ordering::Relaxed))
}

/// A failed build that was cancelled says so; a cancel that came after the image was
/// finished and renamed changes nothing.
fn settle(result: Result<ImageBuilt, String>, cancelled: bool) -> Result<ImageBuilt, String> {
    match result {
        Err(_) if cancelled => Err("cancelled".into()),
        r => r,
    }
}

/// AMPR LZ4 asset packs for an image (see `ps5upload_fpkg::ampr_pack`): the game's files
/// packed into seekable LZ4 volumes that a backport's `ampr_emu` serves, the originals left out.
pub(crate) struct Lz4Packs {
    /// drakmor's profile format; the built-in profile (from `level` and `block_shift`) when
    /// absent.
    pub profile: Option<ps5upload_fpkg::ampr_pack::Config>,
    /// 1 (fastest) to 12 (smallest).
    pub level: u8,
    /// log2 of the block size, 14 (16 KiB) to 20 (1 MiB).
    pub block_shift: u8,
}

/// The spool an LZ4 build packs into, next to the image it is for.
pub(crate) fn lz4_spool(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lz4spool", output.display()))
}

/// [`build`], with the game's files packed first. Only exFAT: the packs are already
/// compressed, so the `.ffpfsc` container has nothing left to gain. The volumes are spooled
/// beside the output and removed once the image is written and read back, or has failed.
/// `stage` reports `pack` before the image's own stages.
pub(crate) fn build_packed(
    format: ImageFormat,
    source: &mut dyn SourceTree,
    output: &Path,
    packs: &Lz4Packs,
    verify_chunk: u64,
    cancel: &AtomicBool,
    stage: &mut dyn FnMut(&str, u64, u64),
) -> Result<ImageBuilt, String> {
    use ps5upload_fpkg::ampr_pack;
    if format != ImageFormat::Exfat {
        return Err("LZ4 asset packs are written into exFAT images only".into());
    }
    if let Some(why) = ampr_pack::image::refusal(source) {
        return Err(why);
    }
    let config = packs
        .profile
        .clone()
        .unwrap_or_else(|| ampr_pack::default_profile(packs.level, packs.block_shift));
    // Claimed by the caller like the image's `.partial`, so nothing else writes here.
    let spool = lz4_spool(output);
    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    stage("pack", 0, 0);
    let prepared = {
        let mut progress = |done: u64, total: u64| stage("pack", done, total);
        let mut control = ampr_pack::Control {
            progress: Some(&mut progress),
            cancel: Some(cancel),
        };
        ampr_pack::image::prepare(source, &config, &spool, mtime, &mut control)
    };
    let result = match prepared {
        Ok((mut overlay, report)) => build(
            format,
            None,
            &mut overlay,
            output,
            verify_chunk,
            cancel,
            stage,
        )
        .map(|b| (b, report)),
        Err(e) => Err(e.to_string()),
    };
    let _ = std::fs::remove_dir_all(&spool);
    let (mut built, report) = result?;
    let s = &report.stats;
    built.detail.push_str(&format!(
        ", LZ4 packs: {} files packed into {} volume(s) ({} -> {} bytes, {} LZ4 / {} raw \
         chunks), {} loose",
        s.files_packed,
        report.volumes.len(),
        s.logical_bytes,
        s.stored_bytes,
        s.chunks_lz4,
        s.chunks_raw,
        s.files_loose
    ));
    Ok(built)
}

#[cfg(test)]
mod tests {
    #[test]
    fn verify_chunk_takes_1_to_64_mib_in_kib_and_defaults_to_8() {
        assert_eq!(verify_chunk(None).unwrap(), 8 << 20);
        assert_eq!(verify_chunk(Some(1024)).unwrap(), 1 << 20);
        assert_eq!(verify_chunk(Some(65536)).unwrap(), 64 << 20);
        assert!(verify_chunk(Some(512))
            .unwrap_err()
            .contains("1024 to 65536"));
        assert!(verify_chunk(Some(0)).is_err());
        assert!(verify_chunk(Some(1 << 20)).is_err());
    }

    use super::*;
    use ps5upload_fpkg::source::FolderSource;

    #[test]
    fn formats_parse_and_name_their_files() {
        assert_eq!(ImageFormat::parse(None), Ok(ImageFormat::Exfat));
        assert_eq!(ImageFormat::parse(Some("FFPKG")), Ok(ImageFormat::Ffpkg));
        assert_eq!(ImageFormat::parse(Some("pfs")), Ok(ImageFormat::Ffpfs));
        assert!(ImageFormat::parse(Some("iso")).is_err());
        let src = Path::new("/games/PPSA01234-app");
        assert_eq!(
            output_names(src, ImageFormat::Ffpkg, false),
            ("PPSA01234-app.ffpkg".into(), None)
        );
        assert_eq!(
            output_names(Path::new("/games/What: a game?"), ImageFormat::Exfat, false).0,
            "What_ a game_.exfat"
        );
        assert_eq!(
            output_names(Path::new("/"), ImageFormat::Exfat, false).0,
            "game.exfat"
        );
        // An image source: the output carries its name, not its old extension.
        assert_eq!(
            output_names(Path::new("/g/PPSA29343.exfat"), ImageFormat::Ffpkg, false).0,
            "PPSA29343.ffpkg"
        );
        assert_eq!(
            output_names(src, ImageFormat::Exfat, true),
            (
                "PPSA01234-app.ffpfsc".into(),
                Some("PPSA01234-app.exfat".into())
            )
        );
    }

    #[test]
    fn pfs_based_names_fit_what_shadowmount_mounts() {
        let long =
            Path::new("/g/A very long game folder name that goes on and on past sixty-three bytes");
        let (pfs, _) = output_names(long, ImageFormat::Ffpfs, false);
        assert!(
            pfs.len() <= PFS_NAME_MAX && pfs.ends_with(".ffpfs"),
            "{pfs}"
        );
        let (outer, inner) = output_names(long, ImageFormat::Ffpkg, true);
        assert!(
            outer.len() <= PFS_NAME_MAX && outer.ends_with(".ffpfsc"),
            "{outer}"
        );
        assert!(inner.unwrap().ends_with(".ffpkg"));
        // A UFS2 or exFAT image is not held to it.
        let (ufs, _) = output_names(long, ImageFormat::Ffpkg, false);
        assert!(ufs.len() > PFS_NAME_MAX);
        // Multi-byte characters are never cut in half.
        let wide = Path::new("/g/ゲームゲームゲームゲームゲームゲームゲームゲームゲームゲーム");
        let (w, _) = output_names(wide, ImageFormat::Ffpfs, false);
        assert!(w.len() <= PFS_NAME_MAX && w.ends_with(".ffpfs"), "{w}");
    }

    fn game(root: &Path) -> PathBuf {
        let game = root.join("PPSA99999-app");
        std::fs::create_dir_all(game.join("sce_sys")).unwrap();
        std::fs::create_dir_all(game.join("data/sub")).unwrap();
        std::fs::create_dir_all(game.join("empty")).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(game.join("eboot.bin"), &big).unwrap();
        std::fs::write(
            game.join("sce_sys/param.json"),
            b"{\"titleId\":\"PPSA99999\"}",
        )
        .unwrap();
        std::fs::write(game.join("data/sub/a.bin"), b"").unwrap();
        std::fs::write(game.join("data/zeros.bin"), vec![0u8; 200_000]).unwrap();
        game
    }

    /// Every format, plain and compressed, writes a folder that reads back hash for hash.
    #[test]
    fn every_format_round_trips_plain_and_compressed() {
        let root = std::env::temp_dir().join(format!("ps5upload-imgb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = game(&root);
        for format in [ImageFormat::Exfat, ImageFormat::Ffpkg, ImageFormat::Ffpfs] {
            for compress in [false, true] {
                let (name, inner) = output_names(&src, format, compress);
                let out = root.join(&name);
                let mut tree = FolderSource::open(&src).unwrap();
                let cancel = AtomicBool::new(false);
                let mut stages: Vec<String> = Vec::new();
                let built = build(
                    format,
                    inner.as_deref(),
                    &mut tree,
                    &out,
                    DEFAULT_VERIFY_CHUNK,
                    &cancel,
                    &mut |s, _, _| {
                        if stages.last().map(String::as_str) != Some(s) {
                            stages.push(s.to_string());
                        }
                    },
                )
                .unwrap_or_else(|e| panic!("{format:?} compress={compress}: {e}"));
                assert_eq!(built.files, 4, "{format:?}");
                let write = if compress { "compress" } else { "write" };
                assert_eq!(
                    stages,
                    ["plan", write, "verify"],
                    "{format:?} compress={compress}"
                );
                assert!(out.is_file() && !root.join(format!("{name}.partial")).exists());
                let _ = std::fs::remove_file(&out);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Another build's `.partial` under the same name is neither written into nor removed;
    /// and a cancel that lands after the image was finished does not turn it into a failure.
    #[test]
    fn another_builds_partial_is_left_alone_and_a_late_cancel_changes_nothing() {
        let root = std::env::temp_dir().join(format!("ps5upload-imgp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = game(&root);
        let out = root.join("x.exfat");
        let partial = root.join("x.exfat.partial");
        std::fs::write(&partial, b"theirs").unwrap();
        let mut tree = FolderSource::open(&src).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(build(
            ImageFormat::Exfat,
            None,
            &mut tree,
            &out,
            DEFAULT_VERIFY_CHUNK,
            &cancel,
            &mut |_, _, _| {}
        )
        .is_err());
        assert_eq!(std::fs::read(&partial).unwrap(), b"theirs");
        std::fs::remove_file(&partial).unwrap();

        let done = || ImageBuilt {
            image_bytes: 1,
            files: 1,
            detail: String::new(),
        };
        assert!(settle(Ok(done()), true).is_ok());
        assert_eq!(
            settle(Err("disk full".into()), true).err().unwrap(),
            "cancelled"
        );
        assert_eq!(
            settle(Err("disk full".into()), false).err().unwrap(),
            "disk full"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file that is not read front to back while the image is written is still checked.
    #[test]
    fn a_file_read_out_of_order_is_hashed_afterwards() {
        let root = std::env::temp_dir().join(format!("ps5upload-imgh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = game(&root);
        let mut tree = FolderSource::open(&src).unwrap();
        let cancel = AtomicBool::new(false);
        let mut h = HashingTree::new(&mut tree);
        let _ = h.read_range("eboot.bin", 1000, 10).unwrap();
        let _ = h.read("sce_sys/param.json").unwrap();
        let all = h.hashes(&cancel).unwrap();
        let mut again = FolderSource::open(&src).unwrap();
        let direct = hash_file(
            &mut again,
            "eboot.bin",
            300_000,
            DEFAULT_VERIFY_CHUNK,
            &cancel,
        )
        .unwrap();
        assert_eq!(all["eboot.bin"], direct);
        assert_eq!(all.len(), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A PFS image holds ASCII names only; the build refuses before writing anything.
    #[test]
    fn a_name_pfs_cannot_hold_is_refused_before_writing() {
        let root = std::env::temp_dir().join(format!("ps5upload-imgn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = game(&root);
        std::fs::write(src.join("données.bin"), b"x").unwrap();
        let mut tree = FolderSource::open(&src).unwrap();
        let out = root.join("x.ffpfs");
        let e = build(
            ImageFormat::Ffpfs,
            None,
            &mut tree,
            &out,
            DEFAULT_VERIFY_CHUNK,
            &AtomicBool::new(false),
            &mut |_, _, _| {},
        )
        .err()
        .expect("refused");
        assert!(e.contains("données.bin"), "{e}");
        assert!(!out.exists() && !root.join("x.ffpfs.partial").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A backported AMPR title: its eboot imports libSceAmpr and its fakelib carries a
    /// pack-capable ampr_emu.
    fn ampr_game(root: &Path) -> PathBuf {
        let src = game(root);
        let mut eboot: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        eboot[1000..1014].copy_from_slice(b"libSceAmpr.prx");
        std::fs::write(src.join("eboot.bin"), eboot).unwrap();
        std::fs::create_dir_all(src.join("fakelib")).unwrap();
        let mut module = vec![0u8; 4096];
        module[100..108].copy_from_slice(b"AMPRPAK4");
        module[200..217].copy_from_slice(b"ampr_assets.index");
        module[300..321].copy_from_slice(b"\x000.4.2.1 (c) Drakmor\0");
        std::fs::write(src.join("fakelib/libSceAmpr.sprx"), module).unwrap();
        let text: Vec<u8> = (0..400_000u32)
            .flat_map(|i| format!("asset {} ", i % 977).into_bytes())
            .take(400_000)
            .collect();
        std::fs::create_dir_all(src.join("data/only_packed")).unwrap();
        std::fs::write(src.join("data/only_packed/table.bin"), &text).unwrap();
        std::fs::write(src.join("data/text.bin"), &text[..150_000]).unwrap();
        std::fs::write(src.join("data/settings.json"), b"{\"a\":1}").unwrap();
        src
    }

    /// An LZ4-packed exFAT image, read back through the exFAT reader and then the pack reader,
    /// gives every packed file's source bytes; the packed originals are not in the image, the
    /// loose files are, and the regenerated ampr_emu.index lists the original files.
    #[test]
    fn an_lz4_packed_exfat_reads_back_to_the_source() {
        use ps5upload_fpkg::ampr_pack::Reader;
        let root = std::env::temp_dir().join(format!("ps5upload-imgl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = ampr_game(&root);
        let out = root.join("PPSA99999-app.exfat");
        let mut tree = FolderSource::open(&src).unwrap();
        let cancel = AtomicBool::new(false);
        let mut stages: Vec<String> = Vec::new();
        let built = build_packed(
            ImageFormat::Exfat,
            &mut tree,
            &out,
            &Lz4Packs {
                profile: None,
                level: 9,
                block_shift: 16,
            },
            DEFAULT_VERIFY_CHUNK,
            &cancel,
            &mut |s, _, _| {
                if stages.last().map(String::as_str) != Some(s) {
                    stages.push(s.to_string());
                }
            },
        )
        .unwrap();
        assert_eq!(stages, ["pack", "plan", "write", "verify"]);
        assert!(built.detail.contains("LZ4 packs"), "{}", built.detail);
        assert!(!lz4_spool(&out).exists(), "the spool is removed");

        let mut image = ps5upload_fpkg::exfat::ExFatSource::open(&out).unwrap();
        let in_image: Vec<String> = image.files().iter().map(|f| f.path.clone()).collect();
        let reader = Reader::open(&mut image, "ampr_assets.index").unwrap();
        let mut source = FolderSource::open(&src).unwrap();
        let mut packed = 0;
        for f in source.files().to_vec() {
            let i = reader
                .find(&f.path)
                .expect("every source file is in the manifest");
            let want = source.read(&f.path).unwrap();
            if reader.manifest.files[i].packed() {
                packed += 1;
                assert!(
                    !in_image.contains(&f.path),
                    "{} is packed and loose",
                    f.path
                );
                assert_eq!(
                    reader.read_file(i, &mut image, "").unwrap(),
                    want,
                    "{}",
                    f.path
                );
            } else {
                assert_eq!(image.read(&f.path).unwrap(), want, "{}", f.path);
            }
        }
        assert!(packed >= 3, "data files are packed ({packed})");
        // The default profile keeps the module, system files and text config loose.
        for loose in [
            "eboot.bin",
            "fakelib/libSceAmpr.sprx",
            "sce_sys/param.json",
            "data/settings.json",
        ] {
            assert!(in_image.contains(&loose.to_string()), "{loose}");
        }
        // A folder packing emptied is still there.
        assert!(image
            .empty_dirs()
            .iter()
            .any(|d| d.trim_matches('/') == "data/only_packed"));
        let index =
            ps5upload_fpkg::ampr_index::parse(&image.read("ampr_emu.index").unwrap()).unwrap();
        assert_eq!(index.len(), source.files().len());
        assert!(index
            .iter()
            .any(|e| e.path == "data/only_packed/table.bin" && e.size == 400_000));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A plain image of a libSceAmpr game carries a fresh index listing the image's files
    /// (#449), never the dump's stale one.
    #[test]
    fn a_plain_image_regenerates_a_stale_ampr_index() {
        let root = std::env::temp_dir().join(format!("ps5upload-imgi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = ampr_game(&root);
        std::fs::write(src.join("ampr_emu.index"), b"AMPRIDX3 stale").unwrap();
        let out = root.join("plain.exfat");
        let mut tree = FolderSource::open(&src).unwrap();
        let files_in_source = tree.files().len();
        let mut indexed = AmprIndexed::wrap(&mut tree).unwrap().expect("an AMPR game");
        build(
            ImageFormat::Exfat,
            None,
            &mut indexed,
            &out,
            DEFAULT_VERIFY_CHUNK,
            &AtomicBool::new(false),
            &mut |_, _, _| {},
        )
        .unwrap();
        let mut image = open_image(&out, ImageFormat::Exfat, false).unwrap();
        let index =
            ps5upload_fpkg::ampr_index::parse(&image.read("ampr_emu.index").unwrap()).unwrap();
        // Every file but the index itself, each at its size.
        assert_eq!(index.len(), files_in_source - 1);
        assert!(index
            .iter()
            .any(|e| e.path == "eboot.bin" && e.size == 300_000));
        // A game that does not read through libSceAmpr is left alone.
        let plain = game(&root.join("noampr"));
        let mut t2 = FolderSource::open(&plain).unwrap();
        assert!(AmprIndexed::wrap(&mut t2).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Packing is exFAT-only and leaves nothing behind when it is refused.
    #[test]
    fn lz4_packs_are_refused_for_other_formats() {
        let root = std::env::temp_dir().join(format!("ps5upload-imgr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = ampr_game(&root);
        let out = root.join("x.ffpkg");
        let mut tree = FolderSource::open(&src).unwrap();
        let e = build_packed(
            ImageFormat::Ffpkg,
            &mut tree,
            &out,
            &Lz4Packs {
                profile: None,
                level: 9,
                block_shift: 16,
            },
            DEFAULT_VERIFY_CHUNK,
            &AtomicBool::new(false),
            &mut |_, _, _| {},
        )
        .err()
        .expect("refused");
        assert!(e.contains("exFAT"), "{e}");
        assert!(!out.exists() && !lz4_spool(&out).exists());

        let exfat = root.join("x.exfat");
        let attempt = |src: &Path| {
            let mut tree = FolderSource::open(src).unwrap();
            build_packed(
                ImageFormat::Exfat,
                &mut tree,
                &exfat,
                &Lz4Packs {
                    profile: None,
                    level: 9,
                    block_shift: 16,
                },
                DEFAULT_VERIFY_CHUNK,
                &AtomicBool::new(false),
                &mut |_, _, _| {},
            )
            .err()
            .expect("refused")
        };
        // An ampr_emu without pack support (0.3) would see every packed file missing.
        std::fs::write(
            src.join("fakelib/libSceAmpr.sprx"),
            b"\x000.3.1 (c) Drakmor\0AMPRIDX3",
        )
        .unwrap();
        let e = attempt(&src);
        assert!(
            e.contains("ampr_emu 0.3.1") && e.contains("does not serve"),
            "{e}"
        );
        // No ampr_emu in the game at all.
        std::fs::remove_file(src.join("fakelib/libSceAmpr.sprx")).unwrap();
        assert!(attempt(&src).contains("has none"));
        // A title that does not read through libSceAmpr.
        let plain = game(&root.join("plain"));
        assert!(attempt(&plain).contains("does not read its files through AMPR"));
        assert!(!exfat.exists() && !lz4_spool(&exfat).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
