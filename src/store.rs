//! Machine-wide store of EXTRACTED packages, keyed by integrity.
//!
//! Layout: `<store>/v1/<algo>/<hex[0..2]>/<hex[2..]>/` holds the package root
//! (the tarball's first path component stripped, as pacote does). An entry is
//! written to `<store>/tmp/` and renamed into place, so a present directory is
//! always complete; it is removed the same way in reverse (renamed out, then
//! deleted).
//!
//! Also under the store: `sizes/` (cached entry sizes), `projects/` (one
//! record per installed project, for `thinclone store gc`) and `lock` (held
//! shared by every install, exclusively by commands that remove entries).
//!
//! Tarballs come from npm's own content-addressed cache (`~/.npm/_cacache`).

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use sha2::Digest;
use std::collections::BTreeMap;
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
        let lf = fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join("lock"))?;
        let lock = if exclusive {
            match flock(&lf, libc::LOCK_EX | libc::LOCK_NB) {
                Ok(()) => Some(lf),
                Err(e) if would_block(&e) => bail!("the store is in use by a running install; try again when it has finished"),
                Err(e) => bail!("cannot lock the store ({e}); refusing to remove entries without it"),
            }
        } else {
            match flock(&lf, libc::LOCK_SH | libc::LOCK_NB) {
                Ok(()) => Some(lf),
                Err(e) if would_block(&e) => {
                    eprintln!("thinclone: waiting for a `thinclone store` command to release the store");
                    flock(&lf, libc::LOCK_SH).context("waiting for the store lock")?;
                    Some(lf)
                }
                // No flock on this filesystem: installs go on unguarded, as before.
                Err(_) => None,
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
        for e in walkdir::WalkDir::new(&base).min_depth(3).max_depth(3).into_iter().flatten() {
            let rel = e.path().strip_prefix(&base).unwrap();
            let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if let [algo, a, b] = parts.as_slice() {
                v.push(Integrity { algo: algo.clone(), hex: format!("{a}{b}") });
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
    fn find_tarball(&self, i: &Integrity, url: Option<&str>) -> Option<PathBuf> {
        let direct = i.split_path(&self.cacache.join("content-v2"));
        if direct.is_file() {
            return Some(direct);
        }
        let key = format!("make-fetch-happen:request-cache:{}", url?);
        let kh = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let idx = self.cacache.join("index-v5").join(&kh[0..2]).join(&kh[2..4]).join(&kh[4..]);
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
        let p = ci.split_path(&self.cacache.join("content-v2"));
        p.is_file().then_some(p)
    }

    pub fn ensure(&self, i: &Integrity, url: &str) -> Result<Ensure> {
        let dest = self.dir_for(i);
        if dest.is_dir() {
            return Ok(Ensure::Present);
        }
        let Some(tgz) = self.find_tarball(i, Some(url)) else { return Ok(Ensure::Missing) };
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
            Ok(()) => Ok(Ensure::Extracted),
            Err(_) if dest.is_dir() => {
                // Another process won the race; its copy is equally valid.
                let _ = fs::remove_dir_all(&tmp.path);
                Ok(Ensure::Present)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Compare an entry with what its tarball in the npm cache extracts to:
    /// the same files, contents, modes and directories, nothing else. Catches
    /// an entry left truncated or changed (entries are renamed into place
    /// without fsync, and a clone tool or a person could write into one).
    /// Found by content address only; an entry whose lockfile had only sha1
    /// (cacache keeps those under sha512) reads as NoTarball.
    pub fn verify_entry(&self, i: &Integrity) -> Result<Check> {
        let Some(tgz) = self.find_tarball(i, None) else { return Ok(Check::NoTarball) };
        let data = fs::read(&tgz)?;
        if i.digest(&data) != i.hex {
            return Ok(Check::NoTarball);
        }
        let mut want: BTreeMap<PathBuf, (u32, blake3::Hash)> = BTreeMap::new();
        tar_files(&data, |rel, mode, r| {
            // A later archive entry replaces an earlier one at the same path
            // (extract removes what is there, a directory included).
            want.retain(|p, _| !p.starts_with(rel));
            let mut h = blake3::Hasher::new();
            std::io::copy(r, &mut h)?;
            want.insert(rel.to_path_buf(), (mode, h.finalize()));
            Ok(())
        })?;
        let mut want_dirs: std::collections::BTreeSet<PathBuf> = Default::default();
        for p in want.keys() {
            let mut a = p.parent();
            while let Some(d) = a.filter(|d| !d.as_os_str().is_empty()) {
                want_dirs.insert(d.to_path_buf());
                a = d.parent();
            }
        }
        let dir = self.dir_for(i);
        let mut seen_dirs: std::collections::BTreeSet<PathBuf> = Default::default();
        let mut seen = 0usize;
        for e in walkdir::WalkDir::new(&dir).min_depth(1).follow_links(false) {
            let e = e?;
            let rel = e.path().strip_prefix(&dir).unwrap().to_path_buf();
            let ft = e.file_type();
            if ft.is_dir() {
                if !want_dirs.contains(&rel) {
                    return Ok(Check::Differs(format!("{}/: directory not in the tarball", rel.display())));
                }
                seen_dirs.insert(rel);
            } else if ft.is_file() {
                let Some((mode, hash)) = want.get(&rel) else {
                    return Ok(Check::Differs(format!("{}: file not in the tarball", rel.display())));
                };
                let m = e.metadata()?.permissions().mode() & 0o7777;
                if m != *mode {
                    return Ok(Check::Differs(format!("{}: mode {m:o}, the tarball gives {mode:o}", rel.display())));
                }
                let mut h = blake3::Hasher::new();
                std::io::copy(&mut fs::File::open(e.path())?, &mut h)?;
                if h.finalize() != *hash {
                    return Ok(Check::Differs(format!("{}: contents differ", rel.display())));
                }
                seen += 1;
            } else {
                return Ok(Check::Differs(format!("{}: not a regular file or directory", rel.display())));
            }
        }
        if seen != want.len() {
            let missing = want.keys().find(|p| !dir.join(p).is_file()).map(|p| p.display().to_string()).unwrap_or_default();
            return Ok(Check::Differs(format!("{missing}: missing ({} of {} files present)", seen, want.len())));
        }
        if seen_dirs.len() != want_dirs.len() {
            return Ok(Check::Differs("a directory is missing".into()));
        }
        Ok(Check::Match)
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

/// The files of a package tarball as pacote writes them
/// (npm/node_modules/pacote/lib/fetcher.js `#tarxOptions`), in archive order:
/// the first path component stripped; FILE entries only (links and directory
/// entries are filtered out, so empty directories never appear; directories
/// exist only as parents of files); `..` refused; a `.gitignore` renamed to
/// `.npmignore` unless a `.npmignore` at that path was seen earlier in the
/// archive, in which case it is dropped; mode `((mode | 0o666) & ~umask) |
/// 0o600`. The one definition of these rules: `extract` writes what it
/// yields, `Store::verify_entry` compares against it.
fn tar_files(data: &[u8], mut f: impl FnMut(&Path, u32, &mut dyn Read) -> Result<()>) -> Result<()> {
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
        let mode = ent.header().mode().unwrap_or(0o644);
        let m = (((mode | 0o666) & !UMASK) | 0o600) & 0o7777;
        f(&rel, m, &mut ent)?;
    }
    Ok(())
}

/// Extract like pacote (the rules are in `tar_files`). Executable bits for
/// declared bins are added later by npm's bin linking, as in `npm ci`.
pub fn extract(data: &[u8], dest: &Path) -> Result<()> {
    tar_files(data, |rel, mode, r| {
        let out = dest.join(rel);
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
        let mut f = fs::File::create(&out)?;
        std::io::copy(r, &mut f)?;
        drop(f);
        fs::set_permissions(&out, fs::Permissions::from_mode(mode))?;
        Ok(())
    })
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
        assert!(differs("extra").contains("extra.js: file not in the tarball"));
        let e = fresh();
        fs::set_permissions(e.join("index.js"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(differs("mode").contains("mode 755"));
        let e = fresh();
        fs::create_dir(e.join("empty")).unwrap();
        assert!(differs("empty dir").contains("empty/"));
        let e = fresh();
        std::os::unix::fs::symlink("index.js", e.join("link")).unwrap();
        assert!(differs("symlink").contains("link"));
        fs::remove_file(i.split_path(&cacache.join("content-v2"))).unwrap();
        assert_eq!(store.verify_entry(&i).unwrap(), Check::NoTarball);
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
