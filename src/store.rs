//! Machine-wide store of EXTRACTED packages, keyed by integrity.
//!
//! Layout: `<store>/v1/<algo>/<hex[0..2]>/<hex[2..]>/` holds the package root
//! (the tarball's first path component stripped, as pacote does). An entry is
//! written to `<store>/tmp/` and renamed into place, so a present directory is
//! always complete; it is removed the same way in reverse (renamed out, then
//! deleted).
//!
//! Also under the store: `sizes/` (cached entry sizes), `projects/` (one
//! record per installed project, for `thinclone store gc`), `lock` (held
//! shared by every install, exclusively by commands that remove entries), and
//! for the inject engine `made-by/` (per entry: the pacote and tar versions
//! that made it or were last found to extract it the same way) and
//! `inject-checked` / `inject-strict` (one line per pacote and tar version;
//! see serve.rs).
//!
//! Tarballs come from npm's own content-addressed cache (`~/.npm/_cacache`).

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use sha2::Digest;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Read the newest cacache entry for `key` (npm's content cache): the content
/// bytes, located through the index.
pub fn cacache_read(cacache: &Path, key: &str) -> Option<Vec<u8>> {
    let kh = hex::encode(sha2::Sha256::digest(key.as_bytes()));
    let idx = cacache.join("index-v5").join(&kh[0..2]).join(&kh[2..4]).join(&kh[4..]);
    let text = fs::read_to_string(idx).ok()?;
    let mut found = None;
    for line in text.lines() {
        let Some((_, json)) = line.split_once('\t') else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { continue };
        if v.get("key").and_then(|k| k.as_str()) != Some(key) {
            continue;
        }
        // A null integrity is a deletion record.
        found = v.get("integrity").and_then(|x| x.as_str()).map(String::from);
    }
    let ci = Integrity::parse(&found?).ok()?;
    fs::read(ci.split_path(&cacache.join("content-v2"))).ok()
}

fn flock(f: &fs::File, op: libc::c_int) -> std::io::Result<()> {
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

fn unsupported(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(n) if n == libc::ENOTSUP || n == libc::EOPNOTSUPP)
}

fn would_block(e: &std::io::Error) -> bool {
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

pub enum Ensure {
    Present,
    Extracted,
    /// No tarball in the npm cache; the caller must fetch it.
    Missing,
}

/// What `verify_entry` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Match,
    /// The entry is not what its tarball extracts to (the first difference).
    Differs(String),
    /// The tarball is no longer in the npm cache (or no longer matches its
    /// integrity): nothing to compare with.
    NoTarball,
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
                    eprintln!("thinclone: waiting for a `thinclone store` command to release the store");
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
        clean_tmp(&root.join("tmp"))?;
        Ok(Store { root, cacache, tmp_seq: AtomicU64::new(0), _lock: lock })
    }

    pub fn dir_for(&self, i: &Integrity) -> PathBuf {
        self.root.join(LAYOUT).join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
    }

    fn size_file(&self, i: &Integrity) -> PathBuf {
        self.root.join("sizes").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
    }

    fn made_by_file(&self, i: &Integrity) -> PathBuf {
        self.root.join("made-by").join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
    }

    /// The pacote and tar versions (`pacote@… tar@…`) that extracted this
    /// entry, or were last found to extract its tarball to the same tree.
    /// None: made by thinclone's own extractor (the classic engine), or
    /// before this was recorded.
    pub fn made_by(&self, i: &Integrity) -> Option<String> {
        fs::read_to_string(self.made_by_file(i)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn forget_made_by(&self, i: &Integrity) {
        let _ = fs::remove_file(self.made_by_file(i));
    }

    pub fn set_made_by(&self, i: &Integrity, key: &str) -> Result<()> {
        let f = self.made_by_file(i);
        fs::create_dir_all(f.parent().unwrap())?;
        let tmp = f.with_extension(format!("{}-{}", process_tag(), self.tmp_seq.fetch_add(1, Ordering::Relaxed)));
        fs::write(&tmp, format!("{key}\n"))?;
        fs::rename(&tmp, &f)?;
        Ok(())
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
    pub fn remove_entry(&self, i: &Integrity) -> Result<()> {
        let src = self.dir_for(i);
        let f = fs::File::open(&src).with_context(|| format!("opening {}", src.display()))?;
        let _ = flock(&f, libc::LOCK_EX | libc::LOCK_NB);
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let trash = self.root.join("tmp").join(format!("{}-gc{seq}", process_tag()));
        fs::rename(&src, &trash).with_context(|| format!("moving {} out of the store", src.display()))?;
        let _ = fs::remove_file(self.size_file(i));
        let _ = fs::remove_file(self.made_by_file(i));
        fs::remove_dir_all(&trash).with_context(|| format!("removing {}", trash.display()))?;
        Ok(())
    }

    fn project_file(&self, root: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let h = blake3::hash(root.as_os_str().as_bytes());
        self.root.join("projects").join(format!("{}.json", &h.to_hex()[..32]))
    }

    /// Record which entries the project at `root` (canonical) was installed
    /// from, for `thinclone store gc`. Replaces the project's earlier record.
    pub fn record_project(&self, root: &Path, used: &[Integrity]) -> Result<()> {
        let mut keys: Vec<String> = used.iter().map(Integrity::key).collect();
        keys.sort();
        keys.dedup();
        let rec = serde_json::json!({
            "path": root,
            "ts": crate::trial::now(),
            "build": env!("THINCLONE_BUILD"),
            "entries": keys,
        });
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

    /// The entry for `i`, extracted by thinclone's own extractor from the
    /// tarball in npm's cache when missing (the classic engine; the inject
    /// engine's entries are extracted by pacote, serve.rs). The tarball's
    /// digest is checked against `i` before anything is extracted.
    pub fn ensure(&self, i: &Integrity, url: &str) -> Result<Ensure> {
        let cacache = &self.cacache;
        let dest = self.dir_for(i);
        match fs::symlink_metadata(&dest) {
            Ok(m) if m.is_dir() => return Ok(Ensure::Present),
            // A directory clone of a symlink places the symlink, not a package.
            Ok(_) => bail!("store entry {} is not a directory; `thinclone store verify --fix` removes it", dest.display()),
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
        match fs::rename(&tmp.path, &dest) {
            Ok(()) => {
                // Made by thinclone's extractor: a record left by an entry
                // that was here before is not about this one.
                self.forget_made_by(i);
                Ok(Ensure::Extracted)
            }
            Err(_) if dest.is_dir() => {
                // Another process won the race; its copy is equally valid.
                let _ = fs::remove_dir_all(&tmp.path);
                Ok(Ensure::Present)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Compare an entry with what its tarball in the npm cache extracts to:
    /// the tarball is extracted again into tmp/ on the same filesystem, and
    /// the two trees must hold the same files (contents, modes), the same
    /// directories and nothing else; the entry itself must be a directory.
    /// Directory modes are not compared: they follow the umask of the process
    /// that extracted the entry (as npm's do), so the tarball does not fix
    /// them, and a verify under another umask would condemn every entry.
    /// File modes are set explicitly and are compared. Extracting rather than
    /// modelling the result keeps the filesystem's own name rules (a
    /// case-insensitive volume keeps one of `README.md` and `readme.md`).
    /// Catches an entry left truncated or changed (entries are renamed into
    /// place without fsync, and a person or a tool could write into one).
    /// Found by content address only; an entry whose lockfile had only sha1
    /// (cacache keeps those under sha512) reads as NoTarball.
    pub fn verify_entry(&self, i: &Integrity) -> Result<Check> {
        let dir = self.dir_for(i);
        let m = fs::symlink_metadata(&dir).with_context(|| format!("reading {}", dir.display()))?;
        if !m.is_dir() {
            return Ok(Check::Differs("the entry is not a directory (a symlink?)".into()));
        }
        let Some(tgz) = self.find_tarball(&self.cacache, i, None) else { return Ok(Check::NoTarball) };
        let data = fs::read(&tgz)?;
        if i.digest(&data) != i.hex {
            return Ok(Check::NoTarball);
        }
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let fresh = TmpDir::create(self.root.join("tmp").join(format!("{}-v{seq}-{}", process_tag(), &i.hex[..16])), 0o755)?;
        let r = extract(&data, &fresh.path)
            .with_context(|| format!("extracting {}", tgz.display()))
            .and_then(|()| Ok(match compare_trees(&fresh.path, &dir, false)? {
                Some(d) => Check::Differs(d),
                None => Check::Match,
            }));
        let _ = fs::remove_dir_all(&fresh.path);
        r
    }

    /// Forget an entry's cached size (its contents were replaced).
    pub fn forget_size(&self, i: &Integrity) {
        let _ = fs::remove_file(self.size_file(i));
    }

    /// Lines of `<store>/<file>` whose first tab-separated field is `key`
    /// (the inject engine's `inject-checked` / `inject-blocked`).
    pub fn key_lines(&self, file: &str, key: &str) -> Vec<String> {
        fs::read_to_string(self.root.join(file))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.split('\t').next() == Some(key))
            .map(String::from)
            .collect()
    }

    /// Append one line `key<TAB>date<TAB>note` to `<store>/<file>`, in one
    /// write (O_APPEND), so concurrent installs never interleave lines.
    pub fn add_key_line(&self, file: &str, key: &str, note: &str) -> Result<PathBuf> {
        use std::io::Write;
        let p = self.root.join(file);
        let note: String = note.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).collect();
        let line = format!("{key}\t{}\t{note}\n", crate::trial::now());
        fs::OpenOptions::new().create(true).append(true).open(&p)?.write_all(line.as_bytes())?;
        Ok(p)
    }
}

/// The first difference between two extracted trees, `want` the reference
/// (what the tarball extracts to) and `got` the one checked: the same files
/// (contents, modes), the same directories and nothing else. Directory modes
/// are compared only with `dir_modes`: `verify_entry` leaves them out (they
/// follow the umask of whoever extracted), the inject engine compares them
/// (its umask is gated to 022, and a clone carries them into the project).
/// None when they are the same.
pub fn compare_trees(want: &Path, got: &Path, dir_modes: bool) -> Result<Option<String>> {
    let (want, got) = (tree(want, dir_modes)?, tree(got, dir_modes)?);
    Ok(match want.iter().zip(got.iter()).find(|(w, g)| w != g) {
        Some((w, g)) => Some(describe(w, g)),
        None if want.len() != got.len() => {
            let (longer, what) = if got.len() > want.len() { (&got, "not in the tarball") } else { (&want, "missing") };
            let p = &longer[want.len().min(got.len())].0;
            Some(format!("{}: {what}", show_rel(p)))
        }
        None => None,
    })
}

/// One path in a tree: its kind, its mode (a directory's only when compared),
/// and for a file its contents' hash.
#[derive(PartialEq, Eq)]
enum Node {
    Dir(Option<u32>),
    File(u32, blake3::Hash),
    Other,
}

/// Every path under `root` (`root` itself as ""), sorted, symlinks not followed.
fn tree(root: &Path, dir_modes: bool) -> Result<Vec<(PathBuf, Node)>> {
    let mut v = vec![];
    for e in walkdir::WalkDir::new(root).follow_links(false).follow_root_links(false).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
        let ft = e.file_type();
        let mode = e.metadata()?.permissions().mode() & 0o7777;
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
    Ok(v)
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

const UMASK: u32 = 0o022;

/// Store layout / extraction-rules version. Bump whenever `extract` changes what
/// lands on disk, so entries made under old rules are never reused.
/// v1 → v2: drop directory entries (pacote only creates parents of files) and
/// the `.gitignore` → `.npmignore` rename.
pub const LAYOUT: &str = "v2";

/// Extract like pacote (npm/node_modules/pacote/lib/fetcher.js `#tarxOptions`):
/// strip the first path component; keep FILE entries only (links and directory
/// entries are filtered out, so empty directories never appear; directories
/// exist only as parents of files, mode 0o755); refuse `..`; a `.gitignore` is
/// renamed to `.npmignore` unless a `.npmignore` at that path was seen earlier in
/// the archive, in which case it is dropped; files get
/// `((mode | 0o666) & ~umask) | 0o600`. Executable bits for declared bins are
/// added later by npm's bin linking, as in `npm ci`.
pub fn extract(data: &[u8], dest: &Path) -> Result<()> {
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

    /// A fresh directory under the system temp dir, removed first if a
    /// previous run left it.
    pub fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("thinclone-test-{}-{name}", std::process::id()));
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

    #[test]
    fn verify_finds_changed_missing_and_extra_files_and_modes() {
        let d = scratch("verify");
        let cacache = d.join("cacache");
        let i = cached_tarball(&cacache, &[("index.js", 0o644, "module.exports = 1\n"), ("bin/cli", 0o755, "#!/bin/sh\n"), (".gitignore", 0o644, "x\n")]);
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        let fresh = || {
            if store.dir_for(&i).exists() {
                fs::remove_dir_all(store.dir_for(&i)).unwrap();
            }
            assert!(matches!(store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted));
            assert_eq!(store.verify_entry(&i).unwrap(), Check::Match, "a fresh extraction matches");
            store.dir_for(&i)
        };
        let differs = |what: &str| match store.verify_entry(&i).unwrap() {
            Check::Differs(why) => why,
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
        // Directory modes follow the extracting process's umask: not a difference.
        let e = fresh();
        fs::set_permissions(e.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&e, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(store.verify_entry(&i).unwrap(), Check::Match);
        fs::set_permissions(&e, fs::Permissions::from_mode(0o755)).unwrap();
        // The entry root replaced by a symlink to a valid copy.
        let e = fresh();
        let moved = d.join("moved");
        fs::rename(&e, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &e).unwrap();
        assert!(differs("root symlink").contains("not a directory"));
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
        assert_eq!(store.verify_entry(&i).unwrap(), Check::NoTarball);
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
        assert_eq!(store.verify_entry(&i).unwrap(), Check::Match);
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
    fn removing_entries_waits_for_installs() {
        let d = scratch("storelock");
        let install = Store::new(d.join("store"), d.join("cacache")).unwrap();
        let e = Store::exclusive(d.join("store"), d.join("cacache")).err().expect("refused while an install holds the store");
        assert!(e.to_string().contains("in use"), "{e}");
        drop(install);
        let gc = Store::exclusive(d.join("store"), d.join("cacache")).unwrap();
        drop(gc);
        let _ = fs::remove_dir_all(&d);
    }
}
