//! `matchapack store verify [--fix]`: every store entry compared with what
//! npm's own pacote extracts from its tarball (NOTES "PR C1").
//!
//! The inject engine, the default since PR C1, places store entries in
//! place of pacote's extraction, and checks entries against that extraction
//! before it uses them (serve.rs). So the store is judged by pacote here too,
//! with the same rules as the inject check:
//! - pacote is the running npm's (the npm on PATH), driven by
//!   src/store-verify.js the way the hook drives it (a registry tarball's
//!   fetcher with npm's default options, umask 022, the tarball read from
//!   npm's cache by its integrity; the hook's own eligibility rule is
//!   asserted on the fetcher);
//! - the comparison is `store::compare_trees` with directory modes, as the
//!   inject check's;
//! - an entry that matches gets this pacote/tar version added to its made-by
//!   record (the next inject install then samples it instead of comparing it
//!   first), as a check that matched does;
//! - an entry that differs is classified by who made it (`extractor`: no
//!   record, i.e. the classic engine's Rust extractor; `changed`: this
//!   version had made or vouched for it; `pacote`: only other versions had)
//!   and, with `--fix`, replaced by pacote's extraction, recorded as this
//!   version's, under the entry's exclusive lock: what a differing check
//!   does in an install.
//!
//! The classic engine's Rust extractor is not used: an entry it made (the
//! engine was removed in PR C2; older stores keep its entries) is judged by
//! pacote like any other (it has an empty record, so it shows as `extractor`
//! when it differs).

use crate::store::{compare_trees, process_tag, rename_noreplace, Integrity, Store, TmpDir};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// What verifying one entry found (and, with `fix`, did).
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// The same tree as pacote's extraction. `recorded`: this version was
    /// added to the entry's made-by record.
    Match { recorded: bool },
    /// Not what pacote extracts: who made it (module comment), the first
    /// difference, and whether it was replaced by pacote's extraction.
    Differs { kind: &'static str, why: String, fixed: bool },
    /// The entry is not a directory (a symlink?); removed with `fix`.
    NotDir { fixed: bool },
    /// npm's cache no longer holds the tarball (or other bytes under its
    /// integrity, or the entry's integrity is sha1 only: cacache keeps those
    /// under sha512): nothing to compare with.
    NoTarball,
    /// pacote could not extract the tarball.
    Failed(String),
}

/// Compare one entry with `extract`'s extraction of its tarball (pacote's;
/// the tests stand in matchapack's own extractor). `key`: the pacote and tar
/// versions that extract (`pacote@… tar@…`).
pub fn verify_entry(store: &Store, key: &str, i: &Integrity, extract: &dyn Fn(&Integrity, &Path) -> Result<()>, fix: bool) -> Result<Found> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = store.dir_for(i);
    let m = std::fs::symlink_metadata(&dir).with_context(|| format!("reading {}", dir.display()))?;
    if !m.is_dir() {
        if fix {
            store.remove_entry(i)?;
        }
        return Ok(Found::NotDir { fixed: fix });
    }
    if !store.cached_tarball_ok(i)? {
        return Ok(Found::NoTarball);
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    // Made here under umask 022 (verify sets it), as the inject engine's
    // server makes the directory pacote extracts into.
    let fresh = TmpDir::create(store.root.join("tmp").join(format!("{}-v{seq}-{}", process_tag(), &i.hex[..16])), 0o755)?;
    let discard = || {
        let _ = std::fs::remove_dir_all(&fresh.path);
    };
    if let Err(e) = extract(i, &fresh.path) {
        discard();
        return Ok(Found::Failed(format!("{e:#}")));
    }
    let ino = |p: &Path| std::fs::symlink_metadata(p).ok().filter(|m| m.is_dir()).map(|m| std::os::unix::fs::MetadataExt::ino(&m));
    // Shared while comparing: an install does not replace the entry meanwhile.
    let (made_by, ino_before, differs) = {
        let _shared = store.entry_lock(i, false)?;
        let made_by = store.made_by(i);
        let ino_before = ino(&dir);
        let differs = compare_trees(&fresh.path, &dir, true);
        (made_by, ino_before, differs)
    };
    let differs = match differs {
        Ok(d) => d,
        Err(e) => {
            discard();
            return Err(e.context(format!("comparing {}", dir.display())));
        }
    };
    let known = !key.contains('?');
    let Some(why) = differs else {
        discard();
        let mut recorded = false;
        if known && !made_by.iter().any(|k| k == key) {
            // Exclusive, and only for the tree that was compared.
            let _excl = store.entry_lock(i, true)?;
            if ino(&dir) == ino_before && store.made_by(i) == made_by {
                store.add_made_by(i, key)?;
                recorded = true;
            }
        }
        return Ok(Found::Match { recorded });
    };
    let kind = crate::serve::differs_kind(&made_by, key);
    if !fix {
        discard();
        return Ok(Found::Differs { kind, why, fixed: false });
    }
    // Replaced by pacote's extraction, as an install's check replaces it.
    let _excl = store.entry_lock(i, true)?;
    let aside = store.root.join("tmp").join(format!("{}-v{seq}-old", process_tag()));
    store.forget_made_by(i);
    store.forget_size(i);
    let moved = std::fs::rename(&dir, &aside).and_then(|()| rename_noreplace(&fresh.path, &dir));
    if let Err(e) = moved {
        discard();
        let _ = std::fs::remove_dir_all(&aside);
        bail!("replacing {}: {e}", dir.display());
    }
    if known {
        store.set_made_by(i, key)?;
    }
    let _ = std::fs::remove_dir_all(&aside);
    Ok(Found::Differs { kind, why, fixed: true })
}

/// The running npm's pacote, in a node child (src/store-verify.js), taking
/// extraction jobs from any thread.
pub struct Pacote {
    child: Child,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, Sender<std::result::Result<(), String>>>>>,
    next: AtomicU64,
    reader: Option<std::thread::JoinHandle<()>>,
    /// `pacote@… tar@…`, from the helper's hello.
    pub key: String,
}

impl Pacote {
    /// Start the helper for the npm installed at `npm_dir`, reading tarballs
    /// from `cacache`; its files go into `run_dir`.
    pub fn start(npm_dir: &Path, cacache: &Path, run_dir: &Path, threads: usize) -> Result<Pacote> {
        for (name, text) in [("inject.js", include_str!("inject.js")), ("store-verify.js", include_str!("store-verify.js"))] {
            let p = run_dir.join(name);
            let _ = std::fs::remove_file(&p);
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p)?;
            f.write_all(text.as_bytes())?;
        }
        let helper = run_dir.join("store-verify.js");
        let spawned = crate::util::npm_node()
            .command()
            .arg(&helper)
            .arg(npm_dir)
            .arg(cacache)
            .env("MATCHAPACK", "0")
            // tar's zlib work runs on libuv's pool (4 threads by default).
            .env("UV_THREADPOOL_SIZE", threads.clamp(4, 64).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        // node has read its files once it answers (or failed); either way
        // they are not needed after the hello.
        let cleanup = || {
            let _ = std::fs::remove_file(run_dir.join("inject.js"));
            let _ = std::fs::remove_file(&helper);
        };
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                cleanup();
                bail!("could not start node for npm's pacote: {e}");
            }
        };
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut hello = String::new();
        let read = out.read_line(&mut hello);
        cleanup();
        let v: serde_json::Value = match read {
            Ok(n) if n > 0 => serde_json::from_str(&hello).unwrap_or_default(),
            _ => serde_json::Value::Null,
        };
        if v.get("hello").is_none() {
            let _ = child.kill();
            let status = child.wait().ok();
            bail!("npm's pacote could not be loaded from {} (the helper exited: {status:?})", npm_dir.display());
        }
        let ver = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("?").to_string();
        let key = format!("pacote@{} tar@{}", ver("pacote"), ver("tar"));
        let pending: Arc<Mutex<HashMap<u64, Sender<std::result::Result<(), String>>>>> = Arc::default();
        let p2 = pending.clone();
        let reader = std::thread::spawn(move || {
            for line in out.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let Some(id) = v.get("id").and_then(|x| x.as_u64()) else { continue };
                let r = if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
                    Ok(())
                } else {
                    Err(v.get("error").and_then(|x| x.as_str()).unwrap_or("unknown error").to_string())
                };
                if let Some(tx) = p2.lock().unwrap().remove(&id) {
                    let _ = tx.send(r);
                }
            }
            // The helper is gone: whoever still waits gets an error.
            p2.lock().unwrap().clear();
        });
        let stdin = child.stdin.take();
        Ok(Pacote { child, stdin: Mutex::new(stdin), pending, next: AtomicU64::new(0), reader: Some(reader), key })
    }

    /// pacote's extraction of the tarball of `i` (from npm's cache) into
    /// `dir`; blocks until it is done.
    pub fn extract(&self, i: &Integrity, dir: &Path) -> Result<()> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = serde_json::json!({ "id": id, "integrity": i.sri(), "dir": dir }).to_string() + "\n";
        {
            let mut w = self.stdin.lock().unwrap();
            let sent = w.as_mut().map(|s| s.write_all(line.as_bytes()).and_then(|()| s.flush()));
            if !matches!(sent, Some(Ok(()))) {
                self.pending.lock().unwrap().remove(&id);
                bail!("npm's pacote helper is gone");
            }
        }
        match rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!("pacote: {e}")),
            Err(_) => Err(anyhow!("npm's pacote helper is gone")),
        }
    }
}

impl Drop for Pacote {
    fn drop(&mut self) {
        // End of input: the helper answers what it has and exits.
        self.stdin.lock().unwrap().take();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// The counts of a verify run, and the entries that differed.
#[derive(Default)]
pub struct Report {
    pub entries: usize,
    pub matched: usize,
    pub recorded: usize,
    pub no_tarball: usize,
    /// (entry, kind, first difference, replaced)
    pub differs: Vec<(Integrity, &'static str, String, bool)>,
    /// (entry, removed)
    pub not_dir: Vec<(Integrity, bool)>,
    pub failed: Vec<(Integrity, String)>,
    pub secs: f64,
}

/// Verify every entry of `store` with the npm on PATH's pacote.
pub fn verify_store(store: &Store, cacache: &Path, fix: bool) -> Result<(Report, String)> {
    use rayon::prelude::*;
    let t0 = std::time::Instant::now();
    // pacote's files and directories follow the process umask; the inject
    // engine runs only under 022 (its gate), so its extraction is made so.
    unsafe { libc::umask(0o022) };
    let npm_dir = crate::util::npm_dir().context("store verify extracts with npm's pacote: no npm installation on PATH")?;
    let threads = crate::inject::threads();
    let pacote = Pacote::start(&npm_dir, cacache, &crate::util::private_run_dir()?, threads)?;
    let key = pacote.key.clone();
    let all = store.entries();
    // Each worker waits on node for its extraction, then compares: more
    // workers than cores keep both sides busy.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads * 2).build()?;
    let found: Vec<(Integrity, Result<Found>)> = pool.install(|| {
        all.par_iter().map(|i| (i.clone(), verify_entry(store, &key, i, &|i, d| pacote.extract(i, d), fix))).collect()
    });
    drop(pacote);
    crate::util::remove_run_dir();
    let mut r = Report { entries: all.len(), ..Report::default() };
    for (i, f) in found {
        match f? {
            Found::Match { recorded } => {
                r.matched += 1;
                r.recorded += recorded as usize;
            }
            Found::Differs { kind, why, fixed } => r.differs.push((i, kind, why, fixed)),
            Found::NotDir { fixed } => r.not_dir.push((i, fixed)),
            Found::NoTarball => r.no_tarball += 1,
            Found::Failed(e) => r.failed.push((i, e)),
        }
    }
    r.secs = t0.elapsed().as_secs_f64();
    Ok((r, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{cached_tarball, scratch};
    use crate::store::Ensure;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const KEY: &str = "pacote@1.0.0 tar@2.0.0";
    const OLD: &str = "pacote@0.9.0 tar@1.0.0";

    /// The classification, the record on a match, and --fix's replacement,
    /// with matchapack's own extractor standing in for pacote.
    #[test]
    fn entries_are_classified_recorded_and_replaced_as_by_the_inject_check() {
        let d = scratch("verify-pacote");
        let cacache = d.join("cacache");
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        let i = cached_tarball(&cacache, &[("index.js", 0o644, "1\n"), ("bin/cli", 0o755, "#!/bin/sh\n")]);
        let ex = |i: &Integrity, to: &Path| crate::store::extract(&fs::read(i.split_path(&cacache.join("content-v2")))?, to);
        let v = |fix: bool| verify_entry(&store, KEY, &i, &ex, fix).unwrap();
        assert!(matches!(store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted));
        // No record (as the classic engine's entries): a match records this version.
        assert_eq!(v(false), Found::Match { recorded: true });
        assert_eq!(store.made_by(&i), [KEY]);
        assert_eq!(v(false), Found::Match { recorded: false }, "already known");
        let entry = store.dir_for(&i);
        let change = || fs::set_permissions(entry.join("index.js"), fs::Permissions::from_mode(0o600)).unwrap();
        // Who made a differing entry.
        change();
        assert!(matches!(v(false), Found::Differs { kind: "changed", fixed: false, .. }));
        store.set_made_by(&i, OLD).unwrap();
        assert!(matches!(v(false), Found::Differs { kind: "pacote", .. }));
        store.forget_made_by(&i);
        assert!(matches!(v(false), Found::Differs { kind: "extractor", .. }));
        assert_eq!(fs::metadata(entry.join("index.js")).unwrap().permissions().mode() & 0o777, 0o600, "nothing changed without --fix");
        // --fix: replaced by the extraction, recorded as this version's.
        assert!(matches!(v(true), Found::Differs { kind: "extractor", fixed: true, .. }));
        assert_eq!(fs::metadata(entry.join("index.js")).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(store.made_by(&i), [KEY]);
        assert_eq!(v(false), Found::Match { recorded: false });
        // A key the helper could not name vouches for nothing.
        store.forget_made_by(&i);
        assert_eq!(verify_entry(&store, "pacote@? tar@?", &i, &ex, false).unwrap(), Found::Match { recorded: false });
        assert!(store.made_by(&i).is_empty());
        // An extraction that fails is reported as such, not as a difference.
        let failing = |_: &Integrity, _: &Path| -> Result<()> { bail!("no") };
        assert!(matches!(verify_entry(&store, KEY, &i, &failing, true).unwrap(), Found::Failed(_)));
        assert!(entry.is_dir(), "and the entry stays");
        assert!(fs::read_dir(d.join("store/tmp")).unwrap().next().is_none(), "nothing left in tmp/");
        let _ = fs::remove_dir_all(&d);
    }
}
