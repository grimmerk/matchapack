//! The clone server of the inject engine (see inject.rs and NOTES "Inject
//! engine"): npm runs its own `npm ci` in a node child, and for every package
//! it would extract, the hook in src/inject.js asks this process to place the
//! package's store entry instead. Node cannot clone (see NOTES), so the clone
//! and the mtime touch happen here.
//!
//! The project root is npm's own (grimmerk/matchapack-private#36): when npm
//! starts its command, the helper reports `npm.prefix` (the `root` message),
//! and this server runs the gates on it (`accept_root`) and pins it. Until
//! then every request is refused (`no-root`); a refused root makes the helper
//! exit before the command runs, and matchapack runs plain npm.
//!
//! Protocol: one JSON object per line, both ways, over one socket.
//!   node → matchapack
//!     {"hello":true,"pacote":"21.5.1","tar":"7.5.22","uv":"1.52.1","fingerprint":"…",
//!      "hook":"pending"}                             once, before npm starts
//!     {"key":"fetcher:… tar:… uv:1.52.1","tar_fingerprint":"…"}
//!                                                    the store key of the extraction
//!                                                    code (`code_key`), once, when npm
//!                                                    has loaded pacote, before the hook
//!                                                    (null, with `key_error`: unknown)
//!     {"id":0,"root":"/abs/dir","global":false,"command":"ci","typed":"ic",
//!      "dry_run":false,"package_lock_only":false,"usage":false}
//!                                                    npm's install root and command,
//!                                                    once, before the command runs
//!     {"no_root":"…"}                                npm's root could not be read: no
//!                                                    root, every request refused
//!     {"hook":"on"}                                  npm loaded pacote: the hook is on
//!                                                    (`off`, `none`; `not-installed`
//!                                                    with a `reason`: npm extracts
//!                                                    every package itself)
//!     {"id":7,"integrity":"sha512-…","dest":"/abs/dir"}
//!     {"id":8,"integrity":"sha512-…","dest":"/abs/dir","extracted":"/…/tmp/…/x3"}
//!     {"note":"type-FileFetcher"}                    a package the hook left to npm, and why
//!     {"result_read":"foo","expected":false}         npm read this key of what the hook's
//!                                                    extract resolved to (`expected`:
//!                                                    resolved, integrity or from)
//!     {"id":"declined","declined":"…"}               the hook stops before npm starts, and why
//!   matchapack → node
//!     {"id":0,"ok":true,"root":"/abs/dir"}           the root, canonical, pinned
//!     {"id":0,"ok":false,"reason":"…"}               refused (recorded as the decline
//!                                                    reason; the helper then exits 90)
//!     {"id":7,"ok":true,"kind":"fast","filled":false,"checked":false}
//!     {"id":7,"ok":false,"extract_to":"/…/tmp/…/x3","why":"fill"}
//!     {"id":7,"ok":false,"reason":"rejected-dest","message":"…","fatal":false}
//!     {"id":"declined","ok":true}                    recorded (the helper then exits 90)
//! Requests are served concurrently (npm starts every extraction at once);
//! answers come back in any order. `fatal` means no later request can succeed
//! (the filesystem cannot clone): the hook stops asking.
//!
//! The store is filled by pacote, never by matchapack's own extractor: for a
//! package not in the store the answer is `extract_to`, a fresh directory in
//! this server's temporary directory; the hook runs pacote's own extract into
//! it (the very fetcher npm made for that package) and sends the request
//! again with `extracted`. The directory then becomes the store entry
//! (renamed in only if no other install got there first) and is cloned.
//! The same round trip checks the store against pacote (`why: "check"`): the
//! entry is compared with pacote's extraction (`store::compare_trees`) before
//! anything is placed. Each entry records the extraction code under which it
//! is known to be what pacote extracts (`store.made_by`; keys such as
//! `fetcher:<32 hex> tar:<32 hex> uv:1.52.1`, `code_key`: pacote's
//! lib/fetcher.js, the tar files node loaded with tar's dependencies, and
//! libuv's version; NOTES "PR C1: the key is the code"): the code that made
//! it, and each one that later extracted it to the same tree. An entry is
//! compared before use **once per key**: whenever the code that is running is
//! not in the record yet (matchapack's own extractor, the classic engine's
//! until PR C2, recorded nothing; records written before PR C1 of #39 hold
//! `pacote@… tar@…` keys, which no code key matches), and then added to it;
//! an entry whose record has this key is compared one time in
//! MATCHAPACK_INJECT_CHECK (default 50), at random, to catch an entry changed
//! since. pacote or tar extract differently across versions for odd tarballs
//! (tar 6 extracts a file named `a\..\b` that tar 7 refuses), and no install
//! under one version can vouch for entries it did not use, so every code
//! checks every entry it uses, once.
//! The comparison also checks pacote's extraction itself
//! (`store::compare_extraction`): only files and directories, every file
//! written by this install. Otherwise the clones (touched to now) are not
//! what npm puts on disk: the hook stops for this run (a fatal refusal), the
//! install counts as broken (npm ci rebuilds node_modules, inject.rs), and
//! the key goes on the store's decline list (`store.declined`), under which
//! every later request is refused the same way: npm extracts every package.
//! A difference places pacote's extraction and replaces the entry with it
//! (renamed aside first: an install cloning from it at that moment gets the
//! whole old entry or an error, never a mix); the record is then this key
//! only. Its kind: `extractor` (nothing recorded: matchapack's extractor made
//! it; expected for odd tarballs), `changed` (this key recorded: the entry was
//! changed since), `pacote` (only other keys recorded: their rules differ
//! from this one's for this tarball).
//!
//! Nothing in a request is trusted as a path to read from or write to:
//!   - the source is named by its integrity, and the entry path is derived
//!     from the parsed digest, so it is always inside the store's layout; it
//!     must be a directory, not a symlink;
//!   - the destination must be an absolute path whose parent really is (after
//!     resolving symlinks) a directory inside the pinned root and below a
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
        /// Paths touched.
        paths: usize,
        /// Paths given the group npm's extraction would have given them
        /// (#41: a placement moved in from the store's tmp/ keeps the
        /// store's group).
        regrouped: usize,
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
    /// The install root npm reported and this server accepted, canonical.
    pub root: Option<PathBuf>,
    /// npm's name of the command (`ci`), with the root.
    pub command: Option<String>,
    /// Why npm's root could not be read (the helper's `no_root`).
    pub no_root: Option<String>,
    /// The hook's state once npm loaded pacote: `on`, `off`, `none`, or
    /// `not-installed` (then `hook_reason`). None: npm never loaded the
    /// pacote the helper hashed.
    pub hook: Option<String>,
    pub hook_reason: Option<String>,
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
    pub paths_regrouped: usize,
    /// The first few refusal messages, for the log.
    pub errors: Vec<String>,
    /// The SHA-256 of tar's loaded code (the helper's `key` message), for
    /// the log and `matchapack report`; `key_error` when the key could not
    /// be named.
    pub tar_fingerprint: Option<String>,
    pub key_error: Option<String>,
    /// Comparisons made because the entry's record lacked the running key
    /// (not the sample): the work a new extraction code costs.
    pub new_key_checks: usize,
    /// pacote's extraction was not what a clone reproduces (serve.rs module
    /// comment): the run is broken, the key declined.
    pub extraction_unexpected: Option<String>,
    /// The running key is on the store's decline list: why.
    pub key_declined: Option<String>,
    /// Why the helper stopped before npm's command ran: its own `declined`
    /// (before npm started), or this server's refusal of npm's root.
    pub declined: Option<String>,
    /// Reads of `resolved`, `integrity` or `from` of what the hook's extract
    /// resolved to, by key: npm does not read them today; noted.
    pub result_reads: BTreeMap<String, usize>,
    /// Reads of any other key of it: npm uses something of pacote's result
    /// the hook may not give, so the tree may not be npm's (inject.rs then
    /// reports the install as broken).
    pub result_unexpected: BTreeMap<String, usize>,
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
            "paths_regrouped": self.paths_regrouped,
            "serve_wall_secs": r(self.wall_secs()),
            "errors": self.errors,
            "pacote": self.hello.as_ref().and_then(|h| h.get("pacote")).cloned().unwrap_or(Value::Null),
            "tar": self.hello.as_ref().and_then(|h| h.get("tar")).cloned().unwrap_or(Value::Null),
            "uv": self.hello.as_ref().and_then(|h| h.get("uv")).cloned().unwrap_or(Value::Null),
            "fingerprint": self.hello.as_ref().and_then(|h| h.get("fingerprint")).cloned().unwrap_or(Value::Null),
            "tar_fingerprint": self.tar_fingerprint,
            "key_error": self.key_error,
            "new_key_checks": self.new_key_checks,
            "extraction_unexpected": self.extraction_unexpected,
            "key_declined": self.key_declined,
            "root": self.root.as_ref().map(|r| r.display().to_string()),
            "command": self.command,
            "hook": self.hook,
            "hook_reason": self.hook_reason,
            "no_root": self.no_root,
            "result_reads": self.result_reads,
            "result_unexpected": self.result_unexpected,
        })
    }
}

/// How many store entries are compared with pacote's extraction before
/// being placed (MATCHAPACK_INJECT_CHECK).
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
        match std::env::var("MATCHAPACK_INJECT_CHECK").as_deref() {
            Err(_) | Ok("") => Ok(CheckPolicy::OneIn(Self::DEFAULT_RATE)),
            Ok("all") => Ok(CheckPolicy::All),
            Ok("off") => Ok(CheckPolicy::Off),
            Ok(n) => match n.parse::<u64>() {
                Ok(n) if n > 0 => Ok(CheckPolicy::OneIn(n)),
                _ => Err(format!("MATCHAPACK_INJECT_CHECK={n} is not all, off or a number above 0")),
            },
        }
    }
}

/// Why an entry differed from pacote's extraction (module comment).
pub(crate) fn differs_kind(made_by: &[String], key: &str) -> &'static str {
    if made_by.is_empty() {
        "extractor"
    } else if made_by.iter().any(|k| k == key) {
        "changed"
    } else {
        "pacote"
    }
}

/// The key of extraction code the hook could not name: it vouches for
/// nothing, so every entry is compared.
pub const UNKNOWN_KEY: &str = "pacote@? tar@?";

/// A key the hook could not name vouches for nothing.
fn known_key(key: &str) -> bool {
    !key.contains('?')
}

/// The store key from a helper's message (the inject hook's `key` message,
/// store verify's hello): its `key` when it has exactly the shape
/// `fetcher:<32 hex> tar:<32 hex> uv:<digits and dots>` (src/inject.js
/// `extractionKey`), else UNKNOWN_KEY.
pub fn code_key(v: &Value) -> String {
    let hex = |s: &str| s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let version = |s: &str| !s.is_empty() && s.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let ok = v.get("key").and_then(|k| k.as_str()).filter(|k| {
        let mut parts = k.split(' ');
        let (Some(f), Some(t), Some(u), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return false };
        f.strip_prefix("fetcher:").is_some_and(hex) && t.strip_prefix("tar:").is_some_and(hex) && u.strip_prefix("uv:").is_some_and(version)
    });
    ok.map(String::from).unwrap_or_else(|| UNKNOWN_KEY.to_string())
}

/// The commands whose install root the server accepts (npm's names, after
/// its alias resolution): what the shell function routes.
const ROOT_COMMANDS: &[&str] = &["ci"];

pub struct Server<'a> {
    store: &'a Store,
    /// The install root npm reported (canonical), set once by the `root`
    /// message (or given to `new`, tests); requests are refused until then.
    root: std::sync::OnceLock<PathBuf>,
    /// Whether the root must be inside the trial scope (trial::in_scope):
    /// the shell function's runs, not `matchapack install`.
    trial_scope: AtomicBool,
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
    /// The key of the extraction code npm runs (`code_key`), from the
    /// hook's `key` message (sent when npm has loaded pacote, before any
    /// request), or given to `new` (tests).
    key: std::sync::OnceLock<String>,
    /// When this install began (this server was made before node started):
    /// pacote's extraction must not be older (`store::compare_extraction`).
    started: std::time::SystemTime,
    /// Set when no further request is served from the store in this run (a
    /// declined key, or an extraction no clone reproduces): every request is
    /// refused, fatally, with this reason and message.
    halted: std::sync::OnceLock<(&'static str, String)>,
    policy: CheckPolicy,
    /// Directories handed out for pacote's extraction, and for which entry.
    issued: Mutex<HashMap<PathBuf, Integrity>>,
    /// Integrities being filled by pacote in this run, with the requests
    /// for the same package that wait for that fill (id, destination): a
    /// package installed at several paths is extracted once.
    filling: Mutex<HashMap<Integrity, Vec<(Value, PathBuf)>>>,
    /// Entries filled in this run, with the inode of the directory published:
    /// while the entry is still that directory, its files have just been
    /// extracted by this install's pacote (a clone needs no touch, waiters
    /// may be given it). Another install may replace it any time after.
    filled_now: Mutex<HashMap<Integrity, u64>>,
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

/// The group (and directory setgid bit) npm's extraction gives the paths of
/// a package placed at `dst` (`clone::new_paths` of its parent, which npm
/// made).
fn npm_gid(dst: &Path) -> Result<clone::NewPaths> {
    let parent = dst.parent().context("the destination has no parent")?;
    clone::new_paths(parent).with_context(|| format!("reading {}", parent.display()))
}

/// Whether `p` (not followed) has the group `g`.
fn has_gid(p: &Path, g: u32) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| std::os::unix::fs::MetadataExt::gid(&m) == g)
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
    /// `root`, when given, must be the canonical project directory (tests);
    /// None: npm reports it (the `root` message).
    /// `key` names the pacote and tar versions npm extracts with (None: from
    /// the hook's hello); an entry not yet known to it is compared before use.
    pub fn new(store: &'a Store, root: Option<PathBuf>, mode: Mode, key: Option<String>, policy: CheckPolicy) -> Result<Server<'a>> {
        let tmp = TmpDir::create(store.root.join("tmp").join(format!("{}-inject", crate::store::process_tag())), 0o700)?;
        let key_cell = std::sync::OnceLock::new();
        if let Some(k) = key {
            let _ = key_cell.set(k);
        }
        let root_cell = std::sync::OnceLock::new();
        if let Some(r) = root {
            let _ = root_cell.set(r);
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)
            ^ ((std::process::id() as u64) << 32);
        Ok(Server {
            store,
            root: root_cell,
            trial_scope: AtomicBool::new(false),
            mode,
            tmp,
            seq: AtomicU64::new(0),
            unsupported: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            key: key_cell,
            started: std::time::SystemTime::now(),
            halted: std::sync::OnceLock::new(),
            policy,
            issued: Mutex::new(HashMap::new()),
            filling: Mutex::new(HashMap::new()),
            filled_now: Mutex::new(HashMap::new()),
            rng: AtomicU64::new(seed | 1),
            stats: Mutex::new(Stats::default()),
        })
    }

    fn k(&self) -> &str {
        // The key message comes before any request; without one, nothing is
        // known.
        self.key.get_or_init(|| UNKNOWN_KEY.into())
    }

    /// The key of the extraction code the check rules used.
    pub fn key(&self) -> String {
        self.k().to_string()
    }

    /// The fatal refusal of every request once the run is halted.
    fn halted_refusal(&self) -> Option<Outcome> {
        self.halted.get().map(|(reason, message)| Outcome::Refused { reason, message: message.clone(), fatal: true })
    }

    /// Check the root npm reports against the trial scope (the shell
    /// function's runs).
    pub fn set_trial_scope(&self, on: bool) {
        self.trial_scope.store(on, Ordering::Relaxed);
    }

    /// Whether `p` (absolute) is the pinned root, symlinks resolved.
    fn is_root(&self, p: &Path) -> bool {
        let Some(root) = self.root.get() else { return false };
        p == root || (p.is_absolute() && std::fs::canonicalize(p).is_ok_and(|c| &c == root))
    }

    /// The gates on the install root npm reported (the `root` message), and
    /// the root, canonical, when they pass. Everything that depends on where
    /// npm installs is decided here, on npm's own values (its abbreviations,
    /// its environment and npmrc files, the workspace walk included); the
    /// command has not run yet, so a refusal changes nothing.
    fn accept_root(&self, v: &Value) -> std::result::Result<PathBuf, String> {
        let flag = |k: &str| v.get(k).and_then(|x| x.as_bool()) == Some(true);
        if flag("global") {
            return Err("npm installs globally".into());
        }
        let command = v.get("command").and_then(|c| c.as_str());
        if !command.is_some_and(|c| ROOT_COMMANDS.contains(&c)) {
            let typed = v.get("typed").and_then(|c| c.as_str()).unwrap_or("?");
            return Err(match command {
                Some(c) => format!("npm runs `{c}` for `{typed}`, not one of {}", ROOT_COMMANDS.join(", ")),
                None => format!("npm has no command `{typed}`"),
            });
        }
        for (k, name) in [("dry_run", "--dry-run"), ("package_lock_only", "--package-lock-only"), ("usage", "--usage")] {
            if flag(k) {
                return Err(format!("this run installs nothing ({name})"));
            }
        }
        let Some(reported) = v.get("root").and_then(|r| r.as_str()).map(Path::new).filter(|p| p.is_absolute()) else {
            return Err("npm reported no absolute install root".into());
        };
        let root = match std::fs::canonicalize(reported) {
            Ok(r) if r.is_dir() => r,
            _ => return Err(format!("the install root {} does not exist yet", reported.display())),
        };
        if self.trial_scope.load(Ordering::Relaxed) {
            crate::trial::in_scope(&root)?;
        }
        // npm ci replaces a node_modules that is a symlink; every clone into
        // it would be refused one by one (its real path is outside the root).
        if std::fs::symlink_metadata(root.join("node_modules")).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("the install root's node_modules is a symlink ({}); npm ci replaces it, matchapack does not", root.display()));
        }
        Ok(root)
    }

    /// Whether to compare an entry, known to `made_by`, with pacote's
    /// extraction (module comment): once per key, then one in N.
    fn should_check(&self, made_by: &[String]) -> bool {
        let CheckPolicy::OneIn(n) = self.policy else { return self.policy == CheckPolicy::All };
        let key = self.k();
        if !known_key(key) || !made_by.iter().any(|k| k == key) {
            return true;
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
        let Some(root) = self.root.get() else {
            return Err("npm has reported no install root".into());
        };
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
        let Ok(rel) = real.strip_prefix(root) else {
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
        // What is moved in keeps the group of the store's tmp/, and is given
        // the destination's afterwards (#41, `finish`): not possible for a
        // group this user is not in, and then npm extracts the package.
        match npm_gid(dst) {
            Ok(w) if clone::can_give_group(w.gid) => {}
            Ok(w) => return Err(refused("group", format!("{} needs group {}, which this user cannot give a file", dst.display(), w.gid))),
            Err(e) => return Err(refused("clone-error", format!("{e:#}"))),
        }
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
    #[cfg(test)]
    pub fn place_entry(&self, sri: &str, dest: &Path) -> Outcome {
        self.place_or_wait(&Value::Null, sri, dest).pop().map(|(_, o)| o).unwrap_or_else(|| refused("bad-request", "deferred"))
    }

    /// A request without `extracted`: place the entry, or hand out a
    /// directory for pacote, or (the entry being filled right now by
    /// another request of this install) answer later, when that fill is
    /// done: an empty list.
    fn place_or_wait(&self, id: &Value, sri: &str, dest: &Path) -> Vec<(Value, Outcome)> {
        let one = |o: Outcome| vec![(id.clone(), o)];
        if let Some(o) = self.halted_refusal() {
            return one(o);
        }
        if self.unsupported.load(Ordering::Relaxed) {
            return one(Outcome::Refused { reason: "clone-unsupported", message: "this filesystem cannot clone".into(), fatal: true });
        }
        let dst = match self.check_dest(dest) {
            Ok(d) => d,
            Err(m) => return one(refused("rejected-dest", m)),
        };
        let i = match Integrity::parse(sri) {
            Ok(i) => i,
            Err(e) => return one(refused("rejected-source", format!("{e:#}"))),
        };
        let dir = self.store.dir_for(&i);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {
                // Shared: no replacement of the entry while its record is
                // read and it is cloned, so the tree cloned is the one the
                // record speaks for.
                let shared = match self.store.entry_lock(&i, false) {
                    Ok(l) => l,
                    Err(e) => return one(refused("store-error", format!("{e:#}"))),
                };
                if self.should_check(&self.store.made_by(&i)) {
                    drop(shared);
                    return one(self.issue(i, "check"));
                }
                if !is_real_dir(&dir) {
                    // Replaced away before the lock: decided again by npm.
                    return one(refused("store-miss", "the entry went while it was looked at"));
                }
                let fresh = self.ours(&i);
                one(self.finish(i, &dir, &dst, false, false, None, fresh))
            }
            Ok(_) => one(refused("rejected-source", format!("store entry {} is not a directory", dir.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                {
                    let mut f = self.filling.lock().unwrap();
                    if let Some(waiting) = f.get_mut(&i) {
                        waiting.push((id.clone(), dst));
                        return vec![];
                    }
                    f.insert(i.clone(), vec![]);
                }
                let o = self.issue(i.clone(), "fill");
                if matches!(o, Outcome::Extract { .. }) {
                    return one(o);
                }
                let mut v = one(o);
                v.extend(self.release(&i));
                v
            }
            Err(e) => one(refused("store-error", format!("{}: {e}", dir.display()))),
        }
    }

    /// The fill of `i` is over (made, failed, or given up): answer the
    /// requests that waited for it, from the entry if this server made it
    /// (pacote's extraction in this install). An entry another install
    /// published first is not given to them unchecked: they go to pacote.
    fn release(&self, i: &Integrity) -> Vec<(Value, Outcome)> {
        let waiting = self.filling.lock().unwrap().remove(i).unwrap_or_default();
        let entry = self.store.dir_for(i);
        waiting
            .into_iter()
            .map(|(id, dst)| {
                // One waiter's failure (even a panic) must not leave the
                // others unanswered.
                let o = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Shared, and the entry checked to be ours under it.
                    let shared = self.store.entry_lock(i, false);
                    if shared.is_ok() && self.ours(i) {
                        self.finish(i.clone(), &entry, &dst, false, false, None, true)
                    } else {
                        refused("store-miss", "the fill this package waited for did not complete here")
                    }
                }))
                .unwrap_or_else(|_| refused("store-miss", "internal error (panic) while placing it"));
                (id, o)
            })
            .collect()
    }

    /// The entry for `i` is still the directory this install published.
    fn ours(&self, i: &Integrity) -> bool {
        let Some(&published) = self.filled_now.lock().unwrap().get(i) else { return false };
        std::fs::symlink_metadata(self.store.dir_for(i)).is_ok_and(|m| m.is_dir() && std::os::unix::fs::MetadataExt::ino(&m) == published)
    }

    /// The hook has run pacote's extract into `extracted` (handed out by
    /// `issue` for this integrity): fill the store with it, or compare the
    /// entry with it, then place.
    #[cfg(test)]
    pub fn place_extracted(&self, sri: &str, dest: &Path, extracted: &Path) -> Outcome {
        self.extracted(&Value::Null, sri, dest, extracted, false).swap_remove(0).1
    }

    /// `place_extracted`, or (`failed`) pacote's extraction into the
    /// directory failed; then the requests waiting for this fill.
    fn extracted(&self, id: &Value, sri: &str, dest: &Path, extracted: &Path, failed: bool) -> Vec<(Value, Outcome)> {
        let o = self.place_extracted_inner(sri, dest, extracted, failed);
        let mut v = vec![(id.clone(), o)];
        if let Ok(i) = Integrity::parse(sri) {
            v.extend(self.release(&i));
        }
        v
    }

    fn place_extracted_inner(&self, sri: &str, dest: &Path, extracted: &Path, failed: bool) -> Outcome {
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
        if failed {
            discard();
            return refused("pacote-extract-failed", "pacote could not extract the package into matchapack's directory");
        }
        if let Some(o) = self.halted_refusal() {
            discard();
            return o;
        }
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
    /// What reaches the project is either pacote's extraction itself, or a
    /// clone of the entry that was compared with it, never the entry's path
    /// read again after the comparison: another install, under another
    /// pacote, may replace the entry at any moment.
    fn adopt(&self, i: Integrity, dst: &Path, extracted: &Path) -> Outcome {
        let discard = || {
            let _ = std::fs::remove_dir_all(extracted);
        };
        let entry = self.store.dir_for(&i);
        let ino = |p: &Path| std::fs::symlink_metadata(p).ok().filter(|m| m.is_dir()).map(|m| std::os::unix::fs::MetadataExt::ino(&m));
        if let Some(ino_before) = ino(&entry) {
            // A check, or a fill that another install made first. The entry
            // is cloned into a private candidate first; the candidate is what
            // is compared and then moved into the project: it shares blocks
            // with the store, and a replacement of the entry during the clone
            // shows up as a difference, never in the project.
            let cand = self.tmp.path.join(format!("c{}", self.seq.fetch_add(1, Ordering::Relaxed)));
            let drop_cand = || {
                let _ = std::fs::remove_dir_all(&cand);
            };
            // Shared while the candidate is cloned: one tree, with its record.
            let shared = match self.store.entry_lock(&i, false) {
                Ok(l) => l,
                Err(e) => {
                    discard();
                    return refused("store-error", format!("{e:#}"));
                }
            };
            let made_by = self.store.made_by(&i);
            let cloned = clone::clone_tree(&entry, &cand, self.mode);
            drop(shared);
            if let Err(e) = cloned {
                drop_cand();
                discard();
                if clone::unsupported(&e) {
                    self.unsupported.store(true, Ordering::Relaxed);
                    return Outcome::Refused { reason: "clone-unsupported", message: format!("{e:#}"), fatal: true };
                }
                return refused("clone-error", format!("{e:#}"));
            }
            if !made_by.iter().any(|k| k == self.k()) {
                self.stats.lock().unwrap().new_key_checks += 1;
            }
            let differs = match crate::store::compare_extraction(extracted, &cand, self.started) {
                Ok(crate::store::Compared::Same) => None,
                Ok(crate::store::Compared::Differs(d)) => Some(d),
                Ok(crate::store::Compared::Unexpected(odd)) => {
                    drop_cand();
                    discard();
                    return self.halt_on_extraction(&i, odd);
                }
                Err(e) => {
                    drop_cand();
                    discard();
                    return refused("store-error", format!("comparing with {}: {e:#}", entry.display()));
                }
            };
            let Some(d) = differs else {
                // The record says this pacote extracts the entry to the same
                // tree. Exclusive: written only for the tree that was
                // compared (same inode, same record as when it was cloned),
                // and no replacement can come in between.
                if known_key(self.k()) && !made_by.iter().any(|k| k == self.k()) {
                    if let Ok(_excl) = self.store.entry_lock(&i, true) {
                        if ino(&entry) == Some(ino_before) && self.store.made_by(&i) == made_by {
                            if let Err(e) = self.store.add_made_by(&i, self.k()) {
                                crate::info!("could not record which pacote compared {}: {e:#}", i.key());
                            }
                        }
                    }
                }
                // The candidate was cloned into the store's tmp/ and has its
                // group; npm's extraction gives the destination's (#41). A
                // group this user cannot give (a project under a directory
                // of a group the user is not in, such as /tmp's wheel on
                // macOS): pacote's extraction is cloned at the destination
                // instead, where the kernel gives it that group.
                let g = match npm_gid(dst) {
                    Ok(g) => g,
                    Err(e) => {
                        drop_cand();
                        discard();
                        return refused("clone-error", format!("{e:#}"));
                    }
                };
                if !clone::can_give_group(g.gid) {
                    drop_cand();
                    let o = self.finish(i.clone(), extracted, dst, false, true, None, true);
                    discard();
                    return o;
                }
                let t0 = Instant::now();
                let kind = match self.place_move(&cand, dst) {
                    Ok(k) => k,
                    Err(o) => {
                        drop_cand();
                        discard();
                        return o;
                    }
                };
                let t1 = Instant::now();
                // The candidate carries the entry's times, and the group of
                // the store's tmp/ it was cloned into; pacote's extraction
                // has the same paths (just compared).
                let settled = match clone::settle_placed(extracted, dst, true, Some(g)) {
                    Ok(s) => s,
                    Err(e) => {
                        discard();
                        return refused("clone-error", format!("{e:#}"));
                    }
                };
                discard();
                return Outcome::Placed {
                    kind,
                    filled: false,
                    checked: true,
                    differs: None,
                    entry: i,
                    place_secs: t1.duration_since(t0).as_secs_f64(),
                    touch_secs: t1.elapsed().as_secs_f64(),
                    paths: settled.paths,
                    regrouped: settled.regrouped,
                };
            };
            drop_cand();
            let kind = differs_kind(&made_by, self.k());
            let o = self.finish(i.clone(), extracted, dst, false, true, Some((kind, d)), true);
            // Then the entry is replaced by pacote's extraction (module
            // comment). The record goes first: whatever happens next, an
            // entry left here is never taken for this pacote's without a
            // new record.
            // Exclusive: nobody clones the entry or writes its record while
            // it is replaced.
            let excl = self.store.entry_lock(&i, true);
            let aside = self.tmp.path.join(format!("q{}", self.seq.fetch_add(1, Ordering::Relaxed)));
            let healed = excl.is_ok() && {
                self.store.forget_made_by(&i);
                self.store.forget_size(&i);
                std::fs::rename(&entry, &aside).is_ok() && crate::store::rename_noreplace(extracted, &entry).is_ok()
            };
            if healed {
                self.mark_made_by(&i);
            }
            drop(excl);
            if !healed {
                // Someone else moved the entry meanwhile.
                discard();
            }
            return o;
        }
        // pacote's extraction was made just now: placed without a touch.
        let mut o = self.finish(i.clone(), extracted, dst, false, false, None, true);
        let parent = entry.parent().unwrap();
        if let Err(e) = std::fs::create_dir_all(parent) {
            discard();
            crate::info!("inject: could not create {}: {e}", parent.display());
            return o;
        }
        // Rename-if-absent (a no-replace rename: a plain rename would replace
        // an empty directory), so concurrent installs fill an entry once.
        // The inode is taken before the rename (a rename keeps it): whatever
        // is at the entry's path later is ours only if it is this directory.
        let published = std::fs::symlink_metadata(extracted).map(|m| std::os::unix::fs::MetadataExt::ino(&m)).ok();
        // Exclusive: the entry and its record appear together.
        let excl = self.store.entry_lock(&i, true);
        let renamed = match &excl {
            Ok(_) => crate::store::rename_noreplace(extracted, &entry),
            // Not published without the lock (the package is placed already).
            Err(e) => Err(std::io::Error::other(format!("{e:#}"))),
        };
        if renamed.is_ok() {
            self.mark_made_by(&i);
        }
        drop(excl);
        match renamed {
            Ok(()) => {
                self.store.forget_size(&i);
                if let Some(ino) = published {
                    self.filled_now.lock().unwrap().insert(i.clone(), ino);
                }
                // Counted here: the entry is in the store now, whatever
                // became of the placement.
                self.stats.lock().unwrap().filled.push(i);
                if let Outcome::Placed { filled, .. } = &mut o {
                    *filled = true;
                }
            }
            // Another install filled it while pacote extracted here: theirs
            // is compared before this version uses it, if this version is not
            // in its record.
            Err(_) => discard(),
        }
        o
    }

    /// Move `cand` (a private clone of the entry) into `dst`, as `place`
    /// clones: renamed in whole when the destination is absent or empty,
    /// its entries moved in when the destination holds `node_modules`.
    fn place_move(&self, cand: &Path, dst: &Path) -> std::result::Result<Kind, Outcome> {
        let err = |e: String| refused("clone-error", e);
        match dest_state(dst).map_err(|e| err(format!("reading {}: {e}", dst.display())))? {
            DestState::Missing => {}
            DestState::Empty => {
                let _ = std::fs::remove_dir(dst);
            }
            DestState::OnlyNodeModules => {
                if !is_real_dir(dst) {
                    return Err(refused("rejected-dest", format!("{} is no longer a directory", dst.display())));
                }
                merge_into(cand, dst).map_err(|e| err(format!("{e:#}")))?;
                return Ok(Kind::Merge);
            }
            DestState::Other(name) => return Err(refused("dest-not-empty", format!("{} already holds {name}", dst.display()))),
        }
        match crate::store::rename_noreplace(cand, dst) {
            Ok(()) => Ok(Kind::Fast),
            // Made again in between (npm's sparse tree): merge.
            Err(_) if is_real_dir(dst) => match dest_state(dst) {
                Ok(DestState::Empty | DestState::OnlyNodeModules) => {
                    merge_into(cand, dst).map_err(|e| err(format!("{e:#}")))?;
                    Ok(Kind::Merge)
                }
                _ => Err(refused("dest-not-empty", format!("{} is not empty", dst.display()))),
            },
            Err(e) => Err(err(format!("moving into {}: {e}", dst.display()))),
        }
    }

    /// pacote's extraction of `i` is not what a placed clone reproduces
    /// (`store::compare_extraction`): this run is halted (the hook leaves
    /// this and every later package to npm), it is reported as broken
    /// (inject.rs), and a known key goes on the store's decline list.
    fn halt_on_extraction(&self, i: &Integrity, odd: String) -> Outcome {
        let message = format!("{}: {odd}", i.key());
        {
            let mut s = self.stats.lock().unwrap();
            s.extraction_unexpected.get_or_insert_with(|| message.clone());
        }
        if known_key(self.k()) {
            let why = format!("pacote's extraction is not what matchapack's clones reproduce ({message})");
            if let Err(e) = self.store.decline_key(self.k(), &why) {
                crate::info!("could not put the key on the store's decline list: {e:#}");
            }
        }
        let _ = self.halted.set(("extraction-unexpected", message));
        self.halted_refusal().unwrap()
    }

    fn mark_made_by(&self, i: &Integrity) {
        if !known_key(self.k()) {
            self.store.forget_made_by(i);
            return;
        }
        if let Err(e) = self.store.set_made_by(i, self.k()) {
            crate::info!("could not record which pacote made {}: {e:#}", i.key());
        }
    }

    /// Clone `src` (the entry, or pacote's extraction) to `dst`, and touch
    /// it unless `fresh` (`src` was extracted in this install, so its times
    /// are already the install's).
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        entry: Integrity,
        src: &Path,
        dst: &Path,
        filled: bool,
        checked: bool,
        differs: Option<(&'static str, String)>,
        fresh: bool,
    ) -> Outcome {
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
        // In the same walk, each path gets the group npm's extraction would
        // give it: a merge placement moved its paths in from the store's
        // tmp/, whose group they kept (#41). A fresh extraction needs no
        // touch; it is walked for the group only after a merge, or when the
        // placed directory shows another group (a clone made at the
        // destination gets the destination's group from the kernel).
        let settled = match npm_gid(dst).and_then(|g| {
            if !fresh {
                clone::settle_placed(src, dst, true, Some(g))
            } else if kind == Kind::Merge || !has_gid(dst, g.gid) {
                clone::settle_placed(src, dst, false, Some(g)).map(|s| clone::Settled { paths: 0, ..s })
            } else {
                Ok(clone::Settled::default())
            }
        }) {
            Ok(s) => s,
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
            paths: settled.paths,
            regrouped: settled.regrouped,
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
            Outcome::Placed { kind, entry, place_secs, touch_secs, paths, regrouped, checked, differs, .. } => {
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
                s.paths_regrouped += regrouped;
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

    /// Record a message that needs no answer (`hello`, `note`, `hook`,
    /// `no_root`). Returns false when `v` is something else.
    fn record_message(&self, v: &Value) -> bool {
        // Short words and messages chosen by the helper; keep the log sane
        // whatever arrives.
        let text = |x: &Value| x.as_str().map(|s| s.chars().take(300).collect::<String>());
        if v.get("hello").is_some() {
            self.stats.lock().unwrap().hello = Some(v.clone());
            return true;
        }
        // The key comes when npm has loaded pacote, before any request.
        if v.get("key").is_some() && v.get("id").is_none() {
            let key = code_key(v);
            let _ = self.key.set(key.clone());
            let declined = if known_key(&key) { self.store.declined(&key) } else { None };
            let mut s = self.stats.lock().unwrap();
            s.tar_fingerprint = v
                .get("tar_fingerprint")
                .and_then(|x| x.as_str())
                .filter(|f| f.len() == 64 && f.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(String::from);
            s.key_error = v.get("key_error").and_then(text);
            if let Some(why) = declined {
                let _ = self.halted.set(("key-declined", format!("this extraction code ({key}) is on the store's decline list: {why}")));
                s.key_declined = Some(why);
            }
            return true;
        }
        if let Some(state) = v.get("hook").and_then(text) {
            let mut s = self.stats.lock().unwrap();
            s.hook = Some(state.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(20).collect());
            s.hook_reason = v.get("reason").and_then(text);
            return true;
        }
        if let Some(why) = v.get("no_root").and_then(text) {
            self.stats.lock().unwrap().no_root = Some(why);
            return true;
        }
        if let Some(note) = v.get("note").and_then(|n| n.as_str()) {
            // A reason is a short word chosen by the hook; keep the log sane
            // whatever arrives.
            let key: String = note.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
            *self.stats.lock().unwrap().fallbacks.entry(key).or_default() += 1;
            return true;
        }
        if let Some(key) = v.get("result_read").and_then(|k| k.as_str()) {
            // A property name npm used: printable, bounded.
            let key: String = key.chars().filter(|c| c.is_ascii_graphic()).take(60).collect();
            let mut s = self.stats.lock().unwrap();
            let map = if v.get("expected").and_then(|e| e.as_bool()) == Some(true) { &mut s.result_reads } else { &mut s.result_unexpected };
            *map.entry(key).or_default() += 1;
            return true;
        }
        false
    }

    /// One line from the hook; the answer lines it produces: its own (none
    /// when it waits for a fill), and those of requests that waited for it.
    fn handle_line(&self, line: &str) -> Vec<String> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![];
        };
        if self.record_message(&v) {
            return vec![];
        }
        let Some(id) = v.get("id").cloned() else { return vec![] };
        // Answered, so that the helper knows the reason is recorded before
        // it exits (a line still unread when the child is gone can be lost).
        if let Some(why) = v.get("declined").and_then(|d| d.as_str()) {
            self.stats.lock().unwrap().declined = Some(why.chars().take(300).collect());
            return vec![json!({ "id": id, "ok": true }).to_string()];
        }
        // npm's install root, once, before its command runs.
        if v.get("root").is_some() && v.get("integrity").is_none() {
            let command = v.get("command").and_then(|c| c.as_str()).map(|c| c.chars().take(40).collect::<String>());
            let verdict = match self.root.get() {
                Some(_) => Err("npm reported its install root twice".to_string()),
                None => self.accept_root(&v),
            };
            let mut s = self.stats.lock().unwrap();
            s.command = command;
            return vec![match verdict {
                Ok(root) => {
                    let _ = self.root.set(root.clone());
                    s.root = Some(root.clone());
                    json!({ "id": id, "ok": true, "root": root })
                }
                Err(why) => {
                    s.declined = Some(why.chars().take(300).collect());
                    json!({ "id": id, "ok": false, "reason": why })
                }
            }
            .to_string()];
        }
        {
            let mut s = self.stats.lock().unwrap();
            s.first.get_or_insert_with(Instant::now);
        }
        let str_of = |k: &str| v.get(k).and_then(|x| x.as_str());
        let answers: Vec<(Value, Outcome)> = match (str_of("integrity"), str_of("dest")) {
            // No root reported yet (npm's could not be read): nothing is placed.
            (Some(_), Some(_)) if self.root.get().is_none() => {
                vec![(id.clone(), refused("no-root", "npm has reported no install root"))]
            }
            // The root arborist passed to pacote is not the one npm reported
            // when it started the command: two values inside npm disagree, so
            // nothing is placed. The destination check itself uses the pinned
            // root only.
            (Some(_), Some(_)) if str_of("npm_root").is_some_and(|r| !self.is_root(Path::new(r))) => vec![(
                id.clone(),
                refused(
                    "npm-root-elsewhere",
                    format!("npm installs into {}, not {}", str_of("npm_root").unwrap_or(""), self.root.get().map(|r| r.display().to_string()).unwrap_or_default()),
                ),
            )],
            (Some(sri), Some(dest)) => {
                // A panic in one placement must not take the answer with it:
                // the hook would wait for ever.
                let failed = v.get("failed").and_then(|f| f.as_bool()) == Some(true);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match str_of("extracted") {
                    Some(x) => self.extracted(&id, sri, Path::new(dest), Path::new(x), failed),
                    None => self.place_or_wait(&id, sri, Path::new(dest)),
                }))
                .unwrap_or_else(|_| {
                    // The requests waiting for this fill must not wait for ever.
                    let mut v = vec![(id.clone(), refused("clone-error", "internal error (panic)"))];
                    // (Also when this request was only waiting: the waiters then
                    // go to pacote early, which is always safe.)
                    if let Ok(i) = Integrity::parse(sri) {
                        v.extend(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.release(&i))).unwrap_or_default());
                    }
                    v
                })
            }
            _ => vec![(id.clone(), refused("bad-request", "integrity and dest are required"))],
        };
        answers
            .into_iter()
            .map(|(id, outcome)| {
                self.tally(&outcome);
                match outcome {
                    Outcome::Placed { kind, filled, checked, .. } => {
                        json!({ "id": id, "ok": true, "kind": self.kind_name(kind), "filled": filled, "checked": checked })
                    }
                    Outcome::Extract { to, why } => json!({ "id": id, "ok": false, "extract_to": to, "why": why }),
                    Outcome::Refused { reason, message, fatal } => {
                        json!({ "id": id, "ok": false, "reason": reason, "message": message, "fatal": fatal })
                    }
                }
                .to_string()
            })
            .collect()
    }

    /// Tell the workers to drop what is still queued (the node child is gone).
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// Read requests from `stream` until it ends, serving them on `threads`
    /// worker threads. Returns when the stream is at its end (the peer closed
    /// it, or someone shut it down) and every started request is answered.
    pub fn serve(&self, stream: UnixStream, threads: usize) -> Result<()> {
        serve_lines(stream, threads, &self.stopping, &|v| self.record_message(v), &|l| self.handle_line(l))
    }
}

/// The transport of a hook's JSON-lines conversation (this module's protocol,
/// and the Yarn 1 engine's, src/yarn1.rs): read lines from `stream` until it
/// ends; a line `record` accepts (a message that needs no answer) is handled
/// at once on the reading thread, every other line by one of `threads`
/// workers, which write the answer lines `handle` returns. Once `stopping` is
/// set (the node child is gone), queued lines are dropped. Returns when the
/// stream is at its end and every started line is handled.
pub fn serve_lines(
    stream: UnixStream,
    threads: usize,
    stopping: &AtomicBool,
    record: &(dyn Fn(&Value) -> bool + Sync),
    handle: &(dyn Fn(&str) -> Vec<String> + Sync),
) -> Result<()> {
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
                if stopping.load(Ordering::Relaxed) {
                    continue;
                }
                for mut answer in handle(&line) {
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
                    if serde_json::from_str::<Value>(&l).is_ok_and(|v| record(&v)) {
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

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
            Server::new(&self.store, Some(self.root.clone()), mode, Some(KEY.into()), policy).unwrap()
        }

        /// What the hook does with an `extract_to` answer, with matchapack's own
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

    /// A group the user is in other than `not`, if any.
    fn other_group(not: u32) -> Option<u32> {
        let groups = unsafe {
            let mut g = vec![0 as libc::gid_t; 64];
            let n = libc::getgroups(g.len() as i32, g.as_mut_ptr());
            g.truncate(n.max(0) as usize);
            g
        };
        groups.into_iter().map(|g| g as u32).find(|g| *g != not)
    }

    /// #41. npm's extraction gives every path the group the kernel gives a
    /// new file in the destination (clone::new_path_gid); a placement moved
    /// in from the store's tmp/ (a merge, a checked candidate, a merged
    /// fill) kept the store's group. Each kind of placement, with the store
    /// under another group than the project.
    #[test]
    fn placed_paths_get_the_group_npms_extraction_gives_them() {
        let fx = Fx::new("serve-gid");
        if !fx.can_clone() {
            return;
        }
        let nm = fx.root.join("node_modules");
        let want = clone::new_paths(&nm).unwrap().gid;
        let Some(other) = other_group(want) else {
            eprintln!("skipped: the user is in one group only");
            return;
        };
        let (i, sri) = fx.entry(&[("package.json", 0o644, "{}\n"), ("lib/deep/x.js", 0o644, "x\n"), ("node_modules/inner/i.js", 0o644, "i\n")]);
        let regroup = |p: &Path| {
            for e in walkdir::WalkDir::new(p) {
                std::os::unix::fs::lchown(e.unwrap().path(), None, Some(other)).unwrap();
            }
        };
        // The whole store (entries and tmp/, where the servers below make
        // their directories) under the other group.
        regroup(&fx.store.root);
        // On Linux a new path takes the creating process's group unless its
        // directory is setgid: setgid makes tmp/ hand on its group there too.
        let tmp = fx.store.root.join("tmp");
        let m = fs::metadata(&tmp).unwrap().permissions().mode();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(m | libc::S_ISGID as u32)).unwrap();
        let groups = |d: &Path| -> Vec<String> {
            walkdir::WalkDir::new(d)
                .sort_by_file_name()
                .into_iter()
                .map(|e| e.unwrap())
                .filter(|e| fs::symlink_metadata(e.path()).unwrap().gid() != want)
                .map(|e| e.path().strip_prefix(d).unwrap().display().to_string())
                .collect()
        };
        for mode in modes() {
            let s = fx.server(mode);
            // The entry placed by a clone: at the destination (fast), and
            // into tmp/ then moved in (merge). Another package's file in the
            // destination's node_modules is left as it is.
            let fast = nm.join(format!("fast-{mode:?}"));
            assert_eq!(placed(s.place_entry(&sri, &fast)).0, Kind::Fast);
            assert_eq!(groups(&fast), Vec::<String>::new(), "{mode:?} fast");
            let merge = nm.join(format!("merge-{mode:?}"));
            let foreign = merge.join("node_modules/other/o.js");
            fs::create_dir_all(foreign.parent().unwrap()).unwrap();
            fs::write(&foreign, "o\n").unwrap();
            std::os::unix::fs::lchown(&foreign, None, Some(other)).unwrap();
            let Outcome::Placed { kind: Kind::Merge, regrouped, .. } = s.place_entry(&sri, &merge) else { panic!("{mode:?}: not a merge") };
            assert_eq!(groups(&merge), ["node_modules/other/o.js"], "{mode:?} merge: only the other package's file keeps its group");
            assert_eq!(regrouped, 6, "{mode:?}: the merge regrouped its six paths (not the destination, npm's)");
            // One server per process at a time (its tmp/ directory's name).
            drop(s);
            // A check that matched: the candidate cloned into tmp/, moved in.
            let sc = fx.server_with(mode, CheckPolicy::All);
            let checked = nm.join(format!("checked-{mode:?}"));
            let to = extract_to(sc.place_entry(&sri, &checked), "check");
            fx.pacote_extract(&i, &to);
            regroup(&to);
            assert_eq!(placed_full(sc.place_extracted(&sri, &checked, &to)), (Kind::Fast, false, true, None));
            assert_eq!(groups(&checked), Vec::<String>::new(), "{mode:?} checked");
            drop(sc);
            let s = fx.server(mode);
            // A fill (pacote's extraction in tmp/, no touch), placed by merge.
            let j = cached_tarball(&fx.cacache, &[("package.json", 0o644, &format!("{{\"m\":\"{mode:?}\"}}\n")), ("a/b.js", 0o644, "b\n")]);
            let filled = nm.join(format!("filled-{mode:?}"));
            fs::create_dir_all(filled.join("node_modules")).unwrap();
            let to = extract_to(s.place_entry(&sri_of(&j), &filled), "fill");
            fx.pacote_extract(&j, &to);
            regroup(&to);
            assert_eq!(placed_full(s.place_extracted(&sri_of(&j), &filled, &to)), (Kind::Merge, true, false, None));
            assert_eq!(groups(&filled), Vec::<String>::new(), "{mode:?} filled by merge");
        }
        // The store keeps its own group.
        assert_eq!(fs::metadata(fx.store.dir_for(&i).join("lib/deep/x.js")).unwrap().gid(), other);
    }

    /// Linux: in a setgid directory npm's extraction makes setgid
    /// directories (the kernel hands the bit on); a directory moved in from
    /// the store's tmp/ lacks it until it is given it. macOS hands no bit on.
    #[test]
    fn directories_get_the_setgid_bit_a_setgid_parent_hands_on() {
        let fx = Fx::new("serve-setgid");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(&[("package.json", 0o644, "{}\n"), ("lib/deep/x.js", 0o644, "x\n")]);
        let scope = fx.root.join("node_modules/@sg");
        fs::create_dir_all(&scope).unwrap();
        let m = fs::metadata(&scope).unwrap().permissions().mode();
        fs::set_permissions(&scope, fs::Permissions::from_mode(m | libc::S_ISGID as u32)).unwrap();
        let want = cfg!(target_os = "linux");
        let bits = |d: &Path| -> Vec<(String, bool)> {
            walkdir::WalkDir::new(d)
                .sort_by_file_name()
                .into_iter()
                .map(|e| e.unwrap())
                .filter(|e| e.file_type().is_dir())
                .map(|e| (e.path().strip_prefix(d).unwrap().display().to_string(), fs::metadata(e.path()).unwrap().mode() & libc::S_ISGID as u32 != 0))
                .collect()
        };
        for mode in modes() {
            let s = fx.server(mode);
            let fast = scope.join(format!("fast-{mode:?}"));
            assert_eq!(placed(s.place_entry(&sri, &fast)).0, Kind::Fast);
            let merge = scope.join(format!("merge-{mode:?}"));
            fs::create_dir_all(merge.join("node_modules")).unwrap();
            assert_eq!(placed(s.place_entry(&sri, &merge)).0, Kind::Merge);
            for d in [&fast, &merge] {
                let wrong: Vec<_> = bits(d).into_iter().filter(|(_, b)| *b != want).collect();
                assert!(wrong.is_empty(), "{mode:?} {}: setgid bit not {want}: {wrong:?}", d.display());
            }
            drop(s);
        }
        let _ = i;
    }

    /// The other way round: the store's tmp/ is setgid (a store under a
    /// shared group, #41), the project's directory is not. On Linux the
    /// directories made in tmp/ get the bit there; npm's extraction in the
    /// project gives none, so a directory moved in must lose it.
    #[test]
    fn directories_lose_the_setgid_bit_a_setgid_store_hands_on() {
        let fx = Fx::new("serve-setgid-store");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(&[("package.json", 0o644, "{}\n"), ("lib/deep/x.js", 0o644, "x\n")]);
        let tmp = fx.store.root.join("tmp");
        let m = fs::metadata(&tmp).unwrap().permissions().mode();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(m | libc::S_ISGID as u32)).unwrap();
        let nm = fx.root.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        let setgid = |d: &Path| -> Vec<String> {
            walkdir::WalkDir::new(d)
                .sort_by_file_name()
                .into_iter()
                .map(|e| e.unwrap())
                .filter(|e| e.file_type().is_dir() && fs::metadata(e.path()).unwrap().mode() & libc::S_ISGID as u32 != 0)
                .map(|e| e.path().strip_prefix(d).unwrap().display().to_string())
                .collect()
        };
        for mode in modes() {
            let s = fx.server(mode);
            let fast = nm.join(format!("fast-{mode:?}"));
            assert_eq!(placed(s.place_entry(&sri, &fast)).0, Kind::Fast);
            let merge = nm.join(format!("merge-{mode:?}"));
            fs::create_dir_all(merge.join("node_modules")).unwrap();
            assert_eq!(placed(s.place_entry(&sri, &merge)).0, Kind::Merge);
            for d in [&fast, &merge] {
                assert_eq!(setgid(d), Vec::<String>::new(), "{mode:?} {}: setgid directories", d.display());
            }
            drop(s);
        }
        let _ = i;
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
        assert_eq!(fx.store.made_by(&i), vec![KEY.to_string()], "and is recorded as made by this pacote");
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
    fn a_clone_waits_while_the_entry_is_being_replaced() {
        // The exclusive entry lock is what a replacement (and a fill, and a
        // record write) holds: a clone of the entry starts only after it.
        let fx = Fx::new("serve-entry-lock");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        let s = fx.server(clone::Mode::File);
        let dst = fx.root.join("node_modules/p");
        let excl = fx.store.entry_lock(&i, true).unwrap();
        std::thread::scope(|sc| {
            let t = sc.spawn(|| placed(s.place_entry(&sri, &dst)));
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!dst.join("package.json").exists(), "the clone waited for the lock");
            drop(excl);
            assert_eq!(t.join().unwrap().0, Kind::Fast);
        });
        assert!(dst.join("package.json").exists());
    }

    #[test]
    fn whether_to_check_is_decided_under_the_entry_lock() {
        // A replacement in progress (exclusive) changes the record; a request
        // waiting for the lock decides on the record it finds after it, not
        // on the one before.
        let fx = Fx::new("serve-decide-under-lock");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        fx.store.set_made_by(&i, KEY).unwrap();
        let s = fx.server_with(clone::Mode::File, CheckPolicy::OneIn(1_000_000));
        let dst = fx.root.join("node_modules/p");
        let excl = fx.store.entry_lock(&i, true).unwrap();
        std::thread::scope(|sc| {
            let t = sc.spawn(|| s.place_entry(&sri, &dst));
            std::thread::sleep(std::time::Duration::from_millis(300));
            fx.store.set_made_by(&i, OLD).unwrap();
            drop(excl);
            assert!(matches!(t.join().unwrap(), Outcome::Extract { why: "check", .. }), "decided after the lock");
        });
    }

    #[test]
    fn a_fill_replaces_a_record_left_by_an_earlier_entry() {
        // The entry went (removed by hand, or an interrupted replacement) but
        // its record stayed: the tree pacote makes now is this version's only.
        let fx = Fx::new("serve-fill-stale-record");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        fx.store.set_made_by(&i, OLD).unwrap();
        let s = fx.server(clone::Mode::File);
        let dst = fx.root.join("node_modules/p");
        let to = extract_to(s.place_entry(&sri_of(&i), &dst), "fill");
        fx.pacote_extract(&i, &to);
        placed_full(s.place_extracted(&sri_of(&i), &dst, &to));
        assert_eq!(fx.store.made_by(&i), vec![KEY.to_string()]);
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

    const KEY: &str = "fetcher:11111111111111111111111111111111 tar:22222222222222222222222222222222 uv:1.52.1";
    const OLD: &str = "fetcher:11111111111111111111111111111111 tar:33333333333333333333333333333333 uv:1.52.1";

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
        assert!(fx.store.made_by(i).iter().any(|k| k == KEY), "{:?}", fx.store.made_by(i));
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
        assert!(fx.store.made_by(&i).is_empty(), "made by matchapack's extractor");
        let (d, _) = check_once(&fx, &s, &i, &fx.root.join("node_modules/a"), |_| {});
        assert_eq!(d, None);
        assert_eq!(fx.store.made_by(&i), vec![KEY.to_string()]);
        // Known to this same pacote, and different now: changed since.
        let (d, _) = check_once(&fx, &s, &i, &fx.root.join("node_modules/b"), chmod_644);
        assert!(d.as_ref().is_some_and(|(k, m)| *k == "changed" && m.contains("bin/cli")), "{d:?}");
        // Made by matchapack's extractor: replaced.
        let (j, _) = fx.entry(&[("package.json", 0o644, "{\"name\":\"j\"}\n"), ("bin/cli", 0o755, "x\n")]);
        let (d, _) = check_once(&fx, &s, &j, &fx.root.join("node_modules/c"), chmod_644);
        assert_eq!(d.map(|x| x.0), Some("extractor"));
        // Known to other extraction code only: its rules differ from
        // this one's for this tarball. The record becomes this key alone.
        let (k, _) = fx.entry(&[("package.json", 0o644, "{\"name\":\"k\"}\n"), ("bin/cli", 0o755, "y\n")]);
        fx.store.set_made_by(&k, OLD).unwrap();
        let (d, _) = check_once(&fx, &s, &k, &fx.root.join("node_modules/d"), chmod_644);
        assert_eq!(d.map(|x| x.0), Some("pacote"));
        assert_eq!(fx.store.made_by(&k), vec![KEY.to_string()]);
        // Known to another version, and the same under this one: both kept.
        let (m, _) = fx.entry(&[("package.json", 0o644, "{\"name\":\"m\"}\n")]);
        fx.store.set_made_by(&m, OLD).unwrap();
        let (d, _) = check_once(&fx, &s, &m, &fx.root.join("node_modules/e"), |_| {});
        assert_eq!(d, None);
        assert_eq!(fx.store.made_by(&m), vec![OLD.to_string(), KEY.to_string()]);
        // This version first in the record, and different now: changed since.
        let (n, _) = fx.entry(&[("package.json", 0o644, "{\"name\":\"n\"}\n"), ("bin/cli", 0o755, "z\n")]);
        fx.store.set_made_by(&n, KEY).unwrap();
        fx.store.add_made_by(&n, OLD).unwrap();
        let (d, _) = check_once(&fx, &s, &n, &fx.root.join("node_modules/f"), chmod_644);
        assert_eq!(d.map(|x| x.0), Some("changed"));
    }

    #[test]
    fn a_directory_mode_changed_in_the_store_is_a_difference() {
        // A directory clone (macOS) carries directory modes into the
        // project, so the inject engine compares them (its umask is gated to
        // 022). Per-file clones make every directory anew: nothing to carry.
        let fx = Fx::new("serve-dir-mode");
        if !fx.can_clone() || crate::default_clone_mode() != clone::Mode::Dir {
            return;
        }
        let (i, _) = fx.entry(PKG);
        fx.store.set_made_by(&i, KEY).unwrap();
        fs::set_permissions(fx.store.dir_for(&i).join("lib"), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let s = fx.server_with(clone::Mode::Dir, CheckPolicy::All);
        let dst = fx.root.join("node_modules/p");
        let (d, _) = check_once(&fx, &s, &i, &dst, |_| {});
        assert!(d.as_ref().is_some_and(|(k, m)| *k == "changed" && m.contains("lib: directory mode 700")), "{d:?}");
        assert_eq!(fs::metadata(dst.join("lib")).unwrap().mode() & 0o777, 0o755);
    }

    /// An extended attribute (not compared) marks where a placed file came from.
    #[cfg(target_os = "macos")]
    fn xattr(p: &Path, set: Option<&str>) -> Option<String> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        let name = std::ffi::CString::new("user.matchapack-test").unwrap();
        unsafe {
            if let Some(v) = set {
                assert_eq!(libc::setxattr(path.as_ptr(), name.as_ptr(), v.as_ptr().cast(), v.len(), 0, libc::XATTR_NOFOLLOW), 0);
                return None;
            }
            let mut buf = [0u8; 64];
            let n = libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW);
            (n >= 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_match_is_placed_from_the_compared_clone_of_the_entry() {
        // The placed file is the entry's (blocks shared with the store, and
        // its extended attribute, which a directory clone keeps), by way of
        // the private clone that was compared: a replacement of the entry
        // meanwhile would have shown as a difference.
        let fx = Fx::new("serve-match-source");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        xattr(&fx.store.dir_for(&i).join("lib/index.js"), Some("from-the-entry"));
        let s = fx.server_with(clone::Mode::Dir, CheckPolicy::All);
        let dst = fx.root.join("node_modules/p");
        let to = extract_to(s.place_entry(&sri, &dst), "check");
        fx.pacote_extract(&i, &to);
        assert_eq!(placed_full(s.place_extracted(&sri, &dst, &to)).3, None, "a match");
        assert_eq!(xattr(&dst.join("lib/index.js"), None).as_deref(), Some("from-the-entry"));
        assert!(fs::read_dir(&s.tmp.path).unwrap().next().is_none(), "no candidate or extraction left behind");
    }

    #[test]
    fn a_failed_pacote_extraction_is_refused_and_its_directory_removed() {
        let fx = Fx::new("serve-extract-failed");
        if !fx.can_clone() {
            return;
        }
        let (_, sri) = fx.entry(PKG);
        let s = fx.server_with(clone::Mode::File, CheckPolicy::All);
        let dst = fx.root.join("node_modules/b");
        let to = extract_to(s.place_entry(&sri, &dst), "check");
        let (_, o) = s.extracted(&Value::Null, &sri, &dst, &to, true).swap_remove(0);
        assert_eq!(refusal(o).0, "pacote-extract-failed");
        assert!(!to.exists(), "its directory is removed");
        assert!(!dst.join("package.json").exists());
    }

    #[test]
    fn the_key_comes_from_the_hooks_key_message() {
        let fx = Fx::new("serve-key");
        let new = || Server::new(&fx.store, Some(fx.root.clone()), clone::Mode::File, None, CheckPolicy::OneIn(50)).unwrap();
        // The hello names versions only: no key (pacote is not loaded yet).
        let s = new();
        assert!(s.record_message(&json!({ "hello": true, "pacote": "21.5.1", "tar": "7.5.22", "uv": "1.52.1", "hook": "pending" })));
        let fp = "ab".repeat(32);
        assert!(s.record_message(&json!({ "key": KEY, "tar_fingerprint": fp })));
        assert_eq!(s.key(), KEY);
        let st = s.stats.lock().unwrap().to_json();
        assert_eq!((st["tar_fingerprint"].as_str(), st["tar"].as_str(), st["uv"].as_str()), (Some(fp.as_str()), Some("7.5.22"), Some("1.52.1")));
        drop(s);
        // Anything not of the exact shape is the unknown key.
        for bad in [
            json!(null),
            json!(42),
            json!("pacote@21.5.1 tar@7.5.22"),
            json!(KEY.replace(" uv:1.52.1", "")),
            json!(format!("{KEY} x")),
            json!(KEY.replace("uv:1.52.1", "uv:")),
            json!(KEY.replace("uv:1.52.1", "uv:1..2")),
            json!(KEY.replace("uv:1.52.1", "uv:1.52.1-rc")),
            json!(KEY.replace("fetcher:1", "fetcher:A")),
            json!(KEY.replace("tar:2", "tar:")),
            json!(KEY.replace(' ', "  ")),
        ] {
            let s = new();
            assert!(s.record_message(&json!({ "key": bad, "tar_fingerprint": null, "key_error": "x" })));
            assert_eq!(s.key(), UNKNOWN_KEY, "{bad}");
        }
        assert_eq!(code_key(&json!({ "hello": true, "key": KEY })), KEY, "store verify's hello");
        assert_eq!(code_key(&json!({ "hello": true })), UNKNOWN_KEY);
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
        // Nothing recorded (matchapack's extractor made it): compared.
        assert!(checks(&never()), "an entry no pacote made");
        // Known to this key: sampled.
        fx.store.set_made_by(&i, KEY).unwrap();
        assert!(!checks(&never()));
        {
            let later = fx.server_with(clone::Mode::File, CheckPolicy::OneIn(4));
            let picks = (0..4000).filter(|_| later.should_check(&[KEY.to_string()])).count();
            assert!((700..1300).contains(&picks), "{picks} of 4000 at one in 4");
        }
        // Known to other versions only: compared once under this one.
        fx.store.set_made_by(&i, OLD).unwrap();
        assert!(checks(&never()), "not yet compared under this extraction code");
        // Known to both (an install under each): sampled, whichever runs and
        // in whichever order they were recorded.
        fx.store.add_made_by(&i, KEY).unwrap();
        assert!(!checks(&never()));
        fx.store.set_made_by(&i, KEY).unwrap();
        fx.store.add_made_by(&i, OLD).unwrap();
        assert_eq!(fx.store.made_by(&i), vec![KEY.to_string(), OLD.to_string()]);
        assert!(!checks(&never()), "this version first in the record");
        // Records written before the key was the code (`pacote@… tar@…`)
        // vouch for nothing under a code key; nor does another libuv.
        fx.store.set_made_by(&i, "pacote@21.5.1 tar@7.5.22").unwrap();
        assert!(checks(&never()), "an old-format record");
        fx.store.set_made_by(&i, &KEY.replace("uv:1.52.1", "uv:1.51.0")).unwrap();
        assert!(checks(&never()), "only another libuv recorded");
        fx.store.add_made_by(&i, KEY).unwrap();
        assert!(!checks(&never()), "both libuv versions recorded");
        // A key the hook could not name vouches for nothing.
        let fresh = Fx::new("serve-policy-unknown");
        let (j, sri_j) = fresh.entry(PKG);
        let unknown = Server::new(&fresh.store, Some(fresh.root.clone()), clone::Mode::File, Some(UNKNOWN_KEY.into()), CheckPolicy::OneIn(1_000_000)).unwrap();
        fresh.store.set_made_by(&j, UNKNOWN_KEY).unwrap();
        assert!(matches!(unknown.place_entry(&sri_j, &fresh.root.join("node_modules/r")), Outcome::Extract { why: "check", .. }));
    }

    #[test]
    fn an_extraction_no_clone_reproduces_halts_the_run_and_declines_the_key() {
        let fx = Fx::new("serve-odd-extraction");
        if !fx.can_clone() {
            return;
        }
        let (i, sri) = fx.entry(PKG);
        fx.store.set_made_by(&i, OLD).unwrap();
        // pacote's extraction with a file older than the install (here, an
        // hour and a minute): not what a clone touched to now gives.
        let s = fx.server_with(clone::Mode::File, CheckPolicy::OneIn(1_000_000));
        let dst = fx.root.join("node_modules/odd");
        let to = extract_to(s.place_entry(&sri, &dst), "check");
        fx.pacote_extract(&i, &to);
        crate::store::tests::age(&to.join("lib/index.js"), 3660);
        let (reason, message, fatal) = refusal(s.place_extracted(&sri, &dst, &to));
        assert_eq!((reason, fatal), ("extraction-unexpected", true), "{message}");
        assert!(message.contains("lib/index.js: modified at") && message.contains("before this install began"), "{message}");
        assert!(!to.exists() && !dst.exists(), "nothing placed, the extraction removed");
        assert_eq!(fx.store.made_by(&i), vec![OLD.to_string()], "the entry and its record as they were");
        assert!(fx.store.declined(KEY).is_some_and(|w| w.contains("index.js")), "{:?}", fx.store.declined(KEY));
        assert!(fx.store.declined(OLD).is_none());
        assert!(s.stats.lock().unwrap().extraction_unexpected.is_some());
        // Every later request of this run: the same fatal refusal.
        let (reason, _, fatal) = refusal(s.place_entry(&sri, &fx.root.join("node_modules/later")));
        assert_eq!((reason, fatal), ("extraction-unexpected", true));
        drop(s);
        // A later install under the declined key: refused from its first
        // request, as soon as the key is known (npm extracts everything).
        let s = Server::new(&fx.store, Some(fx.root.clone()), clone::Mode::File, None, CheckPolicy::OneIn(50)).unwrap();
        assert!(s.record_message(&json!({ "key": KEY, "tar_fingerprint": null })));
        let (reason, message, fatal) = refusal(s.place_entry(&sri, &fx.root.join("node_modules/next")));
        assert_eq!((reason, fatal), ("key-declined", true), "{message}");
        assert!(s.stats.lock().unwrap().key_declined.is_some());
        drop(s);
        // Under another key nothing is declined. A symlink in pacote's
        // extraction is not reproduced either; a file written just now is.
        let other = Server::new(&fx.store, Some(fx.root.clone()), clone::Mode::File, Some(OLD.into()), CheckPolicy::All).unwrap();
        let to = extract_to(other.place_entry(&sri, &fx.root.join("node_modules/l")), "check");
        fx.pacote_extract(&i, &to);
        std::os::unix::fs::symlink("index.js", to.join("lib/link.js")).unwrap();
        let (reason, message, _) = refusal(other.place_extracted(&sri, &fx.root.join("node_modules/l"), &to));
        assert_eq!(reason, "extraction-unexpected", "{message}");
        assert!(message.contains("lib/link.js: neither a file nor a directory"), "{message}");
        drop(other);
        fx.store.forget_made_by(&i);
        let fresh = Server::new(&fx.store, Some(fx.root.clone()), clone::Mode::File, Some(KEY.replace("uv:1.52.1", "uv:9")), CheckPolicy::All).unwrap();
        let to = extract_to(fresh.place_entry(&sri, &fx.root.join("node_modules/f")), "check");
        fx.pacote_extract(&i, &to);
        assert_eq!(placed_full(fresh.place_extracted(&sri, &fx.root.join("node_modules/f"), &to)).2, true, "a fresh extraction is compared and placed");
        assert_eq!(fresh.stats.lock().unwrap().new_key_checks, 1);
    }

    #[test]
    fn a_request_from_npm_installing_into_another_root_is_named_as_such() {
        let fx = Fx::new("serve-npm-root");
        let s = fx.server(clone::Mode::File);
        let req = |root: &Path| {
            let line = json!({ "id": 1, "integrity": "sha512-x", "dest": fx.root.join("node_modules/p"), "npm_root": root }).to_string();
            serde_json::from_str::<Value>(&s.handle_line(&line)[0]).unwrap()
        };
        let elsewhere = req(&fx.dir);
        assert_eq!(elsewhere["reason"], "npm-root-elsewhere", "{elsewhere}");
        // The project's own root, also through a symlink: the request is
        // judged on its own (here: a malformed integrity).
        assert_eq!(req(&fx.root)["reason"], "rejected-source");
        let link = fx.dir.join("root-link");
        std::os::unix::fs::symlink(&fx.root, &link).unwrap();
        assert_eq!(req(&link)["reason"], "rejected-source");
    }

    #[test]
    fn the_root_npm_reports_is_gated_and_pinned_once() {
        let fx = Fx::new("serve-root-handshake");
        let new = || Server::new(&fx.store, None, clone::Mode::File, Some(KEY.into()), CheckPolicy::OneIn(50)).unwrap();
        let line = |s: &Server, v: Value| serde_json::from_str::<Value>(&s.handle_line(&v.to_string())[0]).unwrap();
        let root_msg = |root: &Path, extra: Value| {
            let mut m = json!({ "id": 0, "root": root, "global": false, "command": "ci", "typed": "ci", "dry_run": false, "package_lock_only": false, "usage": false });
            if let (Value::Object(m), Value::Object(x)) = (&mut m, extra) {
                m.extend(x);
            }
            m
        };
        let req = json!({ "id": 1, "integrity": "sha512-x", "dest": fx.root.join("node_modules/p") });

        // Before npm reported its root: nothing is placed.
        // (One server at a time: each has its own directory under the
        // store's tmp/, named per process.)
        let s = new();
        assert_eq!(line(&s, req.clone())["reason"], "no-root");
        assert!(s.check_dest(&fx.root.join("node_modules/p")).is_err());
        drop(s);

        // Refusals: recorded as the decline reason, the root stays unset.
        let other = fx.dir.join("not-there");
        let nm_link = fx.dir.join("nm-link");
        fs::create_dir_all(&nm_link).unwrap();
        std::os::unix::fs::symlink(fx.dir.join("elsewhere"), nm_link.join("node_modules")).unwrap();
        for (msg, want) in [
            (root_msg(&fx.root, json!({ "global": true })), "npm installs globally"),
            (root_msg(&fx.root, json!({ "command": "install", "typed": "i" })), "npm runs `install` for `i`"),
            (root_msg(&fx.root, json!({ "command": null, "typed": "u" })), "npm has no command `u`"),
            (root_msg(&fx.root, json!({ "dry_run": true })), "this run installs nothing (--dry-run)"),
            (root_msg(&fx.root, json!({ "package_lock_only": true })), "this run installs nothing (--package-lock-only)"),
            (root_msg(&fx.root, json!({ "usage": true })), "this run installs nothing (--usage)"),
            (root_msg(&other, json!({})), "does not exist yet"),
            (root_msg(Path::new("relative"), json!({})), "no absolute install root"),
            (root_msg(&nm_link, json!({})), "node_modules is a symlink"),
        ] {
            let s = new();
            let a = line(&s, msg.clone());
            assert_eq!(a["ok"], false, "{msg}");
            assert!(a["reason"].as_str().unwrap().contains(want), "{a} {want}");
            let st = s.stats.lock().unwrap();
            assert!(st.declined.as_deref().is_some_and(|d| d.contains(want)) && st.root.is_none(), "{msg}");
            drop(st);
            assert_eq!(line(&s, req.clone())["reason"], "no-root");
            drop(s);
        }

        // Accepted: canonical (here through a symlink), pinned, and the
        // requests are judged against it; a second report is refused.
        let s = new();
        let link = fx.dir.join("root-link");
        std::os::unix::fs::symlink(&fx.root, &link).unwrap();
        let a = line(&s, root_msg(&link, json!({ "typed": "ic" })));
        assert_eq!((a["ok"].as_bool(), a["root"].as_str()), (Some(true), fx.root.to_str()), "{a}");
        {
            let st = s.stats.lock().unwrap();
            assert_eq!((st.root.as_deref(), st.command.as_deref(), st.declined.as_deref()), (Some(fx.root.as_path()), Some("ci"), None));
        }
        assert_eq!(line(&s, req.clone())["reason"], "rejected-source", "judged on its own once the root is pinned");
        let again = line(&s, root_msg(&fx.dir, json!({})));
        assert_eq!(again["ok"], false, "{again}");
        assert!(s.check_dest(&fx.root.join("node_modules/p")).is_ok());
        assert_eq!(s.stats.lock().unwrap().root.as_deref(), Some(fx.root.as_path()));

        // The helper's other messages are recorded.
        assert!(s.record_message(&json!({ "hook": "not-installed", "reason": "no FetcherBase" })));
        assert!(s.record_message(&json!({ "no_root": "npm's lib/npm.js has no Npm.prototype.exec" })));
        let j = s.stats.lock().unwrap().to_json();
        assert_eq!((j["hook"].as_str(), j["hook_reason"].as_str(), j["command"].as_str()), (Some("not-installed"), Some("no FetcherBase"), Some("ci")));
        assert_eq!(j["root"].as_str(), fx.root.to_str());
        assert!(j["no_root"].as_str().unwrap().contains("no Npm.prototype.exec"));
    }

    /// Answer lines from `handle_line`, by id.
    fn answers(s: &Server, req: Value) -> BTreeMap<String, Value> {
        s.handle_line(&req.to_string())
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .map(|v| (v["id"].to_string(), v))
            .collect()
    }

    #[test]
    fn a_package_at_several_paths_is_filled_once_and_the_others_wait_for_it() {
        let fx = Fx::new("serve-coalesce");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        let sri = sri_of(&i);
        let s = fx.server_with(clone::Mode::File, CheckPolicy::All);
        let dst = |n: &str| fx.root.join(format!("node_modules/{n}"));
        let first = answers(&s, json!({ "id": 1, "integrity": sri, "dest": dst("a") }));
        let to = PathBuf::from(first["1"]["extract_to"].as_str().expect("the first fills"));
        assert!(answers(&s, json!({ "id": 2, "integrity": sri, "dest": dst("b") })).is_empty(), "the second waits");
        assert!(answers(&s, json!({ "id": 3, "integrity": sri, "dest": dst("c") })).is_empty(), "the third too");
        fx.pacote_extract(&i, &to);
        let done = answers(&s, json!({ "id": 4, "integrity": sri, "dest": dst("a"), "extracted": to }));
        assert_eq!(done.len(), 3, "{done:?}");
        for id in ["4", "2", "3"] {
            assert_eq!(done[id]["ok"], true, "{id}: {:?}", done[id]);
        }
        assert_eq!(done["4"]["filled"], true);
        let want = listing(&fx.store.dir_for(&i));
        for n in ["a", "b", "c"] {
            assert_eq!(listing(&dst(n)), want, "{n}");
        }
        let st = s.stats.lock().unwrap();
        assert_eq!((st.filled.len(), st.cloned, st.checked), (1, 3, 0), "one extraction; nothing compared with itself");
        // Just extracted: placed without a touch.
        assert_eq!(st.paths_touched, 0);
    }

    #[test]
    fn requests_waiting_for_a_fill_another_install_published_first_go_to_pacote() {
        // The entry they would be cloned from was not made by this install's
        // pacote, and they were not compared with it.
        let fx = Fx::new("serve-coalesce-lost");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        let sri = sri_of(&i);
        let s = fx.server(clone::Mode::File);
        let dst = |n: &str| fx.root.join(format!("node_modules/{n}"));
        let first = answers(&s, json!({ "id": 1, "integrity": sri, "dest": dst("a") }));
        let to = PathBuf::from(first["1"]["extract_to"].as_str().unwrap());
        assert!(answers(&s, json!({ "id": 2, "integrity": sri, "dest": dst("b") })).is_empty());
        fx.pacote_extract(&i, &to);
        assert!(matches!(fx.store.ensure(&i, "http://x/p.tgz").unwrap(), Ensure::Extracted), "another install publishes first");
        let done = answers(&s, json!({ "id": 3, "integrity": sri, "dest": dst("a"), "extracted": to }));
        assert_eq!(done["3"]["ok"], true, "{done:?}");
        assert_eq!(done["2"]["reason"], "store-miss", "{done:?}");
        assert!(!dst("b").join("package.json").exists());
    }

    #[test]
    fn an_entry_replaced_after_this_install_published_it_is_not_given_to_waiters() {
        // Another install (another pacote) replaces the entry between this
        // install's publication and the release of its waiters.
        let fx = Fx::new("serve-coalesce-replaced");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        let sri = sri_of(&i);
        let s = fx.server(clone::Mode::File);
        let dst = |n: &str| fx.root.join(format!("node_modules/{n}"));
        let to = extract_to(s.place_entry(&sri, &dst("a")), "fill");
        fx.pacote_extract(&i, &to);
        placed_full(s.place_extracted(&sri, &dst("a"), &to));
        assert!(s.ours(&i), "published by this install");
        let entry = fx.store.dir_for(&i);
        fs::rename(&entry, fx.dir.join("old-entry")).unwrap();
        fx.pacote_extract(&i, &{
            fs::create_dir_all(&entry).unwrap();
            entry.clone()
        });
        assert!(!s.ours(&i), "the entry is another directory now");
        s.filling.lock().unwrap().insert(i.clone(), vec![(json!(9), dst("b"))]);
        let released = s.release(&i);
        assert_eq!(refusal(released.into_iter().next().unwrap().1).0, "store-miss");
        // And a plain request is touched again (the entry's times are not this install's).
        assert!(matches!(s.place_entry(&sri, &dst("c")), Outcome::Placed { paths, .. } if paths > 0));
    }

    #[test]
    fn requests_waiting_for_a_fill_that_fails_go_to_pacote() {
        let fx = Fx::new("serve-coalesce-fail");
        if !fx.can_clone() {
            return;
        }
        let i = cached_tarball(&fx.cacache, PKG);
        let sri = sri_of(&i);
        let s = fx.server(clone::Mode::File);
        let dst = |n: &str| fx.root.join(format!("node_modules/{n}"));
        let first = answers(&s, json!({ "id": 1, "integrity": sri, "dest": dst("a") }));
        let to = first["1"]["extract_to"].as_str().unwrap().to_string();
        assert!(answers(&s, json!({ "id": 2, "integrity": sri, "dest": dst("b") })).is_empty());
        let done = answers(&s, json!({ "id": 3, "integrity": sri, "dest": dst("a"), "extracted": to, "failed": true }));
        assert_eq!(done["3"]["reason"], "pacote-extract-failed");
        assert_eq!(done["2"]["reason"], "store-miss", "{done:?}");
        // The next request for it fills again.
        let again = answers(&s, json!({ "id": 5, "integrity": sri, "dest": dst("b") }));
        assert!(again["5"]["extract_to"].is_string(), "{again:?}");
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
    fn result_reads_and_the_helpers_decline_are_recorded() {
        let fx = Fx::new("serve-result-reads");
        let s = fx.server(Mode::File);
        assert!(s.record_message(&json!({ "result_read": "resolved", "expected": true })));
        assert!(s.record_message(&json!({ "result_read": "integrity", "expected": true })));
        assert!(s.record_message(&json!({ "result_read": "resolved", "expected": true })));
        // Anything not marked expected is unexpected; the key is kept printable.
        assert!(s.record_message(&json!({ "result_read": "files" })));
        assert!(s.record_message(&json!({ "result_read": "a b\n", "expected": false })));
        // The decline is answered, so the helper can wait for it to be recorded.
        let answers = s.handle_line(&json!({ "id": "declined", "declined": "npm 9 has pacote code …" }).to_string());
        assert_eq!(answers.len(), 1);
        let a: Value = serde_json::from_str(&answers[0]).unwrap();
        assert_eq!((a["id"].as_str(), a["ok"].as_bool()), (Some("declined"), Some(true)));
        let st = s.stats.lock().unwrap();
        assert_eq!(st.result_reads.get("resolved"), Some(&2));
        assert_eq!(st.result_reads.get("integrity"), Some(&1));
        assert_eq!(st.result_unexpected.get("files"), Some(&1));
        assert_eq!(st.result_unexpected.get("ab"), Some(&1));
        assert_eq!(st.declined.as_deref(), Some("npm 9 has pacote code …"));
        assert_eq!(st.fallbacks.len(), 0, "neither is a package left to npm");
        let j = st.to_json();
        assert_eq!(j["result_unexpected"]["files"], 1);
        assert_eq!(j["result_reads"]["resolved"], 2);
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
