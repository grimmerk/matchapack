//! The clone server of the inject engine (see inject.rs and NOTES "Inject
//! engine"): npm runs its own `npm ci` in a node child, and for every package
//! it would extract, the hook in src/inject.js asks this process to place the
//! package's store entry instead. Node cannot clone (see NOTES), so the clone
//! and the mtime touch happen here.
//!
//! Protocol: one JSON object per line, both ways, over one socket.
//!   node → thinclone
//!     {"hello":true,"pacote":"21.5.1","hook":"on"}   once, before npm starts
//!     {"id":7,"integrity":"sha512-…","dest":"/abs/dir"}
//!     {"id":8,"integrity":"sha512-…","dest":"/abs/dir","extracted":"/…/tmp/…/x3"}
//!     {"note":"type-FileFetcher"}                    a package the hook left to npm, and why
//!   thinclone → node
//!     {"id":7,"ok":true,"kind":"fast","filled":false,"checked":false}
//!     {"id":7,"ok":false,"extract_to":"/…/tmp/…/x3","why":"fill"}
//!     {"id":7,"ok":false,"reason":"rejected-dest","message":"…","fatal":false}
//! Requests are served concurrently (npm starts every extraction at once);
//! answers come back in any order. `fatal` means no later request can succeed
//! (the filesystem cannot clone): the hook stops asking.
//!
//! The store is filled by pacote, never by thinclone's own extractor: for a
//! package not in the store the answer is `extract_to`, a fresh directory in
//! this server's temporary directory; the hook runs pacote's own extract into
//! it (the very fetcher npm made for that package) and sends the request
//! again with `extracted`. The directory then becomes the store entry
//! (renamed in only if no other install got there first) and is cloned.
//! The same round trip checks the store against pacote (`why: "check"`): the
//! entry is compared with pacote's extraction (`store::compare_trees`) before
//! anything is placed. Each entry records which pacote and tar made it, or
//! last extracted it to the same tree (`store.made_by`; the key, e.g.
//! `pacote@21.5.1 tar@7.5.22`). An entry is checked when:
//!   - nothing made it that is known (thinclone's own extractor, the classic
//!     engine's, made it, or it predates the record): always;
//!   - this key is not in `<store>/inject-checked` yet (the first install
//!     under a new npm compares every entry it uses);
//!   - another key made it, and this key is in `<store>/inject-strict` (a
//!     difference was found between an earlier pacote's entries and this one);
//!   - otherwise one in THINCLONE_INJECT_CHECK (default 50), at random.
//! A difference places pacote's extraction and replaces the entry with it
//! (renamed aside first: an install cloning from it at that moment gets the
//! whole old entry or an error, never a mix). Its kind: `extractor` (made by
//! thinclone's extractor: expected for odd tarballs), `changed` (made by this
//! same key: the entry was changed since), `pacote` (made by another key: the
//! rules changed; this key becomes strict).
//!
//! Nothing in a request is trusted as a path to read from or write to:
//!   - the source is named by its integrity, and the entry path is derived
//!     from the parsed digest, so it is always inside the store's layout; it
//!     must be a directory, not a symlink;
//!   - the destination must be an absolute path whose parent really is (after
//!     resolving symlinks) a directory inside the project root and below a
//!     `node_modules`, and must itself be absent or a real directory that
//!     holds nothing but `node_modules`;
//!   - `extracted` must be a directory this server handed out for the same
//!     integrity (and is used once).
//! These checks catch a wrong request (a bug in the hook, a path npm derived
//! from an odd lockfile). They are not a boundary against another process of
//! the same user changing the project's directories while a package is
//! placed (a component swapped for a symlink between the check and the
//! operation): such a process can already write anywhere this one can. The
//! store's own layout (`<store>/v2/<algo>/…`) is trusted the same way: only
//! the entry itself is checked not to be a symlink.

use crate::clone::{self, Mode};
use crate::store::{Integrity, Store, TmpDir};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
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
    Placed {
        kind: Kind,
        filled: bool,
        /// Compared with pacote's extraction before being placed.
        checked: bool,
        /// The entry differed from pacote's extraction (its kind, the first
        /// difference): pacote's was placed, and it replaced the entry.
        differs: Option<(&'static str, String)>,
        entry: Integrity,
        place_secs: f64,
        touch_secs: f64,
        paths: usize,
    },
    /// The hook is to run pacote's extract into `to`, then ask again.
    Extract { to: PathBuf, why: &'static str },
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
    /// Entries extracted into the store (by pacote) during this run.
    pub filled: Vec<Integrity>,
    /// Entries compared with pacote's extraction before being placed.
    pub checked: usize,
    /// Entries that differed from pacote's extraction, by kind: how many,
    /// and the first few as "<integrity>: <first difference>".
    pub differs: BTreeMap<String, (usize, Vec<String>)>,
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
            "checked": self.checked,
            "differs": self.differs.iter().map(|(k, (n, first))| (k.clone(), json!({ "n": n, "first": first }))).collect::<serde_json::Map<_, _>>(),
            "paths_touched": self.paths_touched,
            "serve_wall_secs": r(self.wall_secs()),
            "errors": self.errors,
            "pacote": self.hello.as_ref().and_then(|h| h.get("pacote")).cloned().unwrap_or(Value::Null),
        })
    }
}

/// How many store entries are compared with pacote's extraction before
/// being placed (THINCLONE_INJECT_CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckPolicy {
    /// Every entry, in every install.
    All,
    /// None, whoever made the entry (tests and timings; misses are still
    /// filled by pacote).
    Off,
    /// The rules in the module comment, sampling one in N.
    OneIn(u64),
}

impl CheckPolicy {
    pub const DEFAULT_RATE: u64 = 50;

    pub fn from_env() -> std::result::Result<CheckPolicy, String> {
        match std::env::var("THINCLONE_INJECT_CHECK").as_deref() {
            Err(_) | Ok("") => Ok(CheckPolicy::OneIn(Self::DEFAULT_RATE)),
            Ok("all") => Ok(CheckPolicy::All),
            Ok("off") => Ok(CheckPolicy::Off),
            Ok(n) => match n.parse::<u64>() {
                Ok(n) if n > 0 => Ok(CheckPolicy::OneIn(n)),
                _ => Err(format!("THINCLONE_INJECT_CHECK={n} is not all, off or a number above 0")),
            },
        }
    }
}

pub const CHECKED_FILE: &str = "inject-checked";
pub const STRICT_FILE: &str = "inject-strict";

fn key_state(store: &Store, policy: CheckPolicy, key: String) -> KeyState {
    let check_all = match policy {
        CheckPolicy::All => true,
        CheckPolicy::Off => false,
        CheckPolicy::OneIn(_) => store.key_lines(CHECKED_FILE, &key).is_empty(),
    };
    let strict = AtomicBool::new(!store.key_lines(STRICT_FILE, &key).is_empty());
    KeyState { key, check_all, strict }
}

/// Why an entry differed from pacote's extraction (module comment).
fn differs_kind(made_by: Option<&str>, key: &str) -> &'static str {
    match made_by {
        None => "extractor",
        Some(k) if k == key => "changed",
        Some(_) => "pacote",
    }
}

struct KeyState {
    key: String,
    /// Compare every entry: the key has not been checked yet (or `All`).
    check_all: bool,
    /// Compare every entry another key made (module comment).
    strict: AtomicBool,
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
    /// What the check rules depend on: the pacote and tar versions npm
    /// extracts with, known from the hook's hello (the versions node really
    /// loaded), or given to `new` (tests).
    ks: std::sync::OnceLock<KeyState>,
    policy: CheckPolicy,
    /// Checks or fills that did not complete (pacote's extraction failed, or
    /// the placement was refused after it): the key is not recorded as
    /// checked then.
    incomplete: AtomicU64,
    /// Directories handed out for pacote's extraction, and for which entry.
    issued: Mutex<HashMap<PathBuf, Integrity>>,
    rng: AtomicU64,
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
    /// `key` names the pacote and tar versions npm extracts with: whether it
    /// is in `<store>/inject-checked` decides how many entries are compared.
    pub fn new(store: &'a Store, root: PathBuf, mode: Mode, key: Option<String>, policy: CheckPolicy) -> Result<Server<'a>> {
        let tmp = TmpDir::create(store.root.join("tmp").join(format!("{}-inject", crate::store::process_tag())), 0o700)?;
        let ks = std::sync::OnceLock::new();
        if let Some(k) = key {
            let _ = ks.set(key_state(store, policy, k));
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
            ^ ((std::process::id() as u64) << 32);
        Ok(Server {
            store,
            root,
            mode,
            tmp,
            seq: AtomicU64::new(0),
            unsupported: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            ks,
            policy,
            incomplete: AtomicU64::new(0),
            issued: Mutex::new(HashMap::new()),
            rng: AtomicU64::new(seed | 1),
            stats: Mutex::new(Stats::default()),
        })
    }

    fn ks(&self) -> &KeyState {
        // The hello comes before any request; without one, nothing is known.
        self.ks.get_or_init(|| key_state(self.store, self.policy, "pacote@? tar@?".into()))
    }

    /// The pacote and tar versions the check rules used.
    pub fn key(&self) -> String {
        self.ks().key.clone()
    }

    /// Whether every entry is compared in this run.
    pub fn checks_all(&self) -> bool {
        self.ks().check_all
    }

    /// Every check and fill handed out came back and completed.
    pub fn checks_complete(&self) -> bool {
        self.incomplete.load(Ordering::Relaxed) == 0 && self.issued.lock().unwrap().is_empty()
    }

    /// Whether to compare an entry, made by `made_by`, with pacote's
    /// extraction (module comment).
    fn should_check(&self, made_by: Option<&str>) -> bool {
        let CheckPolicy::OneIn(n) = self.policy else { return self.policy == CheckPolicy::All };
        match made_by {
            None => return true,
            Some(k) if k != self.ks().key && self.ks().strict.load(Ordering::Relaxed) => return true,
            _ if self.ks().check_all => return true,
            _ => {}
        }
        // xorshift64: no statistical quality needed, only an unbiased-enough pick.
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        x % n == 0
    }

    /// A fresh directory for pacote's extraction of `i`.
    fn issue(&self, i: Integrity, why: &'static str) -> Outcome {
        let to = self.tmp.path.join(format!("x{}", self.seq.fetch_add(1, Ordering::Relaxed)));
        // Made here, as pacote would make it (mkdir under umask 022), so the
        // entry's own directory has the mode npm's package directories get.
        if let Err(e) = std::fs::create_dir(&to) {
            return refused("store-error", format!("creating {}: {e}", to.display()));
        }
        self.issued.lock().unwrap().insert(to.clone(), i);
        Outcome::Extract { to, why }
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

    /// Serve one request: validate, find the entry, place it, set the times
    /// of every placed path to now. A missing entry, or one picked for a
    /// check, is answered with a directory for pacote's extraction instead.
    pub fn place_entry(&self, sri: &str, dest: &Path) -> Outcome {
        if self.unsupported.load(Ordering::Relaxed) {
            return Outcome::Refused { reason: "clone-unsupported", message: "this filesystem cannot clone".into(), fatal: true };
        }
        let dst = match self.check_dest(dest) {
            Ok(d) => d,
            Err(m) => return refused("rejected-dest", m),
        };
        let i = match Integrity::parse(sri) {
            Ok(i) => i,
            Err(e) => return refused("rejected-source", format!("{e:#}")),
        };
        let dir = self.store.dir_for(&i);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {
                if self.should_check(self.store.made_by(&i).as_deref()) {
                    return self.issue(i, "check");
                }
                self.finish(i, &dir, &dst, false, false, None)
            }
            Ok(_) => refused("rejected-source", format!("store entry {} is not a directory", dir.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.issue(i, "fill"),
            Err(e) => refused("store-error", format!("{}: {e}", dir.display())),
        }
    }

    /// The hook has run pacote's extract into `extracted` (handed out by
    /// `issue` for this integrity): fill the store with it, or compare the
    /// entry with it, then place.
    pub fn place_extracted(&self, sri: &str, dest: &Path, extracted: &Path) -> Outcome {
        let o = self.place_extracted_inner(sri, dest, extracted);
        if matches!(o, Outcome::Refused { .. }) {
            self.incomplete.fetch_add(1, Ordering::Relaxed);
        }
        o
    }

    fn place_extracted_inner(&self, sri: &str, dest: &Path, extracted: &Path) -> Outcome {
        let i = match Integrity::parse(sri) {
            Ok(i) => i,
            Err(e) => return refused("rejected-source", format!("{e:#}")),
        };
        // Used once, and only for the integrity it was handed out for.
        match self.issued.lock().unwrap().remove(extracted) {
            Some(j) if j == i => {}
            Some(_) => return refused("bad-request", "extracted directory was handed out for another integrity"),
            None => return refused("bad-request", format!("{} was not handed out by this server", extracted.display())),
        }
        let discard = || {
            let _ = std::fs::remove_dir_all(extracted);
        };
        if self.unsupported.load(Ordering::Relaxed) {
            discard();
            return Outcome::Refused { reason: "clone-unsupported", message: "this filesystem cannot clone".into(), fatal: true };
        }
        let dst = match self.check_dest(dest) {
            Ok(d) => d,
            Err(m) => {
                discard();
                return refused("rejected-dest", m);
            }
        };
        if !is_real_dir(extracted) {
            return refused("bad-request", format!("{} is not a directory", extracted.display()));
        }
        self.adopt(i, &dst, extracted)
    }

    /// Fill the entry with pacote's extraction, or compare the entry with it.
    /// Every package that went through pacote's extraction is placed from
    /// that extraction, never from the entry's path: another install, under
    /// another pacote, may replace the entry at any moment.
    fn adopt(&self, i: Integrity, dst: &Path, extracted: &Path) -> Outcome {
        let discard = || {
            let _ = std::fs::remove_dir_all(extracted);
        };
        let entry = self.store.dir_for(&i);
        let ino = |p: &Path| std::fs::symlink_metadata(p).ok().filter(|m| m.is_dir()).map(|m| std::os::unix::fs::MetadataExt::ino(&m));
        if let Some(ino_before) = ino(&entry) {
            // A check, or a fill that another install made first: compare.
            let made_by = self.store.made_by(&i);
            let differs = match crate::store::compare_trees(extracted, &entry) {
                Ok(d) => d,
                Err(e) => {
                    discard();
                    return refused("store-error", format!("comparing with {}: {e:#}", entry.display()));
                }
            };
            let Some(d) = differs else {
                let o = self.finish(i.clone(), extracted, dst, false, true, None);
                discard();
                // The record says this pacote extracts the entry to the same
                // tree; written only if the entry is still the one compared
                // (a replacement by another install changes its inode and
                // its record). A replacement between this check and the write
                // is not excluded (NOTES).
                if made_by.as_deref() != Some(self.ks().key.as_str())
                    && ino(&entry) == Some(ino_before)
                    && self.store.made_by(&i) == made_by
                {
                    self.mark_made_by(&i);
                }
                return o;
            };
            let kind = differs_kind(made_by.as_deref(), &self.ks().key);
            if kind == "pacote" && !self.ks().strict.swap(true, Ordering::Relaxed) {
                let note = format!("{} (made by {}): {d}", i.key(), made_by.as_deref().unwrap_or("?"));
                if let Err(e) = self.store.add_key_line(STRICT_FILE, &self.ks().key, &note) {
                    crate::info!("could not record {} in {STRICT_FILE}: {e:#}", self.ks().key);
                }
            }
            let o = self.finish(i.clone(), extracted, dst, false, true, Some((kind, d)));
            // Then the entry is replaced by pacote's extraction (module
            // comment). The record goes first: whatever happens next, an
            // entry left here is never taken for this pacote's without a
            // new record.
            self.store.forget_made_by(&i);
            let aside = self.tmp.path.join(format!("q{}", self.seq.fetch_add(1, Ordering::Relaxed)));
            let healed = std::fs::rename(&entry, &aside).is_ok() && std::fs::rename(extracted, &entry).is_ok();
            self.store.forget_size(&i);
            if healed {
                self.mark_made_by(&i);
            } else {
                // Someone else moved the entry meanwhile.
                discard();
            }
            return o;
        }
        let mut o = self.finish(i.clone(), extracted, dst, false, false, None);
        let parent = entry.parent().unwrap();
        if let Err(e) = std::fs::create_dir_all(parent) {
            discard();
            crate::info!("inject: could not create {}: {e}", parent.display());
            return o;
        }
        // Rename-if-absent: a directory renamed onto an existing, non-empty
        // one fails, so concurrent installs fill an entry once.
        match std::fs::rename(extracted, &entry) {
            Ok(()) => {
                self.store.forget_size(&i);
                self.mark_made_by(&i);
                // Counted here: the entry is in the store now, whatever
                // became of the placement.
                self.stats.lock().unwrap().filled.push(i);
                if let Outcome::Placed { filled, .. } = &mut o {
                    *filled = true;
                }
            }
            // Another install filled it while pacote extracted here (theirs
            // is compared when its maker calls for it, decision 4 in NOTES).
            Err(_) => discard(),
        }
        o
    }

    fn mark_made_by(&self, i: &Integrity) {
        if let Err(e) = self.store.set_made_by(i, &self.ks().key) {
            crate::info!("could not record which pacote made {}: {e:#}", i.key());
        }
    }

    /// Clone `src` (the entry, or pacote's extraction) to `dst` and touch it.
    fn finish(&self, entry: Integrity, src: &Path, dst: &Path, filled: bool, checked: bool, differs: Option<(&'static str, String)>) -> Outcome {
        let t0 = Instant::now();
        let kind = match self.place(src, dst) {
            Ok(k) => k,
            Err(o) => return o,
        };
        let t1 = Instant::now();
        // npm ci leaves every extracted path with the install time (pacote
        // extracts with noMtime); a clone carries the store's. Done here, per
        // package and before the answer, so every touch is over before npm
        // runs an install script, whose files must keep their later times.
        let paths = match clone::touch_placed(src, dst) {
            Ok(n) => n,
            Err(e) => return refused("clone-error", format!("{e:#}")),
        };
        Outcome::Placed {
            kind,
            filled,
            checked,
            differs,
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
            Outcome::Placed { kind, entry, place_secs, touch_secs, paths, checked, differs, .. } => {
                s.cloned += 1;
                if *checked {
                    s.checked += 1;
                }
                if let Some((kind, d)) = differs {
                    let k = s.differs.entry(kind.to_string()).or_default();
                    k.0 += 1;
                    if k.1.len() < 3 {
                        k.1.push(format!("{}: {d}", entry.key()));
                    }
                }
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
                if s.errors.len() < 5 && !s.errors.contains(&line) {
                    s.errors.push(line);
                }
            }
            // Answered again once pacote has extracted.
            Outcome::Extract { .. } => {}
        }
    }

    /// Record a message that needs no answer (`hello`, `note`). Returns
    /// false when `v` is something else.
    fn record_message(&self, v: &Value) -> bool {
        if v.get("hello").is_some() {
            let ver = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("?").to_string();
            let _ = self.ks.set(key_state(self.store, self.policy, format!("pacote@{} tar@{}", ver("pacote"), ver("tar"))));
            self.stats.lock().unwrap().hello = Some(v.clone());
            return true;
        }
        if let Some(note) = v.get("note").and_then(|n| n.as_str()) {
            // A reason is a short word chosen by the hook; keep the log sane
            // whatever arrives.
            let key: String = note.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
            if key == "pacote-extract-failed" {
                self.incomplete.fetch_add(1, Ordering::Relaxed);
            }
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
                // A panic in one placement must not take the answer with it:
                // the hook would wait for ever.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match str_of("extracted") {
                    Some(x) => self.place_extracted(sri, Path::new(dest), Path::new(x)),
                    None => self.place_entry(sri, Path::new(dest)),
                }))
                .unwrap_or_else(|_| refused("clone-error", "internal error (panic)"))
            }
            _ => refused("bad-request", "integrity and dest are required"),
        };
        self.tally(&outcome);
        Some(
            match outcome {
                Outcome::Placed { kind, filled, checked, .. } => {
                    json!({ "id": id, "ok": true, "kind": self.kind_name(kind), "filled": filled, "checked": checked })
                }
                Outcome::Extract { to, why } => json!({ "id": id, "ok": false, "extract_to": to, "why": why }),
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
    use crate::store::Ensure;
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

        /// No checks: the placement tests place entries directly.
        fn server(&self, mode: Mode) -> Server<'_> {
            self.server_with(mode, CheckPolicy::Off)
        }

        fn server_with(&self, mode: Mode, policy: CheckPolicy) -> Server<'_> {
            Server::new(&self.store, self.root.clone(), mode, Some("pacote@1.0.0 tar@2.0.0".into()), policy).unwrap()
        }

        /// What the hook does with an `extract_to` answer, with thinclone's own
        /// extractor standing in for pacote: the tarball of `i` from the fake
        /// npm cache, extracted into `to`.
        fn pacote_extract(&self, i: &Integrity, to: &Path) {
            let data = fs::read(i.split_path(&self.cacache.join("content-v2"))).unwrap();
            crate::store::extract(&data, to).unwrap();
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
        assert_eq!(placed(s.place_entry(&sri, &dst)), (Kind::Fast, false));
        assert_eq!(listing(&dst), want);
        assert_eq!(fs::read_to_string(dst.join("lib/index.js")).unwrap(), "module.exports = 1\n");
        // A destination that does not exist (its parent does).
        fs::create_dir(fx.root.join("node_modules/@scope")).unwrap();
        let dst2 = fx.root.join("node_modules/@scope/q");
        assert_eq!(placed(s.place_entry(&sri, &dst2)), (Kind::Fast, false));
        assert_eq!(listing(&dst2), want);
        // The clone is independent: writing into it leaves the store alone.
        fs::write(dst.join("lib/index.js"), "changed\n").unwrap();
        assert_eq!(fs::read_to_string(fx.store.dir_for(&i).join("lib/index.js")).unwrap(), "module.exports = 1\n");
        // A directory that already holds the package's own files is npm's to
        // handle (pacote empties it first).
        let (reason, _, fatal) = refusal(s.place_entry(&sri, &dst));
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
        assert_eq!(placed(s.place_entry(&sri, &dst)), (Kind::Fast, false));
        assert_eq!(listing(&dst), listing(&fx.store.dir_for(&i)));
        // Merge with directory clones.
        let dst2 = fx.root.join("node_modules/q");
        fs::create_dir_all(dst2.join("node_modules/child")).unwrap();
        assert_eq!(placed(s.place_entry(&sri, &dst2)), (Kind::Merge, false));
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
            assert_eq!(placed(s.place_entry(&sri, &dst)), (Kind::Merge, false));
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
            let (reason, _, _) = refusal(s.place_entry(bad, &dst));
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
        let (reason, msg, _) = refusal(s.place_entry(&sri_of(&i), &dst));
        assert_eq!(reason, "rejected-source", "{msg}");
        // A file where an entry belongs.
        let j = Integrity { algo: "sha512".into(), hex: "cd".repeat(64) };
        fs::create_dir_all(fx.store.dir_for(&j).parent().unwrap()).unwrap();
        fs::write(fx.store.dir_for(&j), "x").unwrap();
        assert_eq!(refusal(s.place_entry(&sri_of(&j), &dst)).0, "rejected-source");
        // An entry not in the store: pacote is asked to extract it.
        let k = Integrity { algo: "sha512".into(), hex: "ef".repeat(64) };
        assert!(matches!(s.place_entry(&sri_of(&k), &dst), Outcome::Extract { why: "fill", .. }));
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
            let (reason, msg, fatal) = refusal(s.place_entry(&sri, dest));
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
            assert_eq!(placed(s.place_entry(&sri, &fast)).0, Kind::Fast);
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
            assert_eq!(placed(s.place_entry(&sri, &merge)).0, Kind::Merge);
            for p in ["", "package.json", "lib", "lib/deep", "lib/deep/x.js", "node_modules", "node_modules/inner", "node_modules/inner/i.js"] {
                assert!(mtime(&merge.join(p)) >= start, "{mode:?} merge: {p:?} kept the store's time");
            }
            assert!(mtime(&other) < start, "{mode:?}: another package's file was touched");
        }
        // The store keeps its own times.
        assert!(mtime(&fx.store.dir_for(&i).join("lib/deep/x.js")) < start);
    }

    fn extract_to(o: Outcome, want: &str) -> PathBuf {
        match o {
            Outcome::Extract { to, why } if why == want => to,
            o => panic!("expected extract_to ({want}), got {o:?}"),
        }
    }

    fn placed_full(o: Outcome) -> (Kind, bool, bool, Option<(&'static str, String)>) {
        match o {
            Outcome::Placed { kind, filled, checked, differs, .. } => (kind, filled, checked, differs),
            o => panic!("expected a placement, got {o:?}"),
        }
    }

    #[test]
    fn a_missing_entry_is_filled_with_pacotes_extraction() {
        let fx = Fx::new("serve-fill");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        let s = fx.server(clone::Mode::File);
        let dst = fx.root.join("node_modules/p");
        let to = extract_to(s.place_entry(&sri_of(&i), &dst), "fill");
        assert!(to.starts_with(&s.tmp.path) && to.is_dir(), "a fresh directory of the server's: {}", to.display());
        assert!(!fx.store.dir_for(&i).exists() && !dst.exists(), "nothing in the store or the project yet");
        fx.pacote_extract(&i, &to);
        let want = listing(&to);
        assert_eq!(placed_full(s.place_extracted(&sri_of(&i), &dst, &to)), (Kind::Fast, true, false, None));
        assert_eq!(listing(&fx.store.dir_for(&i)), want, "pacote's extraction became the entry");
        assert_eq!(fx.store.made_by(&i).as_deref(), Some(KEY), "and is recorded as made by this pacote");
        assert_eq!(listing(&dst), want);
        assert!(!to.exists(), "renamed into the store");
        assert_eq!(s.stats.lock().unwrap().filled, vec![i.clone()]);
        // A directory is used once, for its integrity only, and only if this
        // server handed it out.
        let dst2 = fx.root.join("node_modules/q");
        assert_eq!(refusal(s.place_extracted(&sri_of(&i), &dst2, &to)).0, "bad-request", "used twice");
        let made_up = fx.dir.join("elsewhere");
        fs::create_dir_all(&made_up).unwrap();
        assert_eq!(refusal(s.place_extracted(&sri_of(&i), &dst2, &made_up)).0, "bad-request", "not handed out");
        let j = Integrity { algo: "sha512".into(), hex: "ef".repeat(64) };
        let to_j = extract_to(s.place_entry(&sri_of(&j), &dst2), "fill");
        assert_eq!(refusal(s.place_extracted(&sri_of(&i), &dst2, &to_j)).0, "bad-request", "another integrity");
        assert!(!dst2.exists());
    }

    #[test]
    fn an_entry_filled_meanwhile_by_another_install_is_compared_not_replaced() {
        let fx = Fx::new("serve-fill-race");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        let s = fx.server(clone::Mode::File);
        let dst = fx.root.join("node_modules/p");
        let to = extract_to(s.place_entry(&sri_of(&i), &dst), "fill");
        fx.pacote_extract(&i, &to);
        // Another install fills the entry while this one's pacote extracts.
        assert!(matches!(fx.store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted));
        let before = fs::metadata(fx.store.dir_for(&i)).unwrap().ino();
        assert_eq!(placed_full(s.place_extracted(&sri_of(&i), &dst, &to)), (Kind::Fast, false, true, None));
        assert_eq!(fs::metadata(fx.store.dir_for(&i)).unwrap().ino(), before, "the other install's entry stays");
        assert!(!to.exists());
        assert!(s.stats.lock().unwrap().filled.is_empty());
    }

    const KEY: &str = "pacote@1.0.0 tar@2.0.0";

    /// Check `dst` with pacote's extraction of `i`, changed by `change`
    /// first (the stand-in for a pacote whose rules differ from the entry's).
    fn check_once(fx: &Fx, s: &Server, i: &Integrity, dst: &Path, change: impl Fn(&Path)) -> (Option<(&'static str, String)>, Vec<String>) {
        let to = extract_to(s.place_entry(&sri_of(i), dst), "check");
        fx.pacote_extract(i, &to);
        change(&to);
        let want = listing(&to);
        let (kind, filled, checked, differs) = placed_full(s.place_extracted(&sri_of(i), dst, &to));
        assert_eq!((kind, filled, checked), (Kind::Fast, false, true));
        assert!(!to.exists());
        assert_eq!(listing(dst), want, "pacote's extraction is what was placed");
        assert_eq!(listing(&fx.store.dir_for(i)), want, "and what the entry holds now");
        assert_eq!(fx.store.made_by(i).as_deref(), Some(KEY));
        (differs, want)
    }

    fn chmod_644(to: &Path) {
        fs::set_permissions(to.join("bin/cli"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    }

    #[test]
    fn a_checked_entry_that_differs_is_replaced_and_classified_by_who_made_it() {
        let fx = Fx::new("serve-check");
        if !fx.can_clone() {
            return;
        }
        let (i, _) = fx.entry(PKG);
        let s = fx.server_with(clone::Mode::File, CheckPolicy::All);
        // The same tree: kept, and now known to be this pacote's.
        assert_eq!(fx.store.made_by(&i), None, "made by thinclone's extractor");
        let (d, _) = check_once(&fx, &s, &i, &fx.root.join("node_modules/a"), |_| {});
        assert_eq!(d, None);
        // Made by this same pacote, and different now: changed since.
        let (d, _) = check_once(&fx, &s, &i, &fx.root.join("node_modules/b"), chmod_644);
        assert!(d.as_ref().is_some_and(|(k, m)| *k == "changed" && m.contains("bin/cli")), "{d:?}");
        assert!(fx.store.key_lines(STRICT_FILE, KEY).is_empty());
        // Made by thinclone's extractor: replaced, nothing else.
        let (j, _) = fx.entry(&[("package.json", 0o644, "{\"name\":\"j\"}\n"), ("bin/cli", 0o755, "x\n")]);
        let (d, _) = check_once(&fx, &s, &j, &fx.root.join("node_modules/c"), chmod_644);
        assert_eq!(d.map(|x| x.0), Some("extractor"));
        assert!(fx.store.key_lines(STRICT_FILE, KEY).is_empty());
        // Made by an earlier pacote: the rules changed; this key is strict.
        let (k, _) = fx.entry(&[("package.json", 0o644, "{\"name\":\"k\"}\n"), ("bin/cli", 0o755, "y\n")]);
        fx.store.set_made_by(&k, "pacote@0.9.0 tar@1.0.0").unwrap();
        assert!(!s.ks().strict.load(Ordering::Relaxed));
        let (d, _) = check_once(&fx, &s, &k, &fx.root.join("node_modules/d"), chmod_644);
        assert_eq!(d.map(|x| x.0), Some("pacote"));
        assert!(s.ks().strict.load(Ordering::Relaxed));
        let lines = fx.store.key_lines(STRICT_FILE, KEY);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&k.key()) && lines[0].contains("pacote@0.9.0"), "{}", lines[0]);
    }

    #[test]
    fn a_match_is_placed_from_pacotes_extraction_not_from_the_entry_path() {
        // Directory modes are not compared, so an entry and an extraction can
        // match while differing there: the placed directory shows which was
        // cloned. The extraction is the tree just compared; the entry's path
        // may be replaced by another install meanwhile.
        let fx = Fx::new("serve-match-source");
        if !fx.can_clone() {
            return;
        }
        // Directory clones keep directory modes; per-file clones make the
        // directories anew, so there is nothing to see on Linux.
        if crate::default_clone_mode() != clone::Mode::Dir {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        fs::set_permissions(fx.store.dir_for(&i).join("lib"), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let s = fx.server_with(clone::Mode::Dir, CheckPolicy::All);
        let dst = fx.root.join("node_modules/p");
        let to = extract_to(s.place_entry(&sri, &dst), "check");
        fx.pacote_extract(&i, &to);
        assert_eq!(placed_full(s.place_extracted(&sri, &dst, &to)).3, None, "a match");
        assert_eq!(fs::metadata(dst.join("lib")).unwrap().mode() & 0o777, 0o755, "cloned from the extraction");
    }

    #[test]
    fn the_key_is_recorded_as_checked_only_when_every_check_completed() {
        let fx = Fx::new("serve-complete");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        let s = fx.server_with(clone::Mode::File, CheckPolicy::All);
        assert!(s.checks_complete());
        // Handed out, not come back yet.
        let to = extract_to(s.place_entry(&sri, &fx.root.join("node_modules/a")), "check");
        assert!(!s.checks_complete());
        fx.pacote_extract(&i, &to);
        placed_full(s.place_extracted(&sri, &fx.root.join("node_modules/a"), &to));
        assert!(s.checks_complete());
        // pacote's extraction failed in the hook.
        assert!(s.record_message(&json!({ "note": "pacote-extract-failed" })));
        assert!(!s.checks_complete());
        // A placement refused after the extraction.
        let t = Fx::new("serve-complete-2");
        let (j, sri_j) = t.entry(PKG);
        let s2 = t.server_with(clone::Mode::File, CheckPolicy::All);
        let to = extract_to(s2.place_entry(&sri_j, &t.root.join("node_modules/a")), "check");
        t.pacote_extract(&j, &to);
        assert_eq!(refusal(s2.place_extracted(&sri_j, &t.dir.join("outside/node_modules/a"), &to)).0, "rejected-dest");
        assert!(!s2.checks_complete());
    }

    #[test]
    fn the_key_comes_from_the_hooks_hello() {
        let fx = Fx::new("serve-key");
        let s = Server::new(&fx.store, fx.root.clone(), clone::Mode::File, None, CheckPolicy::OneIn(50)).unwrap();
        assert!(s.record_message(&json!({ "hello": true, "pacote": "21.5.1", "tar": "7.5.22", "hook": "on" })));
        assert_eq!(s.key(), "pacote@21.5.1 tar@7.5.22");
        assert!(s.checks_all(), "not in inject-checked");
    }

    #[test]
    fn a_classic_fill_drops_a_record_left_by_an_earlier_entry() {
        let fx = Fx::new("serve-made-by-stale");
        let (i, _) = fx.entry(PKG);
        fx.store.set_made_by(&i, KEY).unwrap();
        // The entry goes (an interrupted replacement, a hand removal) and
        // thinclone's extractor makes it again.
        fs::remove_dir_all(fx.store.dir_for(&i)).unwrap();
        assert!(matches!(fx.store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted));
        assert_eq!(fx.store.made_by(&i), None);
    }

    #[test]
    fn which_entries_are_checked() {
        let fx = Fx::new("serve-policy");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        // A fresh destination each time: an entry not picked is placed, and
        // a second placement at the same path would be refused for another
        // reason.
        let n = std::sync::atomic::AtomicUsize::new(0);
        let checks = |s: &Server| {
            let dst = fx.root.join(format!("node_modules/p{}", n.fetch_add(1, Ordering::Relaxed)));
            match s.place_entry(&sri, &dst) {
                Outcome::Extract { why: "check", .. } => true,
                Outcome::Placed { .. } => false,
                o => panic!("{o:?}"),
            }
        };
        // (One server at a time: each owns the process's tmp directory.)
        // Off: none, whoever made the entry.
        assert!(!checks(&fx.server_with(clone::Mode::File, CheckPolicy::Off)));
        let never = || fx.server_with(clone::Mode::File, CheckPolicy::OneIn(1_000_000));
        // Made by thinclone's extractor: always.
        fx.store.add_key_line(CHECKED_FILE, KEY, "test").unwrap();
        assert!(checks(&never()), "an entry no pacote made");
        // Made by this key: sampled.
        fx.store.set_made_by(&i, KEY).unwrap();
        assert!(!checks(&never()));
        {
            let later = fx.server_with(clone::Mode::File, CheckPolicy::OneIn(4));
            assert!(!later.checks_all());
            let picks = (0..4000).filter(|_| later.should_check(Some(KEY))).count();
            assert!((700..1300).contains(&picks), "{picks} of 4000 at one in 4");
        }
        // Made by an earlier pacote: sampled, unless this key is strict.
        fx.store.set_made_by(&i, "pacote@0.9.0 tar@1.0.0").unwrap();
        assert!(!checks(&never()));
        fx.store.add_key_line(STRICT_FILE, KEY, "test").unwrap();
        assert!(checks(&never()), "strict: every entry an earlier pacote made");
        fx.store.set_made_by(&i, KEY).unwrap();
        assert!(!checks(&never()), "strict does not touch this key's own entries");
        // A key not in inject-checked: every entry, its own included.
        let fresh = Fx::new("serve-policy-fresh");
        let (j, sri_j) = fresh.entry(PKG);
        fresh.store.set_made_by(&j, KEY).unwrap();
        let s = fresh.server_with(clone::Mode::File, CheckPolicy::OneIn(1_000_000));
        assert!(s.checks_all());
        assert!(matches!(s.place_entry(&sri_j, &fresh.root.join("node_modules/p")), Outcome::Extract { why: "check", .. }));
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
        let (reason, _, fatal) = refusal(s.place_entry(&sri, &dst));
        assert_eq!((reason, fatal), ("clone-unsupported", true));
        assert!(!dst.exists());
        let e = anyhow::Error::new(clone::CloneError::for_test(libc::EXDEV));
        assert!(clone::unsupported(&e) && !already_exists(&e));
        let e = anyhow::Error::new(clone::CloneError::for_test(libc::EEXIST));
        assert!(!clone::unsupported(&e) && already_exists(&e));
    }
}
