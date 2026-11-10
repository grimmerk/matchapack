//! The clone server of the inject engine (see inject.rs and NOTES "Inject
//! engine"): npm runs its own `npm ci` in a node child, and for every package
//! it would extract, the hook in src/inject.js asks this process to place the
//! package's store entry instead. Node cannot clone (see NOTES), so the clone
//! and the mtime touch happen here.
//!
//! Protocol: one JSON object per line, both ways, over one socket.
//!   node → thinclone
//!     {"hello":true,"pacote":"21.5.1","hook":"on"}   once, before npm starts
//!     {"id":7,"integrity":"sha512-…","dest":"/abs/dir","resolved":"https://…","cache":"/…/_cacache"}
//!     {"note":"type-FileFetcher"}                    a package the hook left to npm, and why
//!   thinclone → node
//!     {"id":7,"ok":true,"kind":"fast","filled":false}
//!     {"id":7,"ok":false,"reason":"store-miss","message":"…","fatal":false}
//! Requests are served concurrently (npm starts every extraction at once);
//! answers come back in any order. `fatal` means no later request can succeed
//! (the filesystem cannot clone): the hook stops asking.
//!
//! Nothing in a request is trusted as a path to read from or write to:
//!   - the source is named by its integrity, and the entry path is derived
//!     from the parsed digest, so it is always inside the store's layout; it
//!     must be a directory, not a symlink;
//!   - the destination must be an absolute path whose parent really is (after
//!     resolving symlinks) a directory inside the project root and below a
//!     `node_modules`, and must itself be absent or a real directory that
//!     holds nothing but `node_modules`;
//!   - `cache` is only read from, and a tarball found there is used only when
//!     its digest equals the requested integrity.
//! These checks catch a wrong request (a bug in the hook, a path npm derived
//! from an odd lockfile). They are not a boundary against another process of
//! the same user changing the project's directories while a package is
//! placed (a component swapped for a symlink between the check and the
//! operation): such a process can already write anywhere this one can. The
//! store's own layout (`<store>/v2/<algo>/…`) is trusted the same way: only
//! the entry itself is checked not to be a symlink.

use crate::clone::{self, Mode};
use crate::store::{Ensure, Integrity, Store, TmpDir};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// How a package was placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The destination was absent or empty: removed, then cloned as a whole.
    Fast,
    /// The destination held `node_modules`: cloned into a temporary
    /// directory, whose entries were then moved in.
    Merge,
}

/// The answer to one request.
#[derive(Debug)]
pub enum Outcome {
    Placed { kind: Kind, filled: bool, entry: Integrity, place_secs: f64, touch_secs: f64, paths: usize },
    Refused { reason: &'static str, message: String, fatal: bool },
}

fn refused(reason: &'static str, message: impl Into<String>) -> Outcome {
    Outcome::Refused { reason, message: message.into(), fatal: false }
}

/// What a run did, for the log and the summary line.
#[derive(Debug, Default)]
pub struct Stats {
    /// The hook's first message: npm was about to start.
    pub hello: Option<Value>,
    pub cloned: usize,
    /// kind name → (packages, summed place seconds, summed touch seconds).
    pub kinds: BTreeMap<String, (usize, f64, f64)>,
    /// Packages npm extracted itself, by reason (the hook's and the server's).
    pub fallbacks: BTreeMap<String, usize>,
    /// Entries cloned into the project (for the gc record and the sizes).
    pub used: Vec<Integrity>,
    /// Entries extracted into the store during this run.
    pub filled: Vec<Integrity>,
    pub paths_touched: usize,
    /// The first few refusal messages, for the log.
    pub errors: Vec<String>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    /// Seconds from the first request to the last answer.
    pub fn wall_secs(&self) -> f64 {
        match (self.first, self.last) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
            _ => 0.0,
        }
    }

    pub fn fallback_total(&self) -> usize {
        self.fallbacks.values().sum()
    }

    pub fn to_json(&self) -> Value {
        let r = |x: f64| (x * 100.0).round() / 100.0;
        let kinds: serde_json::Map<String, Value> = self
            .kinds
            .iter()
            .map(|(k, (n, p, t))| (k.clone(), json!({ "n": n, "place_secs": r(*p), "touch_secs": r(*t) })))
            .collect();
        json!({
            "cloned": self.cloned,
            "kinds": kinds,
            "fallbacks": self.fallbacks,
            "filled": self.filled.len(),
            "paths_touched": self.paths_touched,
            "serve_wall_secs": r(self.wall_secs()),
            "errors": self.errors,
            "pacote": self.hello.as_ref().and_then(|h| h.get("pacote")).cloned().unwrap_or(Value::Null),
        })
    }
}

pub struct Server<'a> {
    store: &'a Store,
    /// The project root, canonical.
    root: PathBuf,
    mode: Mode,
    /// Where merge placements clone to first: under the store's tmp/, so on
    /// the store's volume, which is the project's volume whenever a clone
    /// can work at all.
    tmp: TmpDir,
    seq: AtomicU64,
    /// Set once a clone failed because the filesystem cannot clone.
    unsupported: AtomicBool,
    /// Set when the node child has exited: queued requests are dropped.
    stopping: AtomicBool,
    pub stats: Mutex<Stats>,
}

enum DestState {
    Missing,
    Empty,
    OnlyNodeModules,
    Other(String),
}

fn dest_state(dst: &Path) -> std::io::Result<DestState> {
    let rd = match std::fs::read_dir(dst) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DestState::Missing),
        Err(e) => return Err(e),
    };
    let mut state = DestState::Empty;
    for ent in rd {
        let name = ent?.file_name();
        if name == "node_modules" {
            state = DestState::OnlyNodeModules;
        } else {
            return Ok(DestState::Other(name.to_string_lossy().into_owned()));
        }
    }
    Ok(state)
}

fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}

/// Move every entry of `from` into `to`; a directory present on both sides (a
/// package that ships a `node_modules` whose destination already holds
/// nested packages) is merged recursively. `from` ends up empty and is removed.
fn merge_into(from: &Path, to: &Path) -> Result<()> {
    for ent in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let ent = ent?;
        let (a, b) = (ent.path(), to.join(ent.file_name()));
        if let Err(e) = std::fs::rename(&a, &b) {
            if is_real_dir(&a) && is_real_dir(&b) {
                merge_into(&a, &b)?;
            } else {
                return Err(e).with_context(|| format!("moving {} to {}", a.display(), b.display()));
            }
        }
    }
    std::fs::remove_dir(from).with_context(|| format!("removing {}", from.display()))
}

/// The error means "the path already exists" (a destination that reappeared
/// between the check and the clone).
fn already_exists(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<clone::CloneError>().is_some_and(|c| c.errno == Some(libc::EEXIST))
            || c.downcast_ref::<std::io::Error>().is_some_and(|i| i.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

impl<'a> Server<'a> {
    /// `root` must be the canonical project directory.
    pub fn new(store: &'a Store, root: PathBuf, mode: Mode) -> Result<Server<'a>> {
        let tmp = TmpDir::create(store.root.join("tmp").join(format!("{}-inject", crate::store::process_tag())), 0o700)?;
        Ok(Server {
            store,
            root,
            mode,
            tmp,
            seq: AtomicU64::new(0),
            unsupported: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            stats: Mutex::new(Stats::default()),
        })
    }

    /// The directory the request may be placed at: `dest` with its parent
    /// resolved. See the module comment for the rules.
    fn check_dest(&self, dest: &Path) -> std::result::Result<PathBuf, String> {
        if !dest.is_absolute() || !dest.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_))) {
            return Err("the destination is not a plain absolute path".into());
        }
        let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
            return Err("the destination has no parent directory".into());
        };
        if name == "node_modules" {
            return Err("the destination is a node_modules directory".into());
        }
        let real = std::fs::canonicalize(parent).map_err(|e| format!("resolving {}: {e}", parent.display()))?;
        let Ok(rel) = real.strip_prefix(&self.root) else {
            return Err(format!("{} resolves outside the project ({})", parent.display(), real.display()));
        };
        if !rel.components().any(|c| c.as_os_str() == "node_modules") {
            return Err(format!("{} is not inside a node_modules of the project", real.display()));
        }
        let out = real.join(name);
        match std::fs::symlink_metadata(&out) {
            Ok(m) if !m.file_type().is_dir() => Err(format!("{} exists and is not a directory", out.display())),
            _ => Ok(out),
        }
    }

    /// The store entry for `sri`, extracted from npm's cache first when it is
    /// missing and `fill` names a cache and the tarball's URL.
    fn source(&self, sri: &str, fill: Option<(&Path, &str)>) -> std::result::Result<(Integrity, PathBuf, bool), Outcome> {
        let i = Integrity::parse(sri).map_err(|e| refused("rejected-source", format!("{e:#}")))?;
        let dir = self.store.dir_for(&i);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => Ok((i, dir, false)),
            Ok(_) => Err(refused("rejected-source", format!("store entry {} is not a directory", dir.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some((cache, url)) = fill else { return Err(refused("store-miss", "not in the store")) };
                if !cache.is_absolute() {
                    return Err(refused("store-miss", "not in the store (no usable npm cache path)"));
                }
                match self.store.ensure_from(cache, &i, url) {
                    Ok(Ensure::Missing) => Err(refused("store-miss", "not in the store, tarball not in npm's cache")),
                    Ok(Ensure::Present) => Ok((i, dir, false)),
                    Ok(Ensure::Extracted) => Ok((i, dir, true)),
                    Err(e) => Err(refused("store-error", format!("{e:#}"))),
                }
            }
            Err(e) => Err(refused("store-error", format!("{}: {e}", dir.display()))),
        }
    }

    /// Clone `src` to `dst` (validated). Errors leave whatever was placed for
    /// pacote's own extract, which empties the directory first.
    fn place(&self, src: &Path, dst: &Path) -> std::result::Result<Kind, Outcome> {
        let clone_err = |e: anyhow::Error| {
            if clone::unsupported(&e) {
                self.unsupported.store(true, Ordering::Relaxed);
                Outcome::Refused { reason: "clone-unsupported", message: format!("{e:#}"), fatal: true }
            } else {
                refused("clone-error", format!("{e:#}"))
            }
        };
        let io_err = |what: &str, e: std::io::Error| refused("clone-error", format!("{what} {}: {e}", dst.display()));
        let not_empty = |name: String| refused("dest-not-empty", format!("{} already holds {name}", dst.display()));

        let direct = match dest_state(dst).map_err(|e| io_err("reading", e))? {
            DestState::Missing => true,
            // npm made the directory before extracting (its sparse tree).
            DestState::Empty => match std::fs::remove_dir(dst) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                // Filled since the check: decided again below.
                Err(_) => false,
            },
            // Refused below, where the directory is looked at again.
            DestState::OnlyNodeModules | DestState::Other(_) => false,
        };
        if direct {
            match clone::clone_tree(src, dst, self.mode) {
                Ok(()) => return Ok(Kind::Fast),
                // Something made the directory again in between: merge.
                Err(e) if already_exists(&e) => {}
                Err(e) => return Err(clone_err(e)),
            }
        }
        // The destination holds node_modules (nested packages, placed by
        // their own requests at the same time): clone aside, move entries in.
        let tmp = self.tmp.path.join(format!("m{}", self.seq.fetch_add(1, Ordering::Relaxed)));
        let cleanup = || {
            let _ = std::fs::remove_dir_all(&tmp);
        };
        if let Err(e) = clone::clone_tree(src, &tmp, self.mode) {
            cleanup();
            return Err(clone_err(e));
        }
        match std::fs::create_dir(dst) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                cleanup();
                return Err(io_err("creating", e));
            }
        }
        // Checked again right before entries are moved in: dest_state and
        // the renames below follow a symlink here.
        if !is_real_dir(dst) {
            cleanup();
            return Err(refused("rejected-dest", format!("{} is no longer a directory", dst.display())));
        }
        match dest_state(dst) {
            Ok(DestState::Other(name)) => {
                cleanup();
                return Err(not_empty(name));
            }
            Ok(_) => {}
            Err(e) => {
                cleanup();
                return Err(io_err("reading", e));
            }
        }
        if let Err(e) = merge_into(&tmp, dst) {
            cleanup();
            return Err(refused("clone-error", format!("{e:#}")));
        }
        Ok(Kind::Merge)
    }

    /// Serve one request: validate, find (or fill) the entry, place it, set
    /// the times of every placed path to now.
    pub fn place_entry(&self, sri: &str, dest: &Path, fill: Option<(&Path, &str)>) -> Outcome {
        if self.unsupported.load(Ordering::Relaxed) {
            return Outcome::Refused { reason: "clone-unsupported", message: "this filesystem cannot clone".into(), fatal: true };
        }
        let dst = match self.check_dest(dest) {
            Ok(d) => d,
            Err(m) => return refused("rejected-dest", m),
        };
        let (entry, src, filled) = match self.source(sri, fill) {
            Ok(s) => s,
            Err(o) => return o,
        };
        if filled {
            // Counted here: the entry is in the store now, whatever becomes
            // of the placement.
            self.stats.lock().unwrap().filled.push(entry.clone());
        }
        let t0 = Instant::now();
        let kind = match self.place(&src, &dst) {
            Ok(k) => k,
            Err(o) => return o,
        };
        let t1 = Instant::now();
        // npm ci leaves every extracted path with the install time (pacote
        // extracts with noMtime); a clone carries the store's. Done here, per
        // package and before the answer, so every touch is over before npm
        // runs an install script, whose files must keep their later times.
        let paths = match clone::touch_placed(&src, &dst) {
            Ok(n) => n,
            Err(e) => return refused("clone-error", format!("{e:#}")),
        };
        Outcome::Placed {
            kind,
            filled,
            entry,
            place_secs: t1.duration_since(t0).as_secs_f64(),
            touch_secs: t1.elapsed().as_secs_f64(),
            paths,
        }
    }

    fn kind_name(&self, k: Kind) -> &'static str {
        match (self.mode, k) {
            (Mode::Dir, Kind::Fast) => "fast",
            (Mode::Dir, Kind::Merge) => "merge",
            (Mode::File, Kind::Fast) => "file",
            (Mode::File, Kind::Merge) => "file-merge",
        }
    }

    fn tally(&self, o: &Outcome) {
        let mut s = self.stats.lock().unwrap();
        s.last = Some(Instant::now());
        match o {
            Outcome::Placed { kind, entry, place_secs, touch_secs, paths, .. } => {
                s.cloned += 1;
                let k = s.kinds.entry(self.kind_name(*kind).to_string()).or_default();
                k.0 += 1;
                k.1 += place_secs;
                k.2 += touch_secs;
                s.paths_touched += paths;
                s.used.push(entry.clone());
            }
            Outcome::Refused { reason, message, .. } => {
                *s.fallbacks.entry(reason.to_string()).or_default() += 1;
                let line = format!("{reason}: {message}");
                if *reason != "store-miss" && s.errors.len() < 5 && !s.errors.contains(&line) {
                    s.errors.push(line);
                }
            }
        }
    }

    /// Record a message that needs no answer (`hello`, `note`). Returns
    /// false when `v` is something else.
    fn record_message(&self, v: &Value) -> bool {
        if v.get("hello").is_some() {
            self.stats.lock().unwrap().hello = Some(v.clone());
            return true;
        }
        if let Some(note) = v.get("note").and_then(|n| n.as_str()) {
            // A reason is a short word chosen by the hook; keep the log sane
            // whatever arrives.
            let key: String = note.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
            *self.stats.lock().unwrap().fallbacks.entry(key).or_default() += 1;
            return true;
        }
        false
    }

    /// One line from the hook; the answer line, if the line was a request.
    fn handle_line(&self, line: &str) -> Option<String> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return None;
        };
        if self.record_message(&v) {
            return None;
        }
        let id = v.get("id")?.clone();
        {
            let mut s = self.stats.lock().unwrap();
            s.first.get_or_insert_with(Instant::now);
        }
        let str_of = |k: &str| v.get(k).and_then(|x| x.as_str());
        let outcome = match (str_of("integrity"), str_of("dest")) {
            (Some(sri), Some(dest)) => {
                let fill = match (str_of("cache"), str_of("resolved")) {
                    (Some(c), Some(u)) => Some((Path::new(c), u)),
                    _ => None,
                };
                // A panic in one placement must not take the answer with it:
                // the hook would wait for ever.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.place_entry(sri, Path::new(dest), fill)))
                    .unwrap_or_else(|_| refused("clone-error", "internal error (panic)"))
            }
            _ => refused("bad-request", "integrity and dest are required"),
        };
        self.tally(&outcome);
        Some(
            match outcome {
                Outcome::Placed { kind, filled, .. } => json!({ "id": id, "ok": true, "kind": self.kind_name(kind), "filled": filled }),
                Outcome::Refused { reason, message, fatal } => {
                    json!({ "id": id, "ok": false, "reason": reason, "message": message, "fatal": fatal })
                }
            }
            .to_string(),
        )
    }

    /// Tell the workers to drop what is still queued (the node child is gone).
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// Read requests from `stream` until it ends, serving them on `threads`
    /// worker threads. Returns when the stream is at its end (the peer closed
    /// it, or someone shut it down) and every started request is answered.
    pub fn serve(&self, stream: UnixStream, threads: usize) -> Result<()> {
        let reader = BufReader::new(stream.try_clone().context("cloning the socket")?);
        let writer = Mutex::new(stream);
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let rx = Mutex::new(rx);
        std::thread::scope(|s| {
            for _ in 0..threads.max(1) {
                s.spawn(|| loop {
                    let line = match rx.lock().unwrap().recv() {
                        Ok(l) => l,
                        Err(_) => return,
                    };
                    if self.stopping.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Some(mut answer) = self.handle_line(&line) {
                        answer.push('\n');
                        // The peer may be gone; nothing to do about it here.
                        let _ = writer.lock().unwrap().write_all(answer.as_bytes());
                    }
                });
            }
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        // Messages that need no answer are recorded here, at
                        // once: the workers drop what is queued after stop(),
                        // and a lost `hello` would read as "npm never started".
                        if serde_json::from_str::<Value>(&l).is_ok_and(|v| self.record_message(&v)) {
                            continue;
                        }
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            drop(tx);
        });
        Ok(())
    }
}

impl Drop for Server<'_> {
    fn drop(&mut self) {
        // Merge placements remove their own directories; what a failed one
        // left goes here, the directory itself too.
        let _ = std::fs::remove_dir_all(&self.tmp.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{cached_tarball, scratch};
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    struct Fx {
        dir: PathBuf,
        cacache: PathBuf,
        root: PathBuf,
        store: Store,
    }

    impl Fx {
        fn new(name: &str) -> Fx {
            let dir = fs::canonicalize(scratch(name)).unwrap();
            let cacache = dir.join("cacache");
            let root = dir.join("project");
            fs::create_dir_all(root.join("node_modules")).unwrap();
            let store = Store::new(dir.join("store"), cacache.clone()).unwrap();
            Fx { dir, cacache, root, store }
        }

        fn server(&self, mode: Mode) -> Server<'_> {
            Server::new(&self.store, self.root.clone(), mode).unwrap()
        }

        /// A package in the fake npm cache and in the store; its SRI string.
        fn entry(&self, files: &[(&str, u32, &str)]) -> (Integrity, String) {
            let i = cached_tarball(&self.cacache, files);
            assert!(matches!(self.store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted | Ensure::Present));
            let sri = sri_of(&i);
            (i, sri)
        }

        /// Whether this filesystem can clone at all (APFS, btrfs, XFS with
        /// reflink): on others (ext4 in CI's Linux job) the placement tests
        /// have nothing to check and are skipped.
        fn can_clone(&self) -> bool {
            let (i, _) = self.entry(&[("probe.txt", 0o644, "x")]);
            let dst = self.dir.join("probe-clone");
            match clone::clone_tree(&self.store.dir_for(&i), &dst, Mode::File) {
                Ok(()) => true,
                Err(e) if clone::unsupported(&e) => {
                    eprintln!("skipped: this filesystem cannot clone ({e:#})");
                    false
                }
                Err(e) => panic!("{e:#}"),
            }
        }
    }

    fn sri_of(i: &Integrity) -> String {
        use base64::Engine;
        format!("{}-{}", i.algo, base64::engine::general_purpose::STANDARD.encode(hex::decode(&i.hex).unwrap()))
    }

    /// Per-file clones everywhere; directory clones too where they exist.
    fn modes() -> Vec<clone::Mode> {
        let mut v = vec![clone::Mode::File];
        if crate::default_clone_mode() != clone::Mode::File {
            v.push(crate::default_clone_mode());
        }
        v
    }

    const PKG: &[(&str, u32, &str)] = &[("package.json", 0o644, "{\"name\":\"p\"}\n"), ("lib/index.js", 0o644, "module.exports = 1\n"), ("bin/cli", 0o755, "#!/bin/sh\n")];

    fn placed(o: Outcome) -> (Kind, bool) {
        match o {
            Outcome::Placed { kind, filled, .. } => (kind, filled),
            o => panic!("expected a placement, got {o:?}"),
        }
    }

    fn refusal(o: Outcome) -> (&'static str, String, bool) {
        match o {
            Outcome::Refused { reason, message, fatal } => (reason, message, fatal),
            o => panic!("expected a refusal, got {o:?}"),
        }
    }

    /// Every path under `dir`, relative, sorted, with its kind and mode.
    fn listing(dir: &Path) -> Vec<String> {
        let mut v = vec![];
        for e in walkdir::WalkDir::new(dir).follow_links(false).sort_by_file_name() {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            let kind = if m.is_dir() { "d" } else if m.file_type().is_symlink() { "l" } else { "f" };
            v.push(format!("{kind} {:o} {}", m.mode() & 0o7777, e.path().strip_prefix(dir).unwrap().display()));
        }
        v
    }

    /// Set the times of every path under `dir` (and `dir`) to long ago.
    fn age_tree(dir: &Path) {
        for e in walkdir::WalkDir::new(dir).follow_links(false) {
            crate::store::tests::age(e.unwrap().path(), 86400 * 30);
        }
    }

    fn mtime(p: &Path) -> std::time::SystemTime {
        fs::symlink_metadata(p).unwrap().modified().unwrap()
    }

    #[test]
    fn empty_dir_is_replaced_by_one_clone_and_missing_dest_is_created() {
        let fx = Fx::new("serve-fast");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        let want = listing(&fx.store.dir_for(&i));
        let s = fx.server(clone::Mode::File);
        // npm's sparse tree: the directory exists and is empty.
        let dst = fx.root.join("node_modules/p");
        fs::create_dir(&dst).unwrap();
        assert_eq!(placed(s.place_entry(&sri, &dst, None)), (Kind::Fast, false));
        assert_eq!(listing(&dst), want);
        assert_eq!(fs::read_to_string(dst.join("lib/index.js")).unwrap(), "module.exports = 1\n");
        // A destination that does not exist (its parent does).
        fs::create_dir(fx.root.join("node_modules/@scope")).unwrap();
        let dst2 = fx.root.join("node_modules/@scope/q");
        assert_eq!(placed(s.place_entry(&sri, &dst2, None)), (Kind::Fast, false));
        assert_eq!(listing(&dst2), want);
        // The clone is independent: writing into it leaves the store alone.
        fs::write(dst.join("lib/index.js"), "changed\n").unwrap();
        assert_eq!(fs::read_to_string(fx.store.dir_for(&i).join("lib/index.js")).unwrap(), "module.exports = 1\n");
        // A directory that already holds the package's own files is npm's to
        // handle (pacote empties it first).
        let (reason, _, fatal) = refusal(s.place_entry(&sri, &dst, None));
        assert_eq!((reason, fatal), ("dest-not-empty", false));
        assert_eq!(s.stats.lock().unwrap().cloned, 0, "place_entry does not tally; handle_line does");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_clone_gives_the_same_tree_as_file_clones() {
        let fx = Fx::new("serve-dir");
        let (i, sri) = fx.entry(PKG);
        let s = fx.server(clone::Mode::Dir);
        let dst = fx.root.join("node_modules/p");
        fs::create_dir(&dst).unwrap();
        assert_eq!(placed(s.place_entry(&sri, &dst, None)), (Kind::Fast, false));
        assert_eq!(listing(&dst), listing(&fx.store.dir_for(&i)));
        // Merge with directory clones.
        let dst2 = fx.root.join("node_modules/q");
        fs::create_dir_all(dst2.join("node_modules/child")).unwrap();
        assert_eq!(placed(s.place_entry(&sri, &dst2, None)), (Kind::Merge, false));
        fs::remove_dir_all(dst2.join("node_modules")).unwrap();
        assert_eq!(listing(&dst2), listing(&fx.store.dir_for(&i)));
    }

    #[test]
    fn merge_keeps_nested_packages_and_merges_a_shipped_node_modules() {
        let fx = Fx::new("serve-merge");
        if !fx.can_clone() {
            return;
        }
        // A bundler: ships node_modules/inner; npm nests a non-bundled child
        // next to it.
        let (i, sri) = fx.entry(&[
            ("package.json", 0o644, "{\"name\":\"bundler\"}\n"),
            ("index.js", 0o644, "1\n"),
            ("node_modules/inner/package.json", 0o644, "{\"name\":\"inner\"}\n"),
            ("node_modules/inner/lib/a.js", 0o644, "a\n"),
        ]);
        for mode in modes() {
            let s = fx.server(mode);
            let dst = fx.root.join(format!("node_modules/bundler-{mode:?}"));
            fs::create_dir_all(dst.join("node_modules/child")).unwrap();
            fs::write(dst.join("node_modules/child/package.json"), "{\"name\":\"child\"}\n").unwrap();
            // A sparse directory for a package the bundle also ships is replaced.
            fs::create_dir_all(dst.join("node_modules/inner")).unwrap();
            assert_eq!(placed(s.place_entry(&sri, &dst, None)), (Kind::Merge, false));
            assert_eq!(fs::read_to_string(dst.join("node_modules/child/package.json")).unwrap(), "{\"name\":\"child\"}\n", "the nested package stays");
            assert_eq!(fs::read_to_string(dst.join("node_modules/inner/lib/a.js")).unwrap(), "a\n");
            fs::remove_dir_all(dst.join("node_modules/child")).unwrap();
            assert_eq!(listing(&dst), listing(&fx.store.dir_for(&i)), "{mode:?}: the entry's tree, nothing else");
            assert_eq!(fs::read_dir(&s.tmp.path).unwrap().count(), 0, "the temporary clone is gone");
        }
    }

    #[test]
    fn sources_outside_the_store_layout_are_refused() {
        let fx = Fx::new("serve-source");
        let s = fx.server(clone::Mode::File);
        let dst = fx.root.join("node_modules/p");
        // Not an integrity, an unknown algorithm, a digest of the wrong
        // length, a digest that is a path.
        for bad in ["", "../../outside", "md5-AAAA", "sha512-AAAA", "sha512-../../../../outside", "sha1-../.."] {
            let (reason, _, _) = refusal(s.place_entry(bad, &dst, None));
            assert_eq!(reason, "rejected-source", "{bad:?}");
        }
        // An entry path that is a symlink to a directory outside the store:
        // a directory clone of it would place the link, a file clone would
        // read files the store never verified.
        let outside = fx.dir.join("outside-pkg");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("package.json"), "{}").unwrap();
        let i = Integrity { algo: "sha512".into(), hex: "ab".repeat(64) };
        let entry = fx.store.dir_for(&i);
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &entry).unwrap();
        let (reason, msg, _) = refusal(s.place_entry(&sri_of(&i), &dst, None));
        assert_eq!(reason, "rejected-source", "{msg}");
        // A file where an entry belongs.
        let j = Integrity { algo: "sha512".into(), hex: "cd".repeat(64) };
        fs::create_dir_all(fx.store.dir_for(&j).parent().unwrap()).unwrap();
        fs::write(fx.store.dir_for(&j), "x").unwrap();
        assert_eq!(refusal(s.place_entry(&sri_of(&j), &dst, None)).0, "rejected-source");
        // An entry that is in neither the store nor the cache is a miss.
        let k = Integrity { algo: "sha512".into(), hex: "ef".repeat(64) };
        assert_eq!(refusal(s.place_entry(&sri_of(&k), &dst, Some((&fx.cacache, "http://x/k.tgz")))).0, "store-miss");
        assert!(!dst.exists(), "nothing was placed");
    }

    #[test]
    fn destinations_outside_the_project_are_refused() {
        let fx = Fx::new("serve-dest");
        let (_, sri) = fx.entry(PKG);
        let s = fx.server(clone::Mode::File);
        let outside = fx.dir.join("victim");
        fs::create_dir_all(outside.join("node_modules")).unwrap();
        fs::create_dir_all(fx.root.join("src")).unwrap();
        fs::create_dir_all(fx.root.join("node_modules/a")).unwrap();
        // A symlinked parent, to a directory outside the project and to one
        // inside it that is not a node_modules.
        std::os::unix::fs::symlink(outside.join("node_modules"), fx.root.join("node_modules/@out")).unwrap();
        std::os::unix::fs::symlink(fx.root.join("src"), fx.root.join("node_modules/@src")).unwrap();
        // A destination that is itself a symlink to a directory.
        std::os::unix::fs::symlink(&outside, fx.root.join("node_modules/link")).unwrap();
        let cases: Vec<(PathBuf, &str)> = vec![
            (outside.join("node_modules/p"), "outside the project"),
            (PathBuf::from("node_modules/p"), "relative"),
            (fx.root.join("node_modules/a/../../../victim/node_modules/p"), "dot-dot"),
            // Not a plain path, although it resolves inside the project: npm
            // never sends one.
            (fx.root.join("node_modules/a/../p"), "dot-dot that stays inside"),
            (fx.root.join("node_modules/@out/p"), "symlinked parent, outside"),
            (fx.root.join("node_modules/@src/p"), "symlinked parent, inside but not node_modules"),
            (fx.root.join("node_modules/link"), "the destination is a symlink"),
            (fx.root.join("src/p"), "not under node_modules"),
            (fx.root.join("node_modules"), "the project's node_modules itself"),
            (fx.root.join("node_modules/a/node_modules"), "a node_modules directory"),
            (fx.root.join("node_modules/missing/p"), "the parent does not exist"),
            (fx.root.clone(), "the project root"),
        ];
        for (dest, what) in &cases {
            let (reason, msg, fatal) = refusal(s.place_entry(&sri, dest, None));
            assert_eq!((reason, fatal), ("rejected-dest", false), "{what}: {msg}");
        }
        assert_eq!(fs::read_dir(outside.join("node_modules")).unwrap().count(), 0, "nothing written outside the project");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(fs::read_dir(fx.root.join("src")).unwrap().count(), 0);
        assert!(fs::symlink_metadata(fx.root.join("node_modules/link")).unwrap().file_type().is_symlink());
    }

    #[test]
    fn every_placed_path_gets_the_current_time_and_nothing_else_does() {
        let fx = Fx::new("serve-touch");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(&[("package.json", 0o644, "{}\n"), ("lib/deep/x.js", 0o644, "x\n"), ("node_modules/inner/i.js", 0o644, "i\n")]);
        age_tree(&fx.store.dir_for(&i));
        let start = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        for mode in modes() {
            let s = fx.server(mode);
            // Fast: every path, the directory itself included.
            let fast = fx.root.join(format!("node_modules/fast-{mode:?}"));
            assert_eq!(placed(s.place_entry(&sri, &fast, None)).0, Kind::Fast);
            for e in walkdir::WalkDir::new(&fast) {
                let e = e.unwrap();
                assert!(mtime(e.path()) >= start, "{mode:?} fast: {} kept the store's time", e.path().display());
            }
            // Merge: the entry's paths, but not another package's files that
            // sit in the destination's node_modules.
            let merge = fx.root.join(format!("node_modules/merge-{mode:?}"));
            let other = merge.join("node_modules/other/o.js");
            fs::create_dir_all(other.parent().unwrap()).unwrap();
            fs::write(&other, "o\n").unwrap();
            crate::store::tests::age(&other, 86400);
            assert_eq!(placed(s.place_entry(&sri, &merge, None)).0, Kind::Merge);
            for p in ["", "package.json", "lib", "lib/deep", "lib/deep/x.js", "node_modules", "node_modules/inner", "node_modules/inner/i.js"] {
                assert!(mtime(&merge.join(p)) >= start, "{mode:?} merge: {p:?} kept the store's time");
            }
            assert!(mtime(&other) < start, "{mode:?}: another package's file was touched");
        }
        // The store keeps its own times.
        assert!(mtime(&fx.store.dir_for(&i).join("lib/deep/x.js")) < start);
    }

    #[test]
    fn a_missing_entry_is_extracted_from_the_cache_the_request_names() {
        let fx = Fx::new("serve-fill");
        if !fx.can_clone() {
            return;
        }
        // In a cache directory the Store was not opened with.
        let other_cache = fx.dir.join("other-cacache");
        let i = cached_tarball(&other_cache, PKG);
        let s = fx.server(clone::Mode::File);
        let dst = fx.root.join("node_modules/p");
        assert_eq!(refusal(s.place_entry(&sri_of(&i), &dst, None)).0, "store-miss", "no cache named");
        assert_eq!(refusal(s.place_entry(&sri_of(&i), &dst, Some((&fx.cacache, "http://x/p.tgz")))).0, "store-miss", "not in that cache");
        assert_eq!(placed(s.place_entry(&sri_of(&i), &dst, Some((&other_cache, "http://x/p.tgz")))), (Kind::Fast, true));
        assert_eq!(s.stats.lock().unwrap().filled, vec![i.clone()]);
        assert!(fx.store.dir_for(&i).is_dir());
        assert_eq!(listing(&dst), listing(&fx.store.dir_for(&i)));
        // A cache file whose bytes are not what the integrity names is never
        // extracted.
        let j = cached_tarball(&other_cache, &[("package.json", 0o644, "{\"name\":\"j\"}\n")]);
        let p = j.split_path(&other_cache.join("content-v2"));
        fs::write(&p, b"not the tarball").unwrap();
        let dst2 = fx.root.join("node_modules/j");
        let (reason, msg, _) = refusal(s.place_entry(&sri_of(&j), &dst2, Some((&other_cache, "http://x/j.tgz"))));
        assert_eq!(reason, "store-error", "{msg}");
        assert!(msg.contains("integrity mismatch"), "{msg}");
        assert!(!fx.store.dir_for(&j).exists() && !dst2.exists());
    }

    #[test]
    fn hello_and_notes_are_recorded_after_stop_while_requests_are_dropped() {
        // inject::run calls stop() once node has exited; lines still unread
        // at that point are dropped, except the ones that need no answer: a
        // lost hello would make a finished npm run read as "never started".
        let fx = Fx::new("serve-stop");
        let s = fx.server(Mode::File);
        s.stop();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut w = theirs.try_clone().unwrap();
        let req = json!({ "id": 1, "integrity": "sha512-x", "dest": fx.root.join("node_modules/p") });
        w.write_all(format!("{{\"hello\":true,\"pacote\":\"9.9.9\"}}\n{{\"note\":\"hook-off\"}}\n{req}\n").as_bytes()).unwrap();
        theirs.shutdown(std::net::Shutdown::Write).unwrap();
        s.serve(ours, 2).unwrap();
        let st = s.stats.lock().unwrap();
        assert_eq!(st.hello.as_ref().unwrap()["pacote"], "9.9.9");
        assert_eq!(st.fallbacks.get("hook-off"), Some(&1));
        assert_eq!(st.cloned + st.fallbacks.values().sum::<usize>(), 1, "the request was dropped, not served");
    }

    #[test]
    fn concurrent_requests_over_the_socket_are_all_answered() {
        let fx = Fx::new("serve-socket");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        let want = listing(&fx.store.dir_for(&i));
        let s = fx.server(crate::default_clone_mode());
        let (ours, theirs) = UnixStream::pair().unwrap();
        // A parent with nested children, all asked for at once, as npm does:
        // p0 holds node_modules/c0..c19; p1..p19 are plain.
        let n = 20;
        let mut lines = String::from("{\"hello\":true,\"pacote\":\"1.2.3\"}\n");
        let mut dests = vec![];
        for k in 0..n {
            let child = fx.root.join(format!("node_modules/p0/node_modules/c{k}"));
            fs::create_dir_all(&child).unwrap();
            dests.push(child);
            let top = fx.root.join(format!("node_modules/p{k}"));
            fs::create_dir_all(&top).unwrap();
            dests.push(top);
        }
        for (id, d) in dests.iter().enumerate() {
            lines += &json!({ "id": id, "integrity": sri, "dest": d, "cache": fx.cacache, "resolved": "http://x/p.tgz" }).to_string();
            lines.push('\n');
        }
        lines += "{\"note\":\"type-FileFetcher\"}\nnot json\n{\"id\":\"x\"}\n";
        lines += &json!({ "id": 999, "integrity": sri, "dest": fx.dir.join("victim/node_modules/p") }).to_string();
        lines.push('\n');
        std::thread::scope(|sc| {
            let server = sc.spawn(|| s.serve(ours, 8).unwrap());
            let mut w = theirs.try_clone().unwrap();
            w.write_all(lines.as_bytes()).unwrap();
            let mut answers: BTreeMap<String, Value> = BTreeMap::new();
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            while answers.len() < dests.len() + 2 {
                let mut l = String::new();
                assert!(r.read_line(&mut l).unwrap() > 0, "the server closed early");
                let v: Value = serde_json::from_str(&l).unwrap();
                answers.insert(v["id"].to_string(), v);
            }
            // The server stops at the end of the stream.
            theirs.shutdown(std::net::Shutdown::Both).unwrap();
            server.join().unwrap();
            for id in 0..dests.len() {
                assert_eq!(answers[&id.to_string()]["ok"], true, "{:?}", answers[&id.to_string()]);
            }
            assert_eq!(answers["999"]["reason"], "rejected-dest");
            assert_eq!(answers["\"x\""]["reason"], "bad-request");
        });
        for d in &dests {
            let mut got = listing(d);
            got.retain(|l| !l.contains("node_modules"));
            assert_eq!(got, want, "{}", d.display());
        }
        let st = s.stats.lock().unwrap();
        assert_eq!(st.cloned, dests.len());
        assert_eq!(st.used.len(), dests.len());
        assert_eq!(st.kinds.values().map(|k| k.0).sum::<usize>(), dests.len());
        assert_eq!(st.fallbacks.get("type-FileFetcher"), Some(&1));
        assert_eq!(st.fallbacks.get("rejected-dest"), Some(&1));
        assert_eq!(st.hello.as_ref().unwrap()["pacote"], "1.2.3");
        assert!(st.paths_touched >= dests.len() * want.len());
    }

    /// A filesystem pair that cannot clone turns the server off: the first
    /// failure is fatal, and later requests are refused without trying.
    #[test]
    fn a_clone_the_filesystem_refuses_is_fatal() {
        let fx = Fx::new("serve-fatal");
        let (_, sri) = fx.entry(PKG);
        let s = fx.server(clone::Mode::File);
        s.unsupported.store(true, Ordering::Relaxed);
        let dst = fx.root.join("node_modules/p");
        let (reason, _, fatal) = refusal(s.place_entry(&sri, &dst, None));
        assert_eq!((reason, fatal), ("clone-unsupported", true));
        assert!(!dst.exists());
        let e = anyhow::Error::new(clone::CloneError::for_test(libc::EXDEV));
        assert!(clone::unsupported(&e) && !already_exists(&e));
        let e = anyhow::Error::new(clone::CloneError::for_test(libc::EEXIST));
        assert!(!clone::unsupported(&e) && already_exists(&e));
    }
}
