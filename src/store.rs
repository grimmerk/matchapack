//! Machine-wide store of EXTRACTED packages, keyed by integrity.
//!
//! Layout: `<store>/v1/<algo>/<hex[0..2]>/<hex[2..]>/` holds the package root
//! (the tarball's first path component stripped, as pacote does). An entry is
//! written to `<store>/tmp/` and renamed into place, so a present directory is
//! always complete; it is removed the same way in reverse (renamed out, then
//! deleted).
//!
//! Also under the store: `sizes/` (cached entry sizes), `projects/` (one
//! record per installed project, for `matchapack store gc`), `lock` (held
//! shared by every install, exclusively by commands that remove entries), and
//! for the inject engine `made-by/` (per entry: the keys of the extraction
//! code under which it is known to be what pacote extracts; see serve.rs)
//! and `declined/` (one file per extraction code whose extraction the clones
//! cannot reproduce: installs under it are npm's own).
//!
//! Tarballs come from npm's own content-addressed cache (`~/.npm/_cacache`).

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use sha2::Digest;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// What the hidden lockfile npm wrote says about a project's store
/// entries (`Store::update_record`): the entry keys it names, and whether it
/// names the source of every package npm extracts.
#[derive(Debug, Clone, Copy)]
pub enum Tree<'a> {
    Complete(&'a std::collections::HashSet<String>),
    Incomplete(&'a std::collections::HashSet<String>),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Integrity {
    pub algo: String,
    pub hex: String,
}

const ALGO_RANK: [&str; 4] = ["sha512", "sha384", "sha256", "sha1"];

impl Integrity {
    /// Parse an SRI string (possibly several space-separated hashes) and keep the strongest.
    pub fn parse(s: &str) -> Result<Integrity> {
        let mut best: Option<(usize, Integrity)> = None;
        for part in s.split_whitespace() {
            let Some((algo, rest)) = part.split_once('-') else { continue };
            let b64 = rest.split('?').next().unwrap_or("");
            let Some(rank) = ALGO_RANK.iter().position(|a| *a == algo) else { continue };
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .with_context(|| format!("bad base64 in integrity {part}"))?;
            let want = match algo { "sha512" => 64, "sha384" => 48, "sha256" => 32, _ => 20 };
            if bytes.len() != want {
                bail!("integrity {part} has a {}-byte digest, {algo} needs {want}", bytes.len());
            }
            let i = Integrity { algo: algo.to_string(), hex: hex::encode(bytes) };
            if best.as_ref().map_or(true, |(r, _)| rank < *r) {
                best = Some((rank, i));
            }
        }
        best.map(|(_, i)| i).ok_or_else(|| anyhow!("no usable hash in integrity {s:?}"))
    }

    /// The SRI string (`sha512-<base64>`), as a lockfile gives it.
    pub fn sri(&self) -> String {
        format!("{}-{}", self.algo, base64::engine::general_purpose::STANDARD.encode(hex::decode(&self.hex).unwrap_or_default()))
    }

    /// `<algo>-<hex>`: how project records name an entry.
    pub fn key(&self) -> String {
        format!("{}-{}", self.algo, self.hex)
    }

    pub fn from_key(k: &str) -> Option<Integrity> {
        let (algo, hex) = k.split_once('-')?;
        let ok = ALGO_RANK.contains(&algo) && hex.len() >= 8 && hex.bytes().all(|b| b.is_ascii_hexdigit());
        ok.then(|| Integrity { algo: algo.to_string(), hex: hex.to_string() })
    }

    pub(crate) fn split_path(&self, base: &Path) -> PathBuf {
        base.join(&self.algo).join(&self.hex[0..2]).join(&self.hex[2..4]).join(&self.hex[4..])
    }

    pub fn digest(&self, data: &[u8]) -> String {
        match self.algo.as_str() {
            "sha512" => hex::encode(sha2::Sha512::digest(data)),
            "sha384" => hex::encode(sha2::Sha384::digest(data)),
            "sha256" => hex::encode(sha2::Sha256::digest(data)),
            "sha1" => hex::encode(sha1::Sha1::digest(data)),
            _ => String::new(),
        }
    }
}

/// Whether the open file `f` is the file `p` names now (same device and
/// inode). A link at `p` is followed, as opening `p` follows it: a store
/// whose `lock` is a link locks the link's target.
pub(crate) fn same_file(f: &fs::File, p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (f.metadata(), fs::metadata(p)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

pub(crate) fn flock(f: &fs::File, op: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// flock, retried when a signal interrupts it.
fn flock_retry(f: &fs::File, op: libc::c_int) -> std::io::Result<()> {
    loop {
        match flock(f, op) {
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            r => return r,
        }
    }
}

pub(crate) fn unsupported(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(n) if n == libc::ENOTSUP || n == libc::EOPNOTSUPP)
}

pub(crate) fn would_block(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EWOULDBLOCK)
}

/// `<pid>-<nanos>`, fixed for the process: the prefix of every tmp directory
/// it makes. The nanos keep two hosts sharing a store from colliding on a
/// pid; the pid stays first for the cleanup's fallback rule.
pub fn process_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        format!("{}-{nanos}", std::process::id())
    })
}

/// A directory under a store's `tmp/`, made fresh and held by this process
/// through an exclusive flock on the directory itself. The OS releases the
/// lock when the process exits, however it exits, so Store::new can tell a
/// leftover from a directory in use without trusting pids.
pub struct TmpDir {
    pub path: PathBuf,
    _lock: fs::File,
}

impl TmpDir {
    pub fn create(path: PathBuf, mode: u32) -> Result<TmpDir> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::DirBuilder::new().mode(mode).create(&path).with_context(|| format!("creating {}", path.display()))?;
        let f = fs::File::open(&path)?;
        // A filesystem without flock leaves the age rule (Store::new) as the guard.
        let _ = flock(&f, libc::LOCK_EX | libc::LOCK_NB);
        Ok(TmpDir { path, _lock: f })
    }
}

fn older_than(p: &Path, secs: u64) -> bool {
    fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|d| d.as_secs() > secs).unwrap_or(false))
        .unwrap_or(false)
}

/// Age below which nothing in `tmp/` is removed and no entry is collected:
/// the guard left where flock does not reach (a store shared over a network
/// filesystem without lock support).
pub const GRACE_SECS: u64 = 3600;

/// Remove leftovers in `<store>/tmp/` of processes that are gone: entries
/// nobody holds a lock on, older than GRACE_SECS. Where flock is not
/// supported, the earlier rule: the pid in the name is dead (on this host).
fn clean_tmp(tmp: &Path) -> Result<()> {
    for ent in fs::read_dir(tmp)?.flatten() {
        let p = ent.path();
        if !older_than(&p, GRACE_SECS) {
            continue;
        }
        let Ok(f) = fs::File::open(&p) else { continue };
        let gone = match flock(&f, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => true,
            Err(e) if would_block(&e) => false,
            Err(_) => {
                let name = ent.file_name().to_string_lossy().into_owned();
                name.split('-').next().and_then(|p| p.parse::<i32>().ok()).is_some_and(|pid| {
                    !(unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
                })
            }
        };
        if gone {
            let _ = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
        }
    }
    Ok(())
}

pub struct Store {
    pub root: PathBuf,
    pub cacache: PathBuf,
    tmp_seq: AtomicU64,
    _lock: Option<fs::File>,
}

/// What `Store::ensure` (tests only) did.
#[cfg(test)]
pub enum Ensure {
    Present,
    Extracted,
    /// No tarball in the npm cache; the caller must fetch it.
    Missing,
}

impl Store {
    /// Open for installing and reading: a shared lock on `<store>/lock`, so
    /// no entry is removed while this process may clone from it.
    pub fn new(root: PathBuf, cacache: PathBuf) -> Result<Store> {
        Store::open(root, cacache, false)
    }

    /// Open for removing entries: the exclusive lock, refused while any
    /// install holds the store.
    pub fn exclusive(root: PathBuf, cacache: PathBuf) -> Result<Store> {
        Store::open(root, cacache, true)
    }

    fn open(root: PathBuf, cacache: PathBuf, exclusive: bool) -> Result<Store> {
        // `matchapack uninstall` moves the store aside while it holds the
        // lock exclusively: a process that waited on the lock then holds the
        // lock file of a store that is gone. So after taking the lock, check
        // that `<root>/lock` is still the file locked, and start over if not
        // (a fresh store; a later command locks the same file).
        for _ in 0..20 {
            fs::create_dir_all(root.join(LAYOUT))?;
            fs::create_dir_all(root.join("tmp"))?;
            // Read and write: NFSv4 refuses a shared lock on a file not open for
            // reading (EBADF), an exclusive one on a file not open for writing.
            let lf = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(root.join("lock"))?;
            let lock = if exclusive {
                match flock_retry(&lf, libc::LOCK_EX | libc::LOCK_NB) {
                    Ok(()) => Some(lf),
                    Err(e) if would_block(&e) => bail!("the store is in use by a running install; try again when it has finished"),
                    Err(e) => bail!("cannot lock the store ({e}); refusing to remove entries without it"),
                }
            } else {
                match flock_retry(&lf, libc::LOCK_SH | libc::LOCK_NB) {
                    Ok(()) => Some(lf),
                    Err(e) if would_block(&e) => {
                        eprintln!("matchapack: waiting for a `matchapack store` or `matchapack uninstall` command to release the store");
                        flock_retry(&lf, libc::LOCK_SH).context("waiting for the store lock")?;
                        Some(lf)
                    }
                    // No flock on this filesystem at all: installs go on unguarded,
                    // as before, and gc / verify --fix refuse (they need the lock).
                    Err(e) if unsupported(&e) => None,
                    // Anything else may pass: refuse rather than install without
                    // the lock while a gc could get it (the install goes to npm).
                    Err(e) => bail!("cannot lock the store ({e})"),
                }
            };
            if let Some(f) = &lock {
                if !same_file(f, &root.join("lock")) {
                    continue;
                }
            }
            clean_tmp(&root.join("tmp"))?;
            return Ok(Store { root, cacache, tmp_seq: AtomicU64::new(0), _lock: lock });
        }
        bail!("the store's lock file kept being replaced while waiting for it ({})", root.join("lock").display())
    }

    pub fn dir_for(&self, i: &Integrity) -> PathBuf {
        entry_dir(&self.root, i)
    }

    fn size_file(&self, i: &Integrity) -> PathBuf {
        self.root.join("sizes").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
    }

    fn made_by_file(&self, i: &Integrity) -> PathBuf {
        self.root.join("made-by").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
    }

    /// The keys of the extraction code (serve.rs `code_key`, one per line;
    /// `pacote@… tar@…` in records written before PR C1 of #39) under which
    /// this entry is known to be what pacote extracts: the one that made it,
    /// and every one that later extracted its tarball to the same tree.
    /// Empty: made by matchapack's own extractor (the classic engine,
    /// removed in PR C2), or before this was recorded.
    pub fn made_by(&self, i: &Integrity) -> Vec<String> {
        fs::read_to_string(self.made_by_file(i))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    /// Add `key` to the entry's record (written whole: tmp, then rename).
    pub fn add_made_by(&self, i: &Integrity, key: &str) -> Result<()> {
        let mut keys = self.made_by(i);
        if keys.iter().any(|k| k == key) {
            return Ok(());
        }
        keys.push(key.to_string());
        self.write_made_by(i, &keys)
    }

    /// A lock on one entry (`<store>/locks/…`, a file beside the entry, never
    /// inside it): shared while an install clones the entry, exclusive while
    /// one replaces or fills it or writes its made-by record. So a clone sees
    /// one tree, and a record is written for the tree that was compared.
    /// Released when the returned file is dropped. Where the filesystem has no
    /// flock, no lock (as for the store lock).
    pub fn entry_lock(&self, i: &Integrity, exclusive: bool) -> Result<fs::File> {
        let p = self.root.join("locks").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..]);
        fs::create_dir_all(p.parent().unwrap())?;
        let f = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&p)?;
        match flock_retry(&f, if exclusive { libc::LOCK_EX } else { libc::LOCK_SH }) {
            Ok(()) => Ok(f),
            Err(e) if unsupported(&e) => Ok(f),
            Err(e) => Err(e).with_context(|| format!("locking {}", p.display())),
        }
    }

    pub fn forget_made_by(&self, i: &Integrity) {
        let _ = fs::remove_file(self.made_by_file(i));
    }

    /// The entry was made (or replaced) by `key`: the record is that key only.
    pub fn set_made_by(&self, i: &Integrity, key: &str) -> Result<()> {
        self.write_made_by(i, &[key.to_string()])
    }

    fn write_made_by(&self, i: &Integrity, keys: &[String]) -> Result<()> {
        let f = self.made_by_file(i);
        fs::create_dir_all(f.parent().unwrap())?;
        let tmp = f.with_extension(format!("{}-{}", process_tag(), self.tmp_seq.fetch_add(1, Ordering::Relaxed)));
        let mut text = keys.join("\n");
        text.push('\n');
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &f)?;
        Ok(())
    }

    /// The file that declines extraction-code `key`: one per key, named by
    /// the key's SHA-256, so that two installs declining two keys at once
    /// never rewrite each other's file (no read-modify-write of a list).
    pub fn declined_path(&self, key: &str) -> PathBuf {
        self.root.join("declined").join(hex::encode(sha2::Sha256::digest(key.as_bytes())))
    }

    /// Why installs under extraction-code `key` are left to npm in this
    /// store (`decline_key`), or None. Removing the file clears it. A decline
    /// that another matchapack build recorded (or one with no build, from
    /// before builds were recorded) is void: None, so this build compares
    /// and fill-checks again, and declines again if the extraction is still
    /// not what a clone reproduces (`decline_key` then rewrites the file).
    pub fn declined(&self, key: &str) -> Option<String> {
        self.declined_by(key, build_identity)
    }

    /// `declined`, with this build named by `build` (asked only when a
    /// decline file for the key exists: computing it may hash the binary).
    fn declined_by(&self, key: &str, build: impl FnOnce() -> &'static str) -> Option<String> {
        let d = read_decline(&self.declined_path(key))?;
        (d.key == key && d.build.as_deref() == Some(build())).then_some(d.why)
    }

    /// Put `key` on the store's decline list, so that later installs under
    /// that code let npm extract every package: `<store>/declined/<sha256 of
    /// the key>`, holding the key, why, and `build: <this build>` on three
    /// lines. Written whole (tmp, then rename), replacing a void decline.
    /// Returns the file.
    pub fn decline_key(&self, key: &str, why: &str) -> Result<PathBuf> {
        self.decline_key_as(key, why, build_identity())
    }

    fn decline_key_as(&self, key: &str, why: &str, build: &'static str) -> Result<PathBuf> {
        let f = self.declined_path(key);
        if self.declined_by(key, || build).is_some() {
            return Ok(f);
        }
        fs::create_dir_all(f.parent().unwrap())?;
        let why: String = why.chars().map(|c| if c.is_control() { ' ' } else { c }).take(300).collect();
        let tmp = f.with_extension(format!("{}-{}", process_tag(), self.tmp_seq.fetch_add(1, Ordering::Relaxed)));
        fs::write(&tmp, format!("{key}\n{why}\nbuild: {build}\n"))?;
        fs::rename(&tmp, &f)?;
        Ok(f)
    }

    /// Every decline file of the store (`matchapack report`), with whether
    /// it holds for this build.
    pub fn declines(&self) -> Vec<Decline> {
        let Ok(rd) = fs::read_dir(self.root.join("declined")) else { return vec![] };
        let mut v: Vec<Decline> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())))
            .filter_map(|p| read_decline(&p))
            .collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    /// Sum of the file sizes in a store entry, cached in `<store>/sizes/` (never
    /// inside the entry, which is cloned into projects as a whole).
    pub fn entry_size(&self, i: &Integrity) -> u64 {
        let p = self.size_file(i);
        if let Some(n) = fs::read_to_string(&p).ok().and_then(|s| s.trim().parse().ok()) {
            return n;
        }
        let n = dir_bytes(&self.dir_for(i));
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fs::write(&p, n.to_string());
        n
    }

    /// Every entry in the store.
    pub fn entries(&self) -> Vec<Integrity> {
        let base = self.root.join(LAYOUT);
        let mut v = vec![];
        // Directories with an entry's name only: a stray file (a Finder
        // .DS_Store) is not an entry, and gc could not remove it as one. A
        // symlink with an entry's name is listed, so that verify reports it
        // and gc or --fix can remove it (ensure refuses to use it).
        for e in walkdir::WalkDir::new(&base).min_depth(3).max_depth(3).into_iter().flatten() {
            if !(e.file_type().is_dir() || e.file_type().is_symlink()) {
                continue;
            }
            let rel = e.path().strip_prefix(&base).unwrap();
            let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if let [algo, a, b] = parts.as_slice() {
                if let Some(i) = Integrity::from_key(&format!("{algo}-{a}{b}")) {
                    v.push(i);
                }
            }
        }
        v
    }

    /// (entries, bytes) of the whole store, from the size cache.
    pub fn totals(&self) -> (usize, u64) {
        let all = self.entries();
        let bytes = all.iter().map(|i| self.entry_size(i)).sum();
        (all.len(), bytes)
    }

    /// Remove an entry: locked, renamed out of the layout (so it is never
    /// seen half-deleted, which would read as present), then deleted. A crash
    /// mid-way leaves an unlocked directory in tmp/ for Store::new to remove.
    /// Only on a store opened with `exclusive`.
    /// Remove the entry locks (`locks/`) whose entry is gone (left by an
    /// interrupted replacement). Only on a store opened with `exclusive`:
    /// no install holds any entry lock then. Returns how many.
    pub fn remove_orphan_locks(&self) -> usize {
        let base = self.root.join("locks");
        let mut n = 0;
        for e in walkdir::WalkDir::new(&base).min_depth(3).max_depth(3).into_iter().flatten() {
            let rel = e.path().strip_prefix(&base).unwrap();
            let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            let gone = match parts.as_slice() {
                [algo, a, b] => Integrity::from_key(&format!("{algo}-{a}{b}")).is_some_and(|i| fs::symlink_metadata(self.dir_for(&i)).is_err()),
                _ => false,
            };
            if gone && fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
        n
    }

    pub fn remove_entry(&self, i: &Integrity) -> Result<()> {
        let src = self.dir_for(i);
        let f = fs::File::open(&src).with_context(|| format!("opening {}", src.display()))?;
        let _ = flock(&f, libc::LOCK_EX | libc::LOCK_NB);
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let trash = self.root.join("tmp").join(format!("{}-gc{seq}", process_tag()));
        fs::rename(&src, &trash).with_context(|| format!("moving {} out of the store", src.display()))?;
        let _ = fs::remove_file(self.size_file(i));
        let _ = fs::remove_file(self.made_by_file(i));
        let _ = fs::remove_file(self.root.join("locks").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..]));
        fs::remove_dir_all(&trash).with_context(|| format!("removing {}", trash.display()))?;
        Ok(())
    }

    fn project_file(&self, root: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let h = blake3::hash(root.as_os_str().as_bytes());
        self.root.join("projects").join(format!("{}.json", &h.to_hex()[..32]))
    }

    /// Replace the record of the project at `root` (canonical) with `used`
    /// (tests; installs use `update_record`).
    #[cfg(test)]
    pub fn record_project(&self, root: &Path, used: &[Integrity]) -> Result<()> {
        self.write_record(root, used.iter().map(Integrity::key).collect(), None)
    }

    /// The record after a routed command (`npm ci`, `install`, `update`,
    /// `uninstall`), one rule for all (NOTES "Review round 3"), from what the
    /// hidden lockfile npm wrote says (`tree`):
    /// - `Complete`: it names the source (an integrity or a `resolved`) of
    ///   every package npm extracts, so its entries are the project's: the
    ///   record is exactly those (what `store adopt` and gc's fallback through
    ///   the trial log use). A record behind the tree (matchapack stopped
    ///   before it wrote one; a project installed by plain npm) is completed,
    ///   and entries of packages that left the tree drop out;
    /// - `Incomplete`: some package has neither (npm read it from disk, not
    ///   from a lockfile: `package-lock=false` with a hidden lockfile npm did
    ///   not trust), so it cannot say what left: nothing is dropped. The
    ///   install family: the earlier record, the tree's entries and this
    ///   run's. `npm ci` (it emptied node_modules, except a workspace's own on
    ///   npm before 10.8.1): of the earlier record what the tree still names,
    ///   and this run's;
    /// - `Unknown` (no hidden lockfile): `npm ci` this run's entries; the
    ///   family the earlier record and this run's.
    pub fn update_record(&self, root: &Path, used: &[Integrity], tree: Tree<'_>, ci: bool) -> Result<()> {
        let old = self.recorded_entries(root).unwrap_or_default();
        let mine = used.iter().map(Integrity::key);
        let keys: Vec<String> = match tree {
            Tree::Complete(t) => t.iter().cloned().collect(),
            Tree::Incomplete(t) if ci => old.into_iter().filter(|k| t.contains(k)).chain(mine).collect(),
            Tree::Incomplete(t) => old.into_iter().chain(t.iter().cloned()).chain(mine).collect(),
            Tree::Unknown if ci => mine.collect(),
            Tree::Unknown => old.into_iter().chain(mine).collect(),
        };
        self.write_record(root, keys, None)
    }

    /// The entry keys of the project's record, if it has one.
    pub fn recorded_entries(&self, root: &Path) -> Option<Vec<String>> {
        let rec: serde_json::Value = serde_json::from_str(&fs::read_to_string(self.project_file(root)).ok()?).ok()?;
        if rec.get("path").and_then(|p| p.as_str()).map(Path::new) != Some(root) {
            return None;
        }
        Some(rec.get("entries")?.as_array()?.iter().filter_map(|k| k.as_str().map(String::from)).collect())
    }

    /// Record a project found by `store adopt` (gc.rs): what its record
    /// already lists plus `used`, marked as adopted. An install's record
    /// later replaces it as usual.
    pub fn adopt_project(&self, root: &Path, mut keys: Vec<String>) -> Result<()> {
        keys.extend(self.recorded_entries(root).unwrap_or_default());
        self.write_record(root, keys, Some("adopt"))
    }

    fn write_record(&self, root: &Path, mut keys: Vec<String>, by: Option<&str>) -> Result<()> {
        keys.sort();
        keys.dedup();
        let mut rec = serde_json::json!({
            "path": root,
            "ts": crate::trial::now(),
            "build": env!("MATCHAPACK_BUILD"),
            "entries": keys,
        });
        if let Some(by) = by {
            rec["by"] = by.into();
        }
        let f = self.project_file(root);
        fs::create_dir_all(f.parent().unwrap())?;
        let tmp = f.with_extension(format!("json.{}", process_tag()));
        fs::write(&tmp, format!("{rec}\n"))?;
        fs::rename(&tmp, &f)?;
        Ok(())
    }

    /// Locate the tarball in npm's cache: first by content address, then via the
    /// index entry for the request URL (needed when the lockfile only has sha1:
    /// cacache stores content under the sha512 it computed itself).
    fn find_tarball(&self, cacache: &Path, i: &Integrity, url: Option<&str>) -> Option<PathBuf> {
        let direct = i.split_path(&cacache.join("content-v2"));
        if direct.is_file() {
            return Some(direct);
        }
        let key = format!("make-fetch-happen:request-cache:{}", url?);
        let kh = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let idx = cacache.join("index-v5").join(&kh[0..2]).join(&kh[2..4]).join(&kh[4..]);
        let text = fs::read_to_string(idx).ok()?;
        let mut found = None;
        for line in text.lines() {
            let Some((_, json)) = line.split_once('\t') else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { continue };
            if v.get("key").and_then(|k| k.as_str()) != Some(key.as_str()) {
                continue;
            }
            found = v.get("integrity").and_then(|x| x.as_str()).map(String::from);
        }
        let ci = Integrity::parse(&found?).ok()?;
        let p = ci.split_path(&cacache.join("content-v2"));
        p.is_file().then_some(p)
    }

    /// Tests only: the entry for `i`, extracted by matchapack's own
    /// extractor (`extract`) from the tarball in npm's cache when missing,
    /// with no made-by record: what the classic engine (removed in PR C2)
    /// put in the store, and a quick way to fill a test store. Installs fill
    /// it through pacote (serve.rs). The tarball's digest is checked against
    /// `i` before anything is extracted.
    #[cfg(test)]
    pub fn ensure(&self, i: &Integrity, url: &str) -> Result<Ensure> {
        let cacache = &self.cacache;
        let dest = self.dir_for(i);
        match fs::symlink_metadata(&dest) {
            Ok(m) if m.is_dir() => return Ok(Ensure::Present),
            // A directory clone of a symlink places the symlink, not a package.
            Ok(_) => bail!("store entry {} is not a directory; `matchapack store verify --fix` removes it", dest.display()),
            Err(_) => {}
        }
        let Some(tgz) = self.find_tarball(cacache, i, Some(url)) else { return Ok(Ensure::Missing) };
        let data = fs::read(&tgz)?;
        let got = i.digest(&data);
        if got != i.hex {
            bail!("integrity mismatch for {url}: cache file {} has {}-{got}", tgz.display(), i.algo);
        }
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = TmpDir::create(self.root.join("tmp").join(format!("{}-x{seq}-{}", process_tag(), &i.hex[..16])), 0o755)?;
        extract(&data, &tmp.path).with_context(|| format!("extracting {url}"))?;
        fs::create_dir_all(dest.parent().unwrap())?;
        // The inject engine's entry lock, exclusive: the entry and the
        // removal of a stale record happen together (serve.rs). A no-replace
        // rename: a plain one would replace an empty directory.
        let excl = self.entry_lock(i, true)?;
        let renamed = rename_noreplace(&tmp.path, &dest);
        if renamed.is_ok() {
            // Made by matchapack's extractor: a record left by an entry that
            // was here before is not about this one.
            self.forget_made_by(i);
        }
        drop(excl);
        match renamed {
            Ok(()) => Ok(Ensure::Extracted),
            Err(_) if dest.is_dir() => {
                // Another process won the race; its copy is equally valid.
                let _ = fs::remove_dir_all(&tmp.path);
                Ok(Ensure::Present)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Whether npm's cache holds the tarball of `i`, found by its content
    /// address and with the bytes its integrity names: what `store verify`
    /// compares an entry with (src/verify.rs). An entry whose lockfile had
    /// only sha1 (cacache keeps those under sha512) has none.
    pub fn cached_tarball_ok(&self, i: &Integrity) -> Result<bool> {
        let Some(tgz) = self.find_tarball(&self.cacache, i, None) else { return Ok(false) };
        Ok(i.digest(&fs::read(&tgz)?) == i.hex)
    }

    /// Forget an entry's cached size (its contents were replaced).
    pub fn forget_size(&self, i: &Integrity) {
        let _ = fs::remove_file(self.size_file(i));
    }

}

/// One decline file (`Store::declines`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decline {
    pub key: String,
    pub why: String,
    /// The matchapack build that recorded it (None: before builds were
    /// recorded, which counts as another build).
    pub build: Option<String>,
    pub path: PathBuf,
    pub modified: Option<std::time::SystemTime>,
}

impl Decline {
    /// Void for this build: the next install under the key retries.
    pub fn void(&self) -> bool {
        self.build.as_deref() != Some(build_identity())
    }
}

fn read_decline(p: &Path) -> Option<Decline> {
    let text = fs::read_to_string(p).ok()?;
    let mut lines = text.lines();
    let key = lines.next()?.to_string();
    let why = lines.next().unwrap_or("").to_string();
    let build = lines.find_map(|l| l.strip_prefix("build: ")).map(String::from);
    let modified = fs::metadata(p).and_then(|m| m.modified()).ok();
    Some(Decline { key, why, build, path: p.to_path_buf(), modified })
}

/// This matchapack build, as a decline file records it: the version and
/// the git commit embedded at build time (`--version`). A build from a
/// checkout with uncommitted changes (`-dirty`) or from outside git
/// (`unknown`) is not told apart by that, so the SHA-256 of the running
/// executable is added; it is computed once, and only when a decline file
/// is read for a matching key or written.
pub fn build_identity() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let build = env!("MATCHAPACK_BUILD");
        let mut id = format!("{} ({build})", env!("CARGO_PKG_VERSION"));
        if build == "unknown" || build.ends_with("-dirty") {
            let exe = std::env::current_exe().and_then(fs::read).map(|b| hex::encode(&sha2::Sha256::digest(&b)[..8]));
            id.push_str(&format!(" exe {}", exe.unwrap_or_else(|_| "?".into())));
        }
        id
    })
}

/// What comparing a store entry with pacote's extraction found
/// (`compare_extraction`).
#[derive(Debug, PartialEq, Eq)]
pub enum Compared {
    Same,
    /// The first difference.
    Differs(String),
    /// pacote's extraction is not what a clone, touched to the install's
    /// time, reproduces: a file older than the install, or something other
    /// than files and directories. Nothing was compared.
    Unexpected(String),
}

/// How much older than the install's start a file of pacote's extraction
/// may be before it counts as not written by this install (`Unexpected`):
/// room for a clock set back during the install, and for filesystems that
/// keep coarse times. A tar that applied the tarball's times would give
/// npm's 1985 dates.
pub const EXTRACTION_TIME_SLACK: std::time::Duration = std::time::Duration::from_secs(3600);

/// Compare a store entry with pacote's extraction of its tarball,
/// `extracted`, made by this install (or `store verify`), which began at
/// `started`: the same files (contents, modes), the same directories (and
/// their modes: the umask is 022 for both, and a clone carries them into
/// the project) and nothing else; the first difference, from the
/// extraction's side (until 2026-11 `compare_trees`). In the same walk (the
/// metadata is read there anyway), every path of `extracted` must be a file
/// or a directory, and every file at most EXTRACTION_TIME_SLACK older than
/// `started`: pacote extracts with `noMtime` and no links, which is what
/// the placement of a clone (then touched to now) reproduces.
pub fn compare_extraction(extracted: &Path, entry: &Path, started: std::time::SystemTime) -> Result<Compared> {
    let (want, odd) = tree(extracted, true, Some(fresh_since(started)))?;
    if let Some(odd) = odd {
        return Ok(Compared::Unexpected(odd));
    }
    let got = tree(entry, true, None)?.0;
    Ok(match first_difference(&want, &got) {
        Some(d) => Compared::Differs(d),
        None => Compared::Same,
    })
}

fn first_difference(want: &[(PathBuf, Node)], got: &[(PathBuf, Node)]) -> Option<String> {
    match want.iter().zip(got.iter()).find(|(w, g)| w != g) {
        Some((w, g)) => Some(describe(w, g)),
        None if want.len() != got.len() => {
            let (longer, what) = if got.len() > want.len() { (&got, "not in the tarball") } else { (&want, "missing") };
            let p = &longer[want.len().min(got.len())].0;
            Some(format!("{}: {what}", show_rel(p)))
        }
        None => None,
    }
}

/// One path in a tree: its kind, its mode (a directory's only when compared),
/// and for a file its contents' hash.
#[derive(PartialEq, Eq)]
enum Node {
    Dir(Option<u32>),
    File(u32, blake3::Hash),
    Other,
}

/// Why the path `rel` of pacote's extraction (file type `ft`, metadata
/// `meta`, not followed) is not what a clone, touched to the install's time,
/// reproduces: neither a file nor a directory, or a file or directory last
/// modified before `since` (a placed clone's paths are all touched to now).
/// None when it is. The one rule for the comparison and the
/// fill (`compare_extraction`, `extraction_odd`).
fn odd_path(rel: &Path, ft: fs::FileType, meta: &fs::Metadata, since: std::time::SystemTime) -> Result<Option<String>> {
    if !ft.is_dir() && !ft.is_file() {
        return Ok(Some(format!("{}: neither a file nor a directory", show_rel(rel))));
    }
    {
        let mtime = meta.modified()?;
        if mtime < since {
            let secs = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(-1);
            return Ok(Some(format!("{}: modified at {} (seconds since 1970), before this install began", show_rel(rel), secs(mtime))));
        }
    }
    Ok(None)
}

/// The earliest moment a file of pacote's extraction made by an install
/// that began at `started` may carry (EXTRACTION_TIME_SLACK).
fn fresh_since(started: std::time::SystemTime) -> std::time::SystemTime {
    started.checked_sub(EXTRACTION_TIME_SLACK).unwrap_or(std::time::UNIX_EPOCH)
}

/// The check `compare_extraction` makes on pacote's extraction, alone, for
/// an extraction that becomes a store entry with nothing to compare it
/// with (a fill): metadata only, nothing read. The first path that is not
/// what a clone reproduces, or None.
pub fn extraction_odd(extracted: &Path, started: std::time::SystemTime) -> Result<Option<String>> {
    let since = fresh_since(started);
    // Unsorted: any odd path will do, and nothing is compared.
    for e in walkdir::WalkDir::new(extracted).follow_links(false).follow_root_links(false) {
        let e = e?;
        let rel = e.path().strip_prefix(extracted).unwrap();
        if let Some(odd) = odd_path(rel, e.file_type(), &e.metadata()?, since)? {
            return Ok(Some(odd));
        }
    }
    Ok(None)
}

/// Every path under `root` (`root` itself as ""), sorted, symlinks not
/// followed. With `fresh_since`: the walk stops at the first path
/// `odd_path` names, and says why (the second value).
fn tree(root: &Path, dir_modes: bool, fresh_since: Option<std::time::SystemTime>) -> Result<(Vec<(PathBuf, Node)>, Option<String>)> {
    let mut v = vec![];
    for e in walkdir::WalkDir::new(root).follow_links(false).follow_root_links(false).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
        let ft = e.file_type();
        let meta = e.metadata()?;
        let mode = meta.permissions().mode() & 0o7777;
        if let Some(since) = fresh_since {
            if let Some(odd) = odd_path(&rel, ft, &meta, since)? {
                return Ok((v, Some(odd)));
            }
        }
        let n = if ft.is_dir() {
            Node::Dir(dir_modes.then_some(mode))
        } else if ft.is_file() {
            let mut h = blake3::Hasher::new();
            std::io::copy(&mut fs::File::open(e.path())?, &mut h)?;
            Node::File(mode, h.finalize())
        } else {
            Node::Other
        };
        v.push((rel, n));
    }
    Ok((v, None))
}

fn show_rel(p: &Path) -> String {
    if p.as_os_str().is_empty() { "(entry root)".into() } else { p.display().to_string() }
}

/// The first difference between two tree items, `w` from the tarball.
fn describe(w: &(PathBuf, Node), g: &(PathBuf, Node)) -> String {
    if w.0 != g.0 {
        // Sorted walks: the smaller name is the one the other side lacks.
        return if w.0 < g.0 { format!("{}: missing", show_rel(&w.0)) } else { format!("{}: not in the tarball", show_rel(&g.0)) };
    }
    let p = show_rel(&w.0);
    match (&w.1, &g.1) {
        (Node::File(wm, _), Node::File(gm, _)) if wm != gm => format!("{p}: mode {gm:o}, the tarball gives {wm:o}"),
        (Node::File(..), Node::File(..)) => format!("{p}: contents differ"),
        (Node::Dir(Some(wm)), Node::Dir(Some(gm))) => format!("{p}: directory mode {gm:o}, the tarball gives {wm:o}"),
        (_, Node::Other) => format!("{p}: not a regular file or directory"),
        (Node::Dir(_), _) => format!("{p}: a file where the tarball has a directory"),
        _ => format!("{p}: a directory where the tarball has a file"),
    }
}

/// Free bytes on the volume holding `p` (for unprivileged users), from statvfs.
pub fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

fn dir_bytes(d: &Path) -> u64 {
    walkdir::WalkDir::new(d)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
const UMASK: u32 = 0o022;

/// Store layout / extraction-rules version. Bump whenever what lands in an
/// entry changes (it was `extract`'s rules while the classic engine filled
/// the store), so entries made under old rules are never reused.
/// v1 → v2: drop directory entries (pacote only creates parents of files) and
/// the `.gitignore` → `.npmignore` rename.
pub const LAYOUT: &str = "v2";

/// An entry's directory in the store at `root`.
pub fn entry_dir(root: &Path, i: &Integrity) -> PathBuf {
    root.join(LAYOUT).join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
}

/// Tests only (the classic engine's extractor, removed in PR C2; installs
/// and `store verify` extract with npm's own pacote). Extract like pacote (npm/node_modules/pacote/lib/fetcher.js `#tarxOptions`):
/// strip the first path component; keep FILE entries only (links and directory
/// entries are filtered out, so empty directories never appear; directories
/// exist only as parents of files, mode 0o755); refuse `..`; a `.gitignore` is
/// renamed to `.npmignore` unless a `.npmignore` at that path was seen earlier in
/// the archive, in which case it is dropped; files get
/// `((mode | 0o666) & ~umask) | 0o600`. Executable bits for declared bins are
/// added later by npm's bin linking, as in `npm ci`.
#[cfg(test)]
pub fn extract(data: &[u8], dest: &Path) -> Result<()> {
    use std::io::Read;
    use std::path::Component;
    let reader: Box<dyn Read> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(data))
    } else {
        Box::new(data)
    };
    let mut ar = tar::Archive::new(reader);
    let mut saw_npmignore: std::collections::HashSet<PathBuf> = Default::default();
    for ent in ar.entries()? {
        let mut ent = ent?;
        let et = ent.header().entry_type();
        let raw = ent.path()?.into_owned();
        let mut rel = PathBuf::new();
        let mut first = true;
        let mut bad = false;
        for c in raw.components() {
            match c {
                Component::Normal(n) => {
                    if first {
                        first = false;
                    } else {
                        rel.push(n)
                    }
                }
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) => {}
                Component::ParentDir => bad = true,
            }
        }
        if bad || rel.as_os_str().is_empty() {
            continue;
        }
        if !(et.is_file() || et == tar::EntryType::Continuous) {
            continue; // directories, links, devices: pacote's filter drops them
        }
        match rel.file_name().and_then(|n| n.to_str()) {
            Some(".npmignore") => {
                saw_npmignore.insert(rel.clone());
            }
            Some(".gitignore") => {
                let ni = rel.with_file_name(".npmignore");
                if saw_npmignore.contains(&ni) {
                    continue;
                }
                rel = ni;
            }
            _ => {}
        }
        let out = dest.join(&rel);
        if let Some(p) = out.parent() {
            fs::create_dir_all(p)?;
        }
        if out.symlink_metadata().is_ok() {
            if out.is_dir() {
                fs::remove_dir_all(&out)?;
            } else {
                fs::remove_file(&out)?;
            }
        }
        let mode = ent.header().mode().unwrap_or(0o644);
        let mut f = fs::File::create(&out)?;
        std::io::copy(&mut ent, &mut f)?;
        drop(f);
        let m = ((mode | 0o666) & !UMASK) | 0o600;
        fs::set_permissions(&out, fs::Permissions::from_mode(m & 0o7777))?;
    }
    Ok(())
}

/// Rename `from` to `to` only if `to` does not exist: a plain rename(2)
/// replaces an existing empty directory. Where the system has no such rename
/// (or the filesystem refuses it), an existence check and a plain rename.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let (f, t) = (c(from)?, c(to)?);
    #[cfg(target_os = "macos")]
    let r = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let r = unsafe { libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    let r: libc::c_int = {
        let _ = (&f, &t);
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        -1
    };
    if r == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::ENOTSUP) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(from, to)
        }
        _ => Err(e),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn integrity_keeps_the_strongest_hash_and_checks_digest_length() {
        let sha1 = format!("sha1-{}", "A".repeat(27) + "=");
        let sha512 = format!("sha512-{}", "A".repeat(86) + "==");
        let i = Integrity::parse(&format!("{sha1} {sha512}")).unwrap();
        assert_eq!(i.algo, "sha512");
        assert_eq!(i.hex.len(), 128);
        assert!(Integrity::parse("sha512-").is_err(), "empty digest");
        assert!(Integrity::parse("sha512-AAAA").is_err(), "short digest");
        assert!(Integrity::parse("md5-AAAA").is_err(), "no supported hash");
        assert_eq!(Integrity::from_key(&i.key()), Some(i));
        assert_eq!(Integrity::from_key("sha512-../../x"), None);
    }

    /// Run `f` again while `locked` says it was refused for a lock, for up
    /// to 10 s; for a test that holds no lock on that store itself.
    ///
    /// Why: tests run on parallel threads of one process, and some start
    /// child processes. A child gets a copy of every open file of the
    /// process, a held flock with it, and keeps it until its exec closes
    /// the close-on-exec ones (on macOS with posix_spawn too). So a lock a
    /// test took and released, itself or through the code it runs, can
    /// stay held for that moment by another test's child, and a lock
    /// attempt without waiting (`Store::exclusive`, uninstall) is refused.
    /// Measured (NOTES "A store lock held by another test's child"): with
    /// two threads spawning `true`, 5 of 500 uninstall runs refused with
    /// posix_spawn, 25 of 500 with fork and exec, none without spawns; the
    /// lock was free again within 2 ms. A lock a test really leaves held
    /// stays held for the 10 s and still fails it.
    pub(crate) fn retry_while_locked<T>(mut f: impl FnMut() -> T, locked: impl Fn(&T) -> bool) -> T {
        let t0 = std::time::Instant::now();
        loop {
            let r = f();
            if !locked(&r) || t0.elapsed() > std::time::Duration::from_secs(10) {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// `Store::exclusive`, retried while another test's child holds a copy
    /// of a lock this test released (`retry_while_locked`).
    pub(crate) fn exclusive(root: &Path, cacache: &Path) -> Result<Store> {
        retry_while_locked(|| Store::exclusive(root.to_path_buf(), cacache.to_path_buf()), |r| matches!(r, Err(e) if e.to_string().contains("in use by a running install")))
    }

    /// A fresh directory under the system temp dir, removed first if a
    /// previous run left it.
    pub fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("matchapack-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Set a path's modification time `secs` into the past.
    pub fn age(p: &Path, secs: i64) {
        use std::os::unix::ffi::OsStrExt;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let t = libc::timespec { tv_sec: (now - secs) as _, tv_nsec: 0 };
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }, 0);
    }

    /// A gzipped package tarball (`package/<path>`), put into a fake npm
    /// cache under its sha512; returns its integrity.
    pub fn cached_tarball(cacache: &Path, files: &[(&str, u32, &str)]) -> Integrity {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(vec![], flate2::Compression::fast()));
        for (p, mode, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(*mode);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, format!("package/{p}"), body.as_bytes()).unwrap();
        }
        let data = b.into_inner().unwrap().finish().unwrap();
        let i = Integrity { algo: "sha512".into(), hex: hex::encode(sha2::Sha512::digest(&data)) };
        let p = i.split_path(&cacache.join("content-v2"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
        i
    }

    /// `store verify` (src/verify.rs) with matchapack's own extractor
    /// standing in for pacote: the tarball of `i` from the fake npm cache.
    const KEY: &str = "pacote@1.0.0 tar@2.0.0";
    fn rust_extract(cacache: &Path) -> impl Fn(&Integrity, &Path) -> Result<()> + '_ {
        move |i, to| extract(&fs::read(i.split_path(&cacache.join("content-v2")))?, to)
    }
    fn verify_entry(store: &Store, i: &Integrity) -> crate::verify::Found {
        crate::verify::verify_entry(store, KEY, i, &rust_extract(&store.cacache), false).unwrap()
    }

    #[test]
    fn verify_finds_changed_missing_and_extra_files_and_modes() {
        use crate::verify::Found;
        let d = scratch("verify");
        let cacache = d.join("cacache");
        let i = cached_tarball(&cacache, &[("index.js", 0o644, "module.exports = 1\n"), ("bin/cli", 0o755, "#!/bin/sh\n"), (".gitignore", 0o644, "x\n")]);
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        let fresh = || {
            if store.dir_for(&i).exists() {
                fs::remove_dir_all(store.dir_for(&i)).unwrap();
            }
            assert!(matches!(store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted));
            assert!(matches!(verify_entry(&store, &i), Found::Match { .. }), "a fresh extraction matches");
            store.dir_for(&i)
        };
        let differs = |what: &str| match verify_entry(&store, &i) {
            Found::Differs { why, .. } => why,
            c => panic!("{what}: expected Differs, got {c:?}"),
        };
        let e = fresh();
        fs::write(e.join("index.js"), "module.exports = 2\n").unwrap();
        assert!(differs("changed").contains("index.js: contents differ"));
        let e = fresh();
        fs::write(e.join("index.js"), "").unwrap();
        assert!(differs("truncated").contains("index.js"));
        let e = fresh();
        fs::remove_file(e.join("bin/cli")).unwrap();
        assert!(differs("missing").contains("bin/cli"));
        let e = fresh();
        fs::write(e.join("extra.js"), "").unwrap();
        assert!(differs("extra").contains("extra.js: not in the tarball"));
        let e = fresh();
        fs::set_permissions(e.join("index.js"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(differs("mode").contains("mode 755"));
        let e = fresh();
        fs::create_dir(e.join("empty")).unwrap();
        assert!(differs("empty dir").contains("empty: not in the tarball"));
        let e = fresh();
        std::os::unix::fs::symlink("index.js", e.join("link")).unwrap();
        assert!(differs("symlink").contains("link"));
        // Directory modes are compared, as the inject engine's checks do (a
        // clone carries them into the project; the umask is 022 for both).
        let e = fresh();
        fs::set_permissions(e.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(differs("dir mode").contains("bin: directory mode 700"));
        let e = fresh();
        fs::set_permissions(&e, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(differs("root dir mode").contains("(entry root): directory mode 700"));
        fs::set_permissions(&e, fs::Permissions::from_mode(0o755)).unwrap();
        // The entry root replaced by a symlink to a valid copy.
        let e = fresh();
        let moved = d.join("moved");
        fs::rename(&e, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &e).unwrap();
        assert_eq!(verify_entry(&store, &i), Found::NotDir { fixed: false }, "root symlink");
        assert_eq!(store.entries(), vec![i.clone()], "listed, so the CLI verify and gc see it");
        assert!(store.ensure(&i, "http://x/p.tgz").is_err(), "never used for an install");
        fs::remove_file(&e).unwrap();
        fs::remove_dir_all(&moved).unwrap();
        // A stray file where entries live is not an entry.
        let _e = fresh();
        fs::write(store.dir_for(&i).parent().unwrap().join(".DS_Store"), "").unwrap();
        fs::write(store.dir_for(&i).parent().unwrap().join("0123456789abcdef"), "").unwrap();
        assert_eq!(store.entries(), vec![i.clone()]);
        fs::remove_file(i.split_path(&cacache.join("content-v2"))).unwrap();
        assert_eq!(verify_entry(&store, &i), Found::NoTarball);
        let _ = fs::remove_dir_all(&d);
    }

    /// Two names one case-insensitive volume (default APFS) holds as one
    /// file: the entry has what extraction left, and that matches.
    #[test]
    fn verify_follows_the_filesystem_name_rules() {
        let d = scratch("verify-case");
        let cacache = d.join("cacache");
        let i = cached_tarball(&cacache, &[("README.md", 0o644, "upper\n"), ("readme.md", 0o644, "lower\n")]);
        let store = Store::new(d.join("store"), cacache).unwrap();
        assert!(matches!(store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted));
        assert!(matches!(verify_entry(&store, &i), crate::verify::Found::Match { .. }));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tmp_cleanup_keeps_locked_and_recent_dirs_whatever_the_pid() {
        let d = scratch("tmpclean");
        let tmp = d.join("store/tmp");
        // Named with this (live) process's pid: the pid rule would keep both.
        let held = TmpDir::create(tmp.join(format!("{}-held", std::process::id())), 0o700).unwrap();
        let left = tmp.join(format!("{}-left", std::process::id()));
        fs::create_dir_all(left.join("sub")).unwrap();
        let fresh = tmp.join("1-fresh");
        fs::create_dir_all(&fresh).unwrap();
        age(&held.path, 7200);
        age(&left, 7200);
        Store::new(d.join("store"), d.join("cacache")).unwrap();
        assert!(held.path.is_dir(), "a directory a live process holds is kept, however old");
        assert!(!left.exists(), "an old directory nobody holds is removed, whatever its pid");
        assert!(fresh.is_dir(), "a directory younger than the grace period is kept");
        drop(held);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn locks_of_entries_that_are_gone_are_removed() {
        let d = scratch("store-orphan-locks");
        let cacache = d.join("cacache");
        let kept = cached_tarball(&cacache, &[("package.json", 0o644, "{}")]);
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        store.ensure(&kept, "http://x/k.tgz").unwrap();
        drop(store.entry_lock(&kept, false).unwrap());
        let gone = Integrity { algo: "sha512".into(), hex: "ab".repeat(64) };
        drop(store.entry_lock(&gone, true).unwrap());
        drop(store);
        let store = exclusive(&d.join("store"), &cacache).unwrap();
        assert_eq!(store.remove_orphan_locks(), 1);
        let lock = |i: &Integrity| d.join("store/locks").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..]);
        assert!(lock(&kept).exists() && !lock(&gone).exists());
    }

    #[test]
    fn the_record_follows_a_complete_hidden_lockfile_and_drops_nothing_otherwise() {
        let d = scratch("store-update-record");
        let store = Store::new(d.join("store"), d.join("cacache")).unwrap();
        let root = d.join("project");
        let i = |c: &str| Integrity { algo: "sha512".into(), hex: c.repeat(128) };
        let (a, b, c, x) = (i("a"), i("b"), i("c"), i("e"));
        let keys = |v: &[&Integrity]| v.iter().map(|i| i.key()).collect::<std::collections::HashSet<String>>();
        let set = |v: &[&Integrity]| {
            let mut k: Vec<String> = v.iter().map(|i| i.key()).collect();
            k.sort();
            k
        };
        let rec = |r: &Path| {
            let mut v = store.recorded_entries(r).unwrap();
            v.sort();
            v
        };
        let at = |v: &[&Integrity]| {
            store.write_record(&root, v.iter().map(|i| i.key()).collect(), None).unwrap();
        };
        let (abc, ac) = (keys(&[&a, &b, &c]), keys(&[&a, &c]));
        // Complete, any command: exactly the tree. A record behind it is
        // completed; an entry that left the tree, or this run's of a package
        // npm rolled back, drops out.
        for ci in [false, true] {
            at(&[&a]);
            store.update_record(&root, std::slice::from_ref(&c), Tree::Complete(&abc), ci).unwrap();
            assert_eq!(rec(&root), set(&[&a, &b, &c]), "behind the tree, ci {ci}");
            store.update_record(&root, std::slice::from_ref(&x), Tree::Complete(&ac), ci).unwrap();
            assert_eq!(rec(&root), set(&[&a, &c]), "b left, x rolled back, ci {ci}");
        }
        // Incomplete, the install family: nothing is dropped.
        at(&[&a, &b]);
        store.update_record(&root, std::slice::from_ref(&x), Tree::Incomplete(&ac), false).unwrap();
        assert_eq!(rec(&root), set(&[&a, &b, &c, &x]));
        // Incomplete, npm ci: of the earlier record what the tree names, and
        // this run's.
        at(&[&a, &b]);
        store.update_record(&root, std::slice::from_ref(&x), Tree::Incomplete(&ac), true).unwrap();
        assert_eq!(rec(&root), set(&[&a, &x]));
        // No hidden lockfile: npm ci this run's; the family adds them.
        at(&[&a, &b]);
        store.update_record(&root, std::slice::from_ref(&c), Tree::Unknown, false).unwrap();
        assert_eq!(rec(&root), set(&[&a, &b, &c]));
        store.update_record(&root, std::slice::from_ref(&c), Tree::Unknown, true).unwrap();
        assert_eq!(rec(&root), set(&[&c]));
        // No record yet (a project plain npm installed): the tree, if complete.
        let other = d.join("other");
        store.update_record(&other, std::slice::from_ref(&b), Tree::Complete(&ac), false).unwrap();
        assert_eq!(rec(&other), set(&[&a, &c]));
        drop(store);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn removing_entries_waits_for_installs() {
        let d = scratch("storelock");
        let install = Store::new(d.join("store"), d.join("cacache")).unwrap();
        let e = Store::exclusive(d.join("store"), d.join("cacache")).err().expect("refused while an install holds the store");
        assert!(e.to_string().contains("in use"), "{e}");
        drop(install);
        let gc = exclusive(&d.join("store"), &d.join("cacache")).unwrap();
        drop(gc);
        let _ = fs::remove_dir_all(&d);
    }

    /// A store whose `lock` is a link (to a file elsewhere): installs and gc
    /// lock what it points to, and the re-check after taking the lock
    /// follows the link (it once compared the link itself, and every
    /// install gave up after 20 tries).
    #[test]
    fn a_lock_file_that_is_a_link() {
        let d = scratch("storelocklink");
        let root = d.join("store");
        fs::create_dir_all(&root).unwrap();
        fs::write(d.join("real-lock"), "").unwrap();
        std::os::unix::fs::symlink(d.join("real-lock"), root.join("lock")).unwrap();
        let install = Store::new(root.clone(), d.join("cacache")).unwrap();
        assert!(Store::exclusive(root.clone(), d.join("cacache")).is_err(), "gc refused while the install holds the lock");
        drop(install);
        drop(exclusive(&root, &d.join("cacache")).unwrap());
        let _ = fs::remove_dir_all(&d);
    }

    /// An install waiting on the lock while `uninstall` moves the store aside
    /// (holding the lock exclusively) opens a fresh store at the same path,
    /// and holds the lock file that is there now, not the one moved away.
    #[test]
    fn an_install_waiting_on_a_store_moved_aside_opens_a_fresh_one() {
        let d = scratch("storemoved");
        let root = d.join("store");
        drop(Store::new(root.clone(), d.join("cacache")).unwrap());
        fs::create_dir_all(root.join(LAYOUT).join("sha512/ab/cdef")).unwrap();
        let held = fs::OpenOptions::new().read(true).write(true).open(root.join("lock")).unwrap();
        // The install above released it; another test's child may hold a copy.
        retry_while_locked(|| flock(&held, libc::LOCK_EX | libc::LOCK_NB), |r| matches!(r, Err(e) if would_block(e))).unwrap();
        let (r, c) = (root.clone(), d.join("cacache"));
        let install = std::thread::spawn(move || Store::new(r, c).unwrap());
        // Long enough for the install to open the lock file and wait on it.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!install.is_finished(), "the install waits for the lock");
        fs::rename(&root, d.join("aside")).unwrap();
        drop(held);
        let store = install.join().unwrap();
        let lock = store._lock.as_ref().expect("locked");
        assert!(same_file(lock, &root.join("lock")), "holds the lock file now at the store's path");
        assert!(root.join(LAYOUT).is_dir() && !root.join(LAYOUT).join("sha512").exists(), "a fresh store");
        drop(store);
        let _ = fs::remove_dir_all(&d);
    }
    /// A decline holds for the build that recorded it only: another build's
    /// (or one with no build line) is void, retried, and rewritten when
    /// declined again. `declines` lists every file, with whether it is void.
    #[test]
    fn a_decline_holds_for_the_build_that_recorded_it() {
        let d = scratch("declines");
        let store = Store::new(d.join("store"), d.join("cacache")).unwrap();
        let (k1, k2) = ("fetcher:1 tar:1 uv:1", "fetcher:2 tar:2 uv:2");
        let f = store.decline_key_as(k1, "odd", "0.1.0 (aaaa)").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), format!("{k1}\nodd\nbuild: 0.1.0 (aaaa)\n"), "readable: key, why, build");
        assert_eq!(store.declined_by(k1, || "0.1.0 (aaaa)").as_deref(), Some("odd"), "the same build: declined");
        assert_eq!(store.declined_by(k1, || "0.1.0 (bbbb)"), None, "another build: void");
        // Declined again by the other build: the file is now that build's.
        store.decline_key_as(k1, "odd again", "0.1.0 (bbbb)").unwrap();
        assert_eq!(store.declined_by(k1, || "0.1.0 (bbbb)").as_deref(), Some("odd again"));
        assert_eq!(store.declined_by(k1, || "0.1.0 (aaaa)"), None);
        // A file with no build line (the format before builds were recorded):
        // another build, whatever this one is.
        fs::write(store.declined_path(k2), format!("{k2}\nold\n")).unwrap();
        assert_eq!(store.declined_by(k2, || "0.1.0 (bbbb)"), None);
        assert_eq!(store.declined(k2), None);
        // This build's own decline holds for it.
        store.decline_key(k2, "mine").unwrap();
        assert_eq!(store.declined(k2).as_deref(), Some("mine"));
        let all = store.declines();
        assert_eq!(all.iter().map(|x| (x.key.as_str(), x.void(), x.path == store.declined_path(&x.key))).collect::<Vec<_>>(), vec![(k1, true, true), (k2, false, true)]);
        assert_eq!(all[1].build.as_deref(), Some(build_identity()));
        // Deleting the file clears it.
        fs::remove_file(store.declined_path(k2)).unwrap();
        assert_eq!(store.declined(k2), None);
        let _ = fs::remove_dir_all(&d);
    }

    /// The rule on pacote's extraction (`odd_path`), on its own and in the
    /// comparison: every path a file or a directory, none more than
    /// EXTRACTION_TIME_SLACK (an hour) older than the install's start. No
    /// clone needed, so this runs everywhere (CI's ext4 included).
    #[test]
    fn the_extraction_rule_has_an_hour_of_slack_and_looks_at_every_path() {
        let d = scratch("extraction-rule");
        let started = std::time::SystemTime::now();
        let fresh = |name: &str| {
            let x = d.join(name);
            fs::create_dir_all(x.join("lib")).unwrap();
            fs::write(x.join("lib/a.js"), "a\n").unwrap();
            fs::write(x.join("package.json"), "{}\n").unwrap();
            x
        };
        let x = fresh("ok");
        assert_eq!(extraction_odd(&x, started).unwrap(), None);
        // Half an hour old: within the slack (a clock stepped back).
        age(&x.join("lib/a.js"), 1800);
        assert_eq!(extraction_odd(&x, started).unwrap(), None);
        assert_eq!(compare_extraction(&x, &x, started).unwrap(), Compared::Same);
        // An hour and a minute: not written by this install.
        age(&x.join("lib/a.js"), 3660);
        let odd = extraction_odd(&x, started).unwrap().unwrap();
        assert!(odd.starts_with("lib/a.js: modified at"), "{odd}");
        assert!(matches!(compare_extraction(&x, &fresh("other"), started).unwrap(), Compared::Unexpected(o) if o == odd));
        // A directory counts too (a placed clone's directories are touched).
        let y = fresh("dir");
        age(&y.join("lib"), 3660);
        assert!(extraction_odd(&y, started).unwrap().unwrap().starts_with("lib: modified at"));
        // A symlink: not reproduced.
        let z = fresh("link");
        std::os::unix::fs::symlink("a.js", z.join("lib/b.js")).unwrap();
        assert_eq!(extraction_odd(&z, started).unwrap().as_deref(), Some("lib/b.js: neither a file nor a directory"));
        assert!(matches!(compare_extraction(&z, &z, started).unwrap(), Compared::Unexpected(_)));
        let _ = fs::remove_dir_all(&d);
    }
}
