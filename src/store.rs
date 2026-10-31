//! Machine-wide store of EXTRACTED packages, keyed by integrity.
//!
//! Layout: `<store>/v1/<algo>/<hex[0..2]>/<hex[2..]>/` holds the package root
//! (the tarball's first path component stripped, as pacote does). An entry is
//! written to `<store>/tmp/` and renamed into place, so a present directory is
//! always complete.
//!
//! Tarballs come from npm's own content-addressed cache (`~/.npm/_cacache`).

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use sha2::Digest;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
            let i = Integrity { algo: algo.to_string(), hex: hex::encode(bytes) };
            if best.as_ref().map_or(true, |(r, _)| rank < *r) {
                best = Some((rank, i));
            }
        }
        best.map(|(_, i)| i).ok_or_else(|| anyhow!("no usable hash in integrity {s:?}"))
    }

    fn split_path(&self, base: &Path) -> PathBuf {
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

pub struct Store {
    pub root: PathBuf,
    pub cacache: PathBuf,
    tmp_seq: AtomicU64,
}

pub enum Ensure {
    Present,
    Extracted,
    /// No tarball in the npm cache; the caller must fetch it.
    Missing,
}

impl Store {
    pub fn new(root: PathBuf, cacache: PathBuf) -> Result<Store> {
        fs::create_dir_all(root.join(LAYOUT))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Store { root, cacache, tmp_seq: AtomicU64::new(0) })
    }

    pub fn dir_for(&self, i: &Integrity) -> PathBuf {
        self.root.join(LAYOUT).join(&i.algo).join(&i.hex[0..2]).join(&i.hex[2..])
    }

    /// Locate the tarball in npm's cache: first by content address, then via the
    /// index entry for the request URL (needed when the lockfile only has sha1:
    /// cacache stores content under the sha512 it computed itself).
    fn find_tarball(&self, i: &Integrity, url: &str) -> Option<PathBuf> {
        let direct = i.split_path(&self.cacache.join("content-v2"));
        if direct.is_file() {
            return Some(direct);
        }
        let key = format!("make-fetch-happen:request-cache:{url}");
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
        let Some(tgz) = self.find_tarball(i, url) else { return Ok(Ensure::Missing) };
        let data = fs::read(&tgz)?;
        let got = i.digest(&data);
        if got != i.hex {
            bail!("integrity mismatch for {url}: cache file {} has {}-{got}", tgz.display(), i.algo);
        }
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join("tmp").join(format!("{}-{seq}-{}", std::process::id(), &i.hex[..16]));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        extract(&data, &tmp).with_context(|| format!("extracting {url}"))?;
        fs::create_dir_all(dest.parent().unwrap())?;
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok(Ensure::Extracted),
            Err(_) if dest.is_dir() => {
                // Another process won the race; its copy is equally valid.
                let _ = fs::remove_dir_all(&tmp);
                Ok(Ensure::Present)
            }
            Err(e) => Err(e.into()),
        }
    }
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
