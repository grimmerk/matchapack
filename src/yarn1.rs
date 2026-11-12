//! The Yarn 1 engine (`matchapack yarn <args>`; NOTES "Yarn 1 engine"): Yarn 1
//! runs its own `yarn install`, in a node child started here through the hook
//! in src/yarn1-hook.js, and during Yarn's link step each file Yarn copies
//! from its cache into node_modules is made as a copy-on-write clone of the
//! cache file instead (macOS `clonefile(2)`; Node cannot clone on macOS).
//! Yarn's own code places every file, so the tree is Yarn's by construction;
//! no store entry is used (Yarn's cache is the source), so `store gc`,
//! `store verify` and the project records are not involved.
//!
//! A clone is made to look like the copy Node's `fs.copyFile` would have
//! left, by facts the hook measures at start with Node's own copy (times of
//! an empty and a non-empty file, the group): times Node's copy keeps are left
//! as the clone has them (the cache file's), the others set to now; the group
//! is set to the cache file's when Node's copy does that (libuv's `fchown`,
//! result ignored, so `lchown` with the result ignored); every extended
//! attribute but `com.apple.provenance` is removed (Node's copy carries none
//! over; the kernel adds `com.apple.provenance` to both). Yarn's `fixTimes`
//! then runs as after a copy. A clone that cannot be made so is removed, and
//! Node copies the file.
//!
//! Protocol (JSON lines over fd 3, the inject engine's transport,
//! `serve::serve_lines`):
//!   node → matchapack
//!     {"id":0,"probe":["/…/nonempty","/…/nonempty.copy"]}  before Yarn starts
//!     {"declined":"why"}                     the hook stops; plain Yarn runs
//!     {"hello":true,"yarn":"1.22.22","first":128,"facts":{…},"hook":"on"}
//!     {"link_step":"start"} / {"link_step":"end","counts":{"first-copies":128}}
//!     {"id":7,"src":"/cache/…/f.js","dest":"/project/node_modules/…/f.js"}
//!   matchapack → node
//!     {"id":0,"shares":false}
//!     {"id":7,"ok":true}
//!     {"id":7,"ok":false,"reason":"rejected-dest","fatal":false}
//! `fatal`: no later request can be cloned (the filesystem pair cannot
//! clone, the cache is on another volume, Yarn's cache folder is unknown):
//! the hook stops asking.
//!
//! What a request may name (it names paths; a Yarn cache entry's name cannot
//! be derived from the lockfile): `src` an absolute plain path, a regular file
//! (lstat; never a symlink) whose parent resolves inside Yarn's cache folder;
//! `dest` an absolute plain path whose parent resolves inside the project
//! root and below a `node_modules` of it. The cache folder comes from the
//! user's Yarn (`yarn cache dir`, run here in parallel with the install,
//! with the same working directory, environment and cache arguments); until
//! it is known every request is answered "copy". These checks catch a
//! confused hook, not an attacker: this process does nothing node could not
//! do with `fs.copyFile` as the same user. The verdict is kept per parent
//! directory (there are about seven files per directory); a directory
//! swapped for a symlink after its check is not caught (as in serve.rs).

use crate::serve::serve_lines;
use crate::{trial, util};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Yarn options that take a value (required or optional), as Yarn's own
/// command-line parser binds them (`commander.optionFor`, lib/cli.js "get
/// command name"): the word after one of these is its value, not a command.
const VALUE_OPTIONS: &[&str] = &[
    "--use-yarnrc",
    "--prod",
    "--production",
    "--link-folder",
    "--global-folder",
    "--modules-folder",
    "--preferred-cache-folder",
    "--cache-folder",
    "--mutex",
    "--emoji",
    "--cwd",
    "--proxy",
    "--https-proxy",
    "--registry",
    "--network-concurrency",
    "--network-timeout",
    "--scripts-prepend-node-path",
    "--otp",
];

/// Options that move the install out of the project matchapack checks
/// (the clone server accepts destinations inside it only).
const MOVING_OPTIONS: &[&str] = &["--modules-folder", "--cwd", "--global-folder"];

/// Options that decide where Yarn's cache is: passed to `yarn cache dir`.
const CACHE_OPTIONS: &[&str] = &["--cache-folder", "--preferred-cache-folder", "--use-yarnrc", "--no-default-rc"];

/// A request older than this when its clone would start is answered "copy"
/// without cloning: the hook gives up on an answer after 30 s
/// (src/yarn1-hook.js) and copies, and a clone made after that must not land
/// on its copy.
const LATE: Duration = Duration::from_secs(20);

/// How long a request waits for Yarn's cache folder (`yarn cache dir`, run in
/// parallel with the install) before it is answered "copy". Measured: with a
/// warm cache a small install reaches its link step before `yarn cache dir`
/// answers, and answering "copy" at once cloned nothing (yarn1-emptyfirst:
/// 314 of 314 files).
const CACHE_WAIT: Duration = Duration::from_secs(10);

/// The name of the command Yarn will run (empty: Yarn's default, install),
/// found the way Yarn finds it.
pub fn command(args: &[String]) -> &str {
    let mut prev_binds = false;
    for a in args {
        let is_option = a.starts_with('-');
        if !is_option && !prev_binds {
            return a;
        }
        prev_binds = is_option && VALUE_OPTIONS.contains(&a.as_str());
    }
    ""
}

/// Whether `matchapack yarn <args>` is an install the engine handles: `yarn`
/// or `yarn install`, not `yarn --version` / `--help`.
pub fn is_install(args: &[String]) -> bool {
    if args.first().is_some_and(|a| a == "-v") || args.iter().any(|a| a == "--version" || a == "--help" || a == "-h") {
        return false;
    }
    matches!(command(args), "" | "install")
}

fn option_name(a: &str) -> &str {
    a.split('=').next().unwrap_or(a)
}

/// The cache options among `args`, with their values, for `yarn cache dir`.
fn cache_args(args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = option_name(a);
        if !CACHE_OPTIONS.contains(&name) {
            continue;
        }
        out.push(a.clone());
        if name == a.as_str() && VALUE_OPTIONS.contains(&name) {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

/// The `yarn.js` the user's `yarn` runs: MATCHAPACK_YARN_JS, or `yarn` on PATH
/// resolved to a file of Yarn 1's npm package (an npm-global or Homebrew
/// install: `…/yarn/bin/yarn.js`, or the `bin/yarn` sh launcher beside it).
/// Returns it with the package's version. A corepack shim, Yarn 2+ or another
/// launcher is not handled here (yet).
pub fn find_yarn_js() -> std::result::Result<(PathBuf, String), String> {
    let p = match std::env::var_os("MATCHAPACK_YARN_JS") {
        Some(p) => PathBuf::from(p),
        None => {
            let path = std::env::var_os("PATH").ok_or("no PATH")?;
            std::env::split_paths(&path).map(|d| d.join("yarn")).find(|p| p.is_file()).ok_or("no yarn on PATH")?
        }
    };
    let real = std::fs::canonicalize(&p).map_err(|e| format!("resolving {}: {e}", p.display()))?;
    let js = if real.file_name().is_some_and(|n| n == "yarn.js") { real.clone() } else { real.with_file_name("yarn.js") };
    let pkg = js.parent().and_then(|b| b.parent()).map(|d| d.join("package.json"));
    let v: Option<Value> = pkg.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    let name = v.as_ref().and_then(|v| v["name"].as_str());
    let version = v.as_ref().and_then(|v| v["version"].as_str()).unwrap_or("").to_string();
    if !js.is_file() || name != Some("yarn") {
        return Err(format!("{} is not Yarn 1's npm package (a corepack shim, Yarn 2+ or another launcher is not handled)", real.display()));
    }
    if !version.starts_with("1.") {
        return Err(format!("yarn {version} at {} is not Yarn 1", js.display()));
    }
    Ok((js, version))
}

/// Yarn's lockfile folder: the nearest directory at or above `cwd` that holds
/// `yarn.lock` (with workspaces, the workspace root).
fn lockfile_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| d.join("yarn.lock").is_file()).map(Path::to_path_buf)
}

/// A `.yarnrc` line that sets `yarn-path` (`yarn-path "…"`, `"yarn-path" …`).
fn sets_yarn_path(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim_start();
        let key = l.strip_prefix('"').map(|r| r.split('"').next().unwrap_or("")).unwrap_or_else(|| l.split_whitespace().next().unwrap_or(""));
        key == "yarn-path"
    })
}

/// The checks that must hold before Yarn starts (each failure: plain Yarn,
/// nothing changed). Yarn's flags and settings are Yarn's to read; only
/// what moves the install, or makes another program than Yarn 1's bundle
/// run, is refused here.
fn gates(cwd: &Path, args: &[String]) -> std::result::Result<PathBuf, String> {
    for a in args {
        if MOVING_OPTIONS.contains(&option_name(a)) {
            return Err(format!("option {a} moves the install out of this directory"));
        }
    }
    let root = lockfile_root(cwd).ok_or("no yarn.lock in this directory or above")?;
    // Yarn 2 and later: their own linker, not this hook. (A `.yarnrc.yml`
    // above the project, such as a user's ~/.yarnrc.yml, does not make it
    // one.) Yarn 1.22 runs a `yarnPath` from `.yarnrc.yml` itself.
    for d in cwd.ancestors() {
        if d.join(".yarnrc.yml").exists() {
            return Err(format!("{} has a .yarnrc.yml (a Yarn 2+ project)", d.display()));
        }
        if d == root {
            break;
        }
    }
    let lock = std::fs::read_to_string(root.join("yarn.lock")).map_err(|e| format!("reading yarn.lock: {e}"))?;
    if lock.lines().any(|l| l.starts_with("__metadata:")) {
        return Err("yarn.lock is a Yarn 2+ lockfile".into());
    }
    for d in [cwd, root.as_path()] {
        let Ok(text) = std::fs::read(d.join("package.json")) else { continue };
        let pm = serde_json::from_slice::<Value>(&text).ok().and_then(|v| v["packageManager"].as_str().map(String::from));
        if let Some(pm) = pm {
            if !pm.starts_with("yarn@1.") {
                return Err(format!("packageManager is {pm}"));
            }
        }
    }
    // Yarn 1 re-runs itself as another file in a child process the hook is
    // not in.
    let mut rcs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".yarnrc")).collect();
    rcs.push(crate::home().join(".yarnrc"));
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--use-yarnrc" {
            if let Some(v) = it.next() {
                rcs.push(cwd.join(v));
            }
        } else if let Some(v) = a.strip_prefix("--use-yarnrc=") {
            rcs.push(cwd.join(v));
        }
    }
    for rc in rcs {
        if std::fs::read_to_string(&rc).is_ok_and(|t| sets_yarn_path(&t)) {
            return Err(format!("{} sets yarn-path (Yarn would run another file)", rc.display()));
        }
    }
    if std::env::var_os("YARN_YARN_PATH").is_some() {
        return Err("YARN_YARN_PATH is set (Yarn would run another file)".into());
    }
    if std::fs::symlink_metadata(root.join("node_modules")).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{}/node_modules is a symlink", root.display()));
    }
    trial::in_scope(cwd)?;
    Ok(root)
}

/// What Node's copy keeps here, measured by the hook (src/yarn1-hook.js
/// `probe`). `mtime` (a non-empty copy kept its mtime) is never true here:
/// the hook declines then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    pub empty_atime: bool,
    pub empty_mtime: bool,
    pub atime: bool,
    /// The copy takes the source's owner and group (None: not measurable,
    /// the user is in one group only).
    pub group: Option<bool>,
}

impl Facts {
    fn from_json(v: &Value) -> Option<Facts> {
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
        if b("mtime")? {
            return None;
        }
        Some(Facts { empty_atime: b("empty_atime")?, empty_mtime: b("empty_mtime")?, atime: b("atime")?, group: b("group") })
    }

    /// The times a clone of a file of `len` bytes keeps (atime, mtime).
    fn kept(&self, len: u64) -> (bool, bool) {
        if len == 0 { (self.empty_atime, self.empty_mtime) } else { (self.atime, false) }
    }
}

/// Where Yarn's cache folder stands for this run.
#[derive(Debug, Clone)]
enum Cache {
    /// `yarn cache dir` has not answered yet.
    Pending,
    Known(PathBuf),
    /// No clone can be made from it (why).
    Unusable(String),
}

#[derive(Debug)]
enum Outcome {
    Cloned { bytes: u64 },
    Copy { reason: String, fatal: bool, message: Option<String> },
}

fn copy(reason: impl Into<String>) -> Outcome {
    Outcome::Copy { reason: reason.into(), fatal: false, message: None }
}

/// What a run did, for the log and the summary line.
#[derive(Debug, Default)]
pub struct Stats {
    pub hello: Option<Value>,
    pub declined: Option<String>,
    pub cloned: u64,
    pub cloned_bytes: u64,
    /// Files Node copied, by reason (the server's refusals and the hook's
    /// own counts).
    pub copied: BTreeMap<String, u64>,
    pub link_steps: u64,
    pub link_step_ends: u64,
    pub cache: Option<String>,
    pub errors: Vec<String>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    pub fn copied_total(&self) -> u64 {
        self.copied.values().sum()
    }

    pub fn to_json(&self) -> Value {
        let wall = match (self.first, self.last) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
            _ => 0.0,
        };
        json!({
            "cloned": self.cloned,
            "cloned_bytes": self.cloned_bytes,
            "copied": self.copied,
            "copied_total": self.copied_total(),
            "link_steps": self.link_steps,
            "first": self.hello.as_ref().and_then(|h| h.get("first")).cloned().unwrap_or(Value::Null),
            "facts": self.hello.as_ref().and_then(|h| h.get("facts")).cloned().unwrap_or(Value::Null),
            "cache": self.cache,
            "serve_wall_secs": (wall * 100.0).round() / 100.0,
            "errors": self.errors,
        })
    }
}

/// Test switches, read once from the environment (scripts/test-yarn1.sh).
#[derive(Debug, Default, Clone, Copy)]
struct TestSwitches {
    /// Refuse every Nth clone request without cloning (EIO).
    fail_every: u64,
    /// Make every Nth clone, then fail its fix-up.
    fail_fixup_every: u64,
    /// Say the probe's copy shares blocks with its source.
    probe_shares: bool,
}

impl TestSwitches {
    fn from_env() -> TestSwitches {
        let n = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        TestSwitches {
            fail_every: n("MATCHAPACK_YARN_FAIL_EVERY"),
            fail_fixup_every: n("MATCHAPACK_YARN_FAIL_FIXUP_EVERY"),
            probe_shares: std::env::var("MATCHAPACK_YARN_SIMULATE").as_deref() == Ok("probe-shares"),
        }
    }
}

pub struct Server {
    /// The project root (Yarn's lockfile folder), canonical.
    root: PathBuf,
    root_dev: u64,
    cache: Mutex<Cache>,
    cache_known: std::sync::Condvar,
    cache_wait: Duration,
    facts: OnceLock<Facts>,
    /// Set once a clone failed because the filesystem pair cannot clone.
    unsupported: AtomicBool,
    stopping: AtomicBool,
    /// Verdicts per parent directory, as given (not resolved).
    dest_dirs: Mutex<HashMap<PathBuf, std::result::Result<(), String>>>,
    src_dirs: Mutex<HashMap<PathBuf, std::result::Result<(), String>>>,
    test: TestSwitches,
    requests: AtomicU64,
    pub stats: Mutex<Stats>,
}

fn plain_absolute(p: &Path) -> bool {
    p.is_absolute() && p.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

impl Server {
    pub fn new(root: PathBuf) -> Result<Server> {
        let root = std::fs::canonicalize(&root).with_context(|| format!("resolving {}", root.display()))?;
        let root_dev = std::fs::metadata(&root)?.dev();
        Ok(Server {
            root,
            root_dev,
            cache: Mutex::new(Cache::Pending),
            cache_known: std::sync::Condvar::new(),
            cache_wait: CACHE_WAIT,
            facts: OnceLock::new(),
            unsupported: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            dest_dirs: Mutex::new(HashMap::new()),
            src_dirs: Mutex::new(HashMap::new()),
            test: TestSwitches::from_env(),
            requests: AtomicU64::new(0),
            stats: Mutex::new(Stats::default()),
        })
    }

    /// Yarn's cache folder as `yarn cache dir` printed it, or why it is not
    /// known. A folder on another volume than the project cannot be cloned
    /// from.
    pub fn set_cache(&self, dir: std::result::Result<PathBuf, String>) {
        let state = match dir.and_then(|d| std::fs::canonicalize(&d).map_err(|e| format!("resolving {}: {e}", d.display()))) {
            Ok(d) => match std::fs::metadata(&d) {
                Ok(m) if m.dev() != self.root_dev => Cache::Unusable(format!("Yarn's cache {} is on another volume than the project", d.display())),
                Ok(_) => Cache::Known(d),
                Err(e) => Cache::Unusable(format!("{}: {e}", d.display())),
            },
            Err(e) => Cache::Unusable(e),
        };
        self.stats.lock().unwrap().cache = Some(match &state {
            Cache::Known(d) => d.display().to_string(),
            Cache::Unusable(why) => format!("unusable: {why}"),
            Cache::Pending => "pending".into(),
        });
        *self.cache.lock().unwrap() = state;
        self.cache_known.notify_all();
    }

    /// `dest`'s parent must resolve inside the project root, below one of its
    /// node_modules.
    fn check_dest(&self, dest: &Path) -> std::result::Result<(), String> {
        if !plain_absolute(dest) {
            return Err("the destination is not a plain absolute path".into());
        }
        let parent = dest.parent().ok_or("the destination has no parent")?;
        if let Some(v) = self.dest_dirs.lock().unwrap().get(parent) {
            return v.clone();
        }
        let v = (|| {
            let real = std::fs::canonicalize(parent).map_err(|e| format!("resolving {}: {e}", parent.display()))?;
            let rel = real.strip_prefix(&self.root).map_err(|_| format!("{} resolves outside the project ({})", parent.display(), real.display()))?;
            if !rel.components().any(|c| c.as_os_str() == "node_modules") {
                return Err(format!("{} is not inside a node_modules of the project", real.display()));
            }
            Ok(())
        })();
        self.dest_dirs.lock().unwrap().insert(parent.to_path_buf(), v.clone());
        v
    }

    /// `src` must be a regular file whose parent resolves inside `cache`.
    fn check_src(&self, src: &Path, cache: &Path) -> std::result::Result<std::fs::Metadata, String> {
        if !plain_absolute(src) {
            return Err("the source is not a plain absolute path".into());
        }
        let parent = src.parent().ok_or("the source has no parent")?;
        let known = self.src_dirs.lock().unwrap().get(parent).cloned();
        let v = match known {
            Some(v) => v,
            None => {
                let v = match std::fs::canonicalize(parent) {
                    Ok(real) if real.starts_with(cache) => Ok(()),
                    Ok(real) => Err(format!("{} resolves outside Yarn's cache ({})", parent.display(), real.display())),
                    Err(e) => Err(format!("resolving {}: {e}", parent.display())),
                };
                self.src_dirs.lock().unwrap().insert(parent.to_path_buf(), v.clone());
                v
            }
        };
        v?;
        match std::fs::symlink_metadata(src) {
            Ok(m) if m.file_type().is_file() => Ok(m),
            Ok(_) => Err(format!("{} is not a regular file", src.display())),
            Err(e) => Err(format!("{}: {e}", src.display())),
        }
    }

    fn clone_file(&self, src: &Path, dest: &Path, read_at: Instant) -> Outcome {
        let Some(facts) = self.facts.get().copied() else { return copy("no-hello") };
        let cache = {
            let g = self.cache.lock().unwrap();
            let (g, _) = self.cache_known.wait_timeout_while(g, self.cache_wait, |c| matches!(c, Cache::Pending)).unwrap();
            g.clone()
        };
        if read_at.elapsed() > LATE {
            return copy("late");
        }
        let cache = match cache {
            Cache::Known(d) => d,
            Cache::Pending => return copy("cache-dir-pending"),
            Cache::Unusable(why) => return Outcome::Copy { reason: "cache-unusable".into(), fatal: true, message: Some(why) },
        };
        if self.unsupported.load(Ordering::Relaxed) {
            return Outcome::Copy { reason: "clone-unsupported".into(), fatal: true, message: None };
        }
        if let Err(e) = self.check_dest(dest) {
            return Outcome::Copy { reason: "rejected-dest".into(), fatal: false, message: Some(e) };
        }
        let meta = match self.check_src(src, &cache) {
            Ok(m) => m,
            Err(e) => return Outcome::Copy { reason: "rejected-src".into(), fatal: false, message: Some(e) },
        };
        // BSD file flags (compressed, hidden, immutable…) would be carried by
        // a clone, never by a copy.
        if sys::flags(&meta) != 0 {
            return copy("src-flags");
        }
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        if self.test.fail_every > 0 && n % self.test.fail_every == 0 {
            return copy("test-refused");
        }
        if let Err(e) = sys::clone(src, dest) {
            let no = errno(&e);
            if no == libc::EXDEV || no == libc::ENOTSUP || no == libc::EOPNOTSUPP || no == libc::ENOSYS {
                self.unsupported.store(true, Ordering::Relaxed);
                return Outcome::Copy { reason: "clone-unsupported".into(), fatal: true, message: Some(format!("clonefile {}: {e}", dest.display())) };
            }
            return Outcome::Copy { reason: format!("clone-errno-{no}"), fatal: false, message: Some(format!("clonefile {}: {e}", dest.display())) };
        }
        // (The test switch fails the fix-up before it starts: the clone still
        // has the cache file's xattrs and group, which a copy made over it
        // would keep.)
        let fixed = if self.test.fail_fixup_every > 0 && n % self.test.fail_fixup_every == 0 {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            fix_up(dest, &meta, facts)
        };
        if let Err(e) = fixed {
            // Not a copy's result: remove it; Node's copy makes the file.
            let _ = std::fs::remove_file(dest);
            return Outcome::Copy { reason: "fixup-failed".into(), fatal: false, message: Some(format!("{}: {e}", dest.display())) };
        }
        Outcome::Cloned { bytes: meta.len() }
    }

    fn tally(&self, o: &Outcome) {
        let mut s = self.stats.lock().unwrap();
        s.last = Some(Instant::now());
        match o {
            Outcome::Cloned { bytes } => {
                s.cloned += 1;
                s.cloned_bytes += bytes;
            }
            Outcome::Copy { reason, message, .. } => {
                *s.copied.entry(reason.clone()).or_default() += 1;
                if let Some(m) = message {
                    if s.errors.len() < 5 {
                        s.errors.push(format!("{reason}: {m}"));
                    }
                }
            }
        }
    }

    /// A message that needs no answer. Returns false when `v` is something else.
    fn record_message(&self, v: &Value) -> bool {
        if v.get("hello").is_some() {
            if let Some(f) = v.get("facts").and_then(Facts::from_json) {
                let _ = self.facts.set(f);
            }
            self.stats.lock().unwrap().hello = Some(v.clone());
            return true;
        }
        if let Some(why) = v.get("declined").and_then(|d| d.as_str()) {
            self.stats.lock().unwrap().declined = Some(why.chars().take(300).collect());
            return true;
        }
        if let Some(step) = v.get("link_step").and_then(|s| s.as_str()) {
            let mut s = self.stats.lock().unwrap();
            if step == "start" {
                s.link_steps += 1;
            } else {
                s.link_step_ends += 1;
                if let Some(m) = v.get("counts").and_then(|c| c.as_object()) {
                    for (k, n) in m {
                        // A short word chosen by the hook; keep the log sane.
                        let key: String = k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
                        *s.copied.entry(key).or_default() += n.as_u64().unwrap_or(0);
                    }
                }
            }
            return true;
        }
        false
    }

    fn handle_line(&self, line: &str, read_at: Instant) -> Vec<String> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let Some(id) = v.get("id").cloned() else { return vec![] };
        if let Some(p) = v.get("probe").and_then(|p| p.as_array()) {
            let path = |i: usize| p.get(i).and_then(|x| x.as_str()).map(PathBuf::from);
            let shares = self.test.probe_shares || matches!((path(0), path(1)), (Some(a), Some(b)) if sys::shares_blocks(&a, &b));
            return vec![json!({ "id": id, "shares": shares }).to_string()];
        }
        self.stats.lock().unwrap().first.get_or_insert_with(Instant::now);
        let str_of = |k: &str| v.get(k).and_then(|x| x.as_str());
        let outcome = match (str_of("src"), str_of("dest")) {
            (Some(src), Some(dest)) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.clone_file(Path::new(src), Path::new(dest), read_at)))
                .unwrap_or_else(|_| copy("internal-error")),
            _ => copy("bad-request"),
        };
        self.tally(&outcome);
        let answer = match outcome {
            Outcome::Cloned { .. } => json!({ "id": id, "ok": true }),
            Outcome::Copy { reason, fatal, .. } => json!({ "id": id, "ok": false, "reason": reason, "fatal": fatal }),
        };
        vec![answer.to_string()]
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    pub fn serve(&self, stream: UnixStream, threads: usize) -> Result<()> {
        serve_lines(stream, threads, &self.stopping, &|v| self.record_message(v), &|l, t| self.handle_line(l, t))
    }
}

/// Make the clone at `dest` look like the copy Node's `fs.copyFile` leaves
/// (module comment). `src_meta`: the cache file's lstat.
fn fix_up(dest: &Path, src_meta: &std::fs::Metadata, facts: Facts) -> std::io::Result<()> {
    if facts.group == Some(true) {
        let m = std::fs::symlink_metadata(dest)?;
        if m.uid() != src_meta.uid() || m.gid() != src_meta.gid() {
            // As libuv's fchown(dst, src uid, src gid): the result is ignored
            // (a group the user is not in stays the directory's).
            sys::lchown(dest, src_meta.uid(), src_meta.gid());
        }
    }
    for name in crate::diff::xattr_names(dest)? {
        if name != "com.apple.provenance" {
            sys::remove_xattr(dest, &name)?;
        }
    }
    let (atime, mtime) = facts.kept(src_meta.len());
    if !(atime && mtime) {
        sys::set_times_now(dest, !atime, !mtime)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    const CLONE_NOFOLLOW: u32 = 0x0001;
    /// The clone is owned by the caller, its group the directory's (as a
    /// file Node creates).
    const CLONE_NOOWNERCOPY: u32 = 0x0002;

    fn c(p: &Path) -> io::Result<CString> {
        Ok(CString::new(p.as_os_str().as_bytes())?)
    }

    pub fn clone(src: &Path, dst: &Path) -> io::Result<()> {
        let (s, d) = (c(src)?, c(dst)?);
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn flags(m: &std::fs::Metadata) -> u32 {
        use std::os::macos::fs::MetadataExt;
        m.st_flags()
    }

    pub fn lchown(p: &Path, uid: u32, gid: u32) {
        if let Ok(cp) = c(p) {
            unsafe { libc::lchown(cp.as_ptr(), uid, gid) };
        }
    }

    pub fn remove_xattr(p: &Path, name: &str) -> io::Result<()> {
        let (cp, n) = (c(p)?, CString::new(name)?);
        if unsafe { libc::removexattr(cp.as_ptr(), n.as_ptr(), libc::XATTR_NOFOLLOW) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn set_times_now(p: &Path, atime: bool, mtime: bool) -> io::Result<()> {
        let t = |now: bool| libc::timespec { tv_sec: 0, tv_nsec: if now { libc::UTIME_NOW } else { libc::UTIME_OMIT } };
        let ts = [t(atime), t(mtime)];
        let cp = c(p)?;
        if unsafe { libc::utimensat(libc::AT_FDCWD, cp.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// The device offset of a file's first block (F_LOG2PHYS_EXT).
    fn first_block(p: &Path) -> io::Result<i64> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(p)?;
        let mut l = libc::log2phys { l2p_flags: 0, l2p_contigbytes: 1, l2p_devoffset: 0 };
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut l) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(l.l2p_devoffset)
    }

    /// The two files share their first block (one is a clone of the other).
    pub fn shares_blocks(a: &Path, b: &Path) -> bool {
        let same_dev = matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if x.dev() == y.dev());
        same_dev && matches!((first_block(a), first_block(b)), (Ok(x), Ok(y)) if x == y)
    }
}

/// Elsewhere the engine never runs (plain Yarn already clones on Linux: its
/// libuv uses FICLONE); this keeps the module building and testable.
#[cfg(not(target_os = "macos"))]
mod sys {
    use std::io;
    use std::path::Path;

    pub fn clone(_src: &Path, _dst: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    pub fn flags(_m: &std::fs::Metadata) -> u32 {
        0
    }

    pub fn lchown(_p: &Path, _uid: u32, _gid: u32) {}

    pub fn remove_xattr(_p: &Path, _name: &str) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    pub fn set_times_now(_p: &Path, _atime: bool, _mtime: bool) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    pub fn shares_blocks(_a: &Path, _b: &Path) -> bool {
        false
    }
}

/// How a `matchapack yarn` install ended when Yarn did not succeed.
enum RunError {
    /// Refused before Yarn started: plain Yarn can take over as is.
    Declined(String),
    /// Yarn ran and exited with this status, having said why itself.
    Yarn { code: i32, stats: Value },
}

pub struct Summary {
    pub secs: f64,
    pub stats: Value,
    pub unseen: bool,
}

fn now_mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok()
}

/// `yarn cache dir` as the user's Yarn answers it for this install.
fn yarn_cache_dir(node: &Path, yarn_js: &Path, cwd: &Path, args: &[String]) -> std::result::Result<PathBuf, String> {
    let out = std::process::Command::new(node)
        .arg(yarn_js)
        .arg("cache")
        .arg("dir")
        .args(cache_args(args))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("yarn cache dir: {e}"))?;
    if !out.status.success() {
        return Err(format!("yarn cache dir exited with {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
    if !Path::new(line).is_absolute() {
        return Err(format!("yarn cache dir printed {line:?}"));
    }
    Ok(PathBuf::from(line))
}

fn run(cwd: &Path, root: &Path, yarn_js: &Path, args: &[String]) -> std::result::Result<Summary, RunError> {
    let t0 = Instant::now();
    let node = PathBuf::from("node");
    let prepared = (|| -> Result<_> {
        let run_dir = util::private_run_dir()?;
        let helper = run_dir.join("yarn1-hook.js");
        let _ = std::fs::remove_file(&helper);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&helper)?;
            f.write_all(include_str!("yarn1-hook.js").as_bytes())?;
        }
        let (ours, theirs) = UnixStream::pair().context("creating the socket for the clone requests")?;
        Ok((run_dir, helper, ours, theirs))
    })();
    let (run_dir, helper, ours, theirs) = prepared.map_err(|e| RunError::Declined(format!("{e:#}")))?;
    let server = match Server::new(root.to_path_buf()) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&helper);
            return Err(RunError::Declined(format!("{e:#}")));
        }
    };
    let integrity = root.join("node_modules/.yarn-integrity");
    let integrity_before = now_mtime(&integrity);

    // Not util::node_cmd: Yarn runs here as the user's `yarn`, and the
    // scripts it starts see the user's environment.
    let mut cmd = std::process::Command::new(&node);
    cmd.arg(&helper)
        .arg(yarn_js)
        .args(args)
        .current_dir(cwd)
        .env("MATCHAPACK_YARN_FD", crate::inject::CHILD_FD.to_string())
        .env("MATCHAPACK_YARN_PROBE_DIR", &run_dir)
        .env_remove("MATCHAPACK_YARN_FAIL_EVERY")
        .env_remove("MATCHAPACK_YARN_FAIL_FIXUP_EVERY");
    let status = std::thread::scope(|s| {
        // Yarn's cache folder, asked of the same Yarn in parallel: until it
        // answers, every request is copied.
        s.spawn(|| server.set_cache(yarn_cache_dir(&node, yarn_js, cwd, args)));
        let serve = |sock| {
            let _ = server.serve(sock, crate::inject::threads());
        };
        crate::inject::run_serving(&mut cmd, ours, theirs, serve, || server.stop())
    });
    let _ = std::fs::remove_file(&helper);
    let status = match status {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(RunError::Yarn { code: 1, stats: json!({ "error": format!("waiting for node: {e}") }) });
        }
        Err(e) => return Err(RunError::Declined(format!("could not start node: {e}"))),
    };
    let stats = std::mem::take(&mut *server.stats.lock().unwrap());
    if stats.hello.is_none() && status.code() == Some(crate::inject::HELPER_DECLINED) {
        let why = stats.declined.clone().unwrap_or_else(|| "the hook could not start Yarn".into());
        return Err(RunError::Declined(why));
    }
    let mut out = stats.to_json();
    // Yarn wrote .yarn-integrity in this run (it linked), but the hook saw
    // no link step: the hook no longer fits this Yarn, or another Yarn ran.
    let linked = now_mtime(&integrity).is_some_and(|t| Some(t) != integrity_before);
    let unseen = linked && stats.link_steps == 0;
    out["linked"] = json!(linked);
    out["unseen"] = json!(unseen);
    if !status.success() {
        let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        return Err(RunError::Yarn { code, stats: out });
    }
    Ok(Summary { secs: t0.elapsed().as_secs_f64(), stats: out, unseen })
}

/// matchapack will not handle this install and nothing has been changed: run
/// plain Yarn (or exit 3 under MATCHAPACK_NO_FALLBACK=1, leaving the project
/// as it is).
fn decline(args: &[String], yarn_js: Option<&Path>, reason: &str) -> i32 {
    if std::env::var("MATCHAPACK_NO_FALLBACK").as_deref() == Ok("1") {
        eprintln!("matchapack: not handled, left unchanged ({reason})");
        return 3;
    }
    eprintln!("matchapack: running plain yarn instead ({reason})");
    exec_yarn(args, yarn_js)
}

/// Replace this process with plain Yarn: the yarn.js matchapack would have
/// run, else the `yarn` on PATH. `MATCHAPACK=0` keeps any wrapper from routing
/// back here.
fn exec_yarn(args: &[String], yarn_js: Option<&Path>) -> i32 {
    util::remove_run_dir();
    let mut cmd = match yarn_js {
        Some(js) => {
            let mut c = std::process::Command::new("node");
            c.arg(js);
            c
        }
        None => std::process::Command::new("yarn"),
    };
    let err = cmd.args(args).env("MATCHAPACK", "0").exec();
    eprintln!("matchapack: could not start yarn: {err}");
    127
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1048576.0)
}

/// `matchapack yarn <args>`: a bare `yarn` or `yarn install` with clones of
/// Yarn's cache; anything else, and every install a gate refuses, is plain
/// Yarn with the same arguments.
pub fn cli(args: Vec<String>) -> i32 {
    // On Linux plain Yarn already clones (its libuv uses FICLONE).
    if !cfg!(target_os = "macos") || std::env::var("MATCHAPACK").as_deref() == Ok("0") || !is_install(&args) {
        return exec_yarn(&args, None);
    }
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return exec_yarn(&args, None),
    };
    let found = find_yarn_js();
    let yarn_js = found.as_ref().ok().map(|(js, _)| js.clone());
    let yarn_version = found.as_ref().map(|(_, v)| v.clone()).unwrap_or_default();
    let log = |outcome: &str, extra: Value| {
        let mut rec = json!({ "outcome": outcome, "engine": "yarn1", "dir": cwd.display().to_string(), "yarn": yarn_version, "args": args });
        if let (Value::Object(m), Value::Object(x)) = (&mut rec, extra) {
            m.extend(x);
        }
        trial::append(rec);
    };
    let checked = found.and_then(|(js, _)| gates(&cwd, &args).map(|root| (js, root)));
    let (yarn_js_path, root) = match checked {
        Ok(x) => x,
        Err(reason) => {
            log("declined", json!({ "reason": reason }));
            return decline(&args, yarn_js.as_deref(), &reason);
        }
    };
    util::set_quiet(true);
    eprintln!("matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&cwd, &root, &yarn_js_path, &args)));
    util::remove_run_dir();
    match outcome {
        Ok(Ok(s)) => {
            let n = |k: &str| s.stats[k].as_u64().unwrap_or(0);
            if s.stats["linked"] == true || n("cloned") > 0 {
                eprintln!(
                    "matchapack: {} files cloned from Yarn's cache ({}), {} copied by Node, in {:.1}s",
                    n("cloned"),
                    mib(n("cloned_bytes")),
                    n("copied_total"),
                    s.secs
                );
            }
            if let Some(why) = s.stats["cache"].as_str().and_then(|c| c.strip_prefix("unusable: ")) {
                eprintln!("matchapack: no clones in this install: {why}");
            }
            if s.unseen {
                eprintln!(
                    "matchapack: Yarn linked node_modules but matchapack's hook never saw its link step: \
                     this Yarn may run another way (the install is Yarn's own and complete; nothing was cloned)"
                );
            }
            log("installed", json!({ "secs": s.secs, "yarn1": s.stats }));
            0
        }
        Ok(Err(RunError::Declined(reason))) => {
            log("declined", json!({ "reason": reason }));
            decline(&args, Some(&yarn_js_path), &reason)
        }
        // Yarn ran and failed, and has said why: its status is the answer
        // (plain Yarn would have failed the same way; nothing is retried).
        Ok(Err(RunError::Yarn { code, stats })) => {
            log("yarn_failed", json!({ "code": code, "reason": format!("yarn exited with status {code}"), "yarn1": stats }));
            code
        }
        Err(_) => {
            // Yarn may have run: never start it twice.
            log("broke", json!({ "reason": "internal error (panic)" }));
            eprintln!("matchapack: internal error while yarn was running; run `MATCHAPACK=0 yarn` to install again");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_command_is_found_as_yarn_finds_it() {
        assert_eq!(command(&s(&[])), "");
        assert_eq!(command(&s(&["install", "--frozen-lockfile"])), "install");
        assert_eq!(command(&s(&["--frozen-lockfile"])), "");
        assert_eq!(command(&s(&["--cache-folder", "/c", "--frozen-lockfile"])), "");
        assert_eq!(command(&s(&["--cache-folder=/c", "add", "x"])), "add");
        assert_eq!(command(&s(&["--cache-folder", "/c", "add", "x"])), "add");
        // An optional value binds the next word too (`--production [prod]`).
        assert_eq!(command(&s(&["--production", "install"])), "");
        assert_eq!(command(&s(&["run", "build"])), "run");
        for a in [vec![], vec!["install"], vec!["--frozen-lockfile", "--cache-folder", "/c"], vec!["install", "--production"]] {
            assert!(is_install(&s(&a)), "{a:?}");
        }
        for a in [vec!["add", "x"], vec!["-v"], vec!["--version"], vec!["install", "--help"], vec!["global", "add", "x"], vec!["run", "install"]] {
            assert!(!is_install(&s(&a)), "{a:?}");
        }
    }

    #[test]
    fn cache_arguments_are_passed_to_cache_dir() {
        assert_eq!(cache_args(&s(&["install", "--cache-folder", "/c", "--frozen-lockfile"])), s(&["--cache-folder", "/c"]));
        assert_eq!(cache_args(&s(&["--cache-folder=/c", "--no-default-rc", "--use-yarnrc", "r"])), s(&["--cache-folder=/c", "--no-default-rc", "--use-yarnrc", "r"]));
        assert_eq!(cache_args(&s(&["--preferred-cache-folder", "/p", "--offline"])), s(&["--preferred-cache-folder", "/p"]));
        assert!(cache_args(&s(&["--frozen-lockfile"])).is_empty());
    }

    #[test]
    fn yarn_path_lines() {
        assert!(sets_yarn_path("yarn-path \".yarn/releases/yarn-1.22.22.js\"\n"));
        assert!(sets_yarn_path("# c\n  \"yarn-path\" \"x.js\"\n"));
        assert!(!sets_yarn_path("yarn-offline-mirror \"./m\"\n# yarn-path x\n"));
        assert!(!sets_yarn_path("cache-folder /c\n"));
    }

    #[test]
    fn facts_from_the_hello() {
        let f = Facts::from_json(&json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": true })).unwrap();
        assert_eq!(f.kept(0), (true, true));
        assert_eq!(f.kept(1), (true, false), "a non-empty copy is written after its times are set: mtime is now");
        let f20 = Facts::from_json(&json!({ "empty_atime": false, "empty_mtime": false, "atime": false, "mtime": false, "group": false })).unwrap();
        assert_eq!(f20.kept(0), (false, false));
        assert_eq!(Facts::from_json(&json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null })).unwrap().group, None);
        assert!(Facts::from_json(&json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": true })).is_none(), "a copy that keeps mtimes: no facts, no clones");
        assert!(Facts::from_json(&json!({ "atime": true })).is_none());
    }

    struct Fx {
        dir: PathBuf,
        root: PathBuf,
        cache: PathBuf,
    }

    /// A project (`p`, with node_modules/a) and a Yarn cache (`cache/v6`,
    /// one entry), side by side in a scratch directory.
    fn fx(name: &str) -> Fx {
        let dir = std::fs::canonicalize(crate::store::tests::scratch(name)).unwrap();
        let root = dir.join("p");
        let cache = dir.join("cache/v6");
        std::fs::create_dir_all(root.join("node_modules/a/lib")).unwrap();
        std::fs::write(root.join("yarn.lock"), "").unwrap();
        let entry = cache.join("npm-a-1.0.0-abc-integrity/node_modules/a");
        std::fs::create_dir_all(entry.join("lib")).unwrap();
        std::fs::write(entry.join("index.js"), "x").unwrap();
        std::fs::write(entry.join("lib/empty.js"), "").unwrap();
        Fx { dir, root, cache }
    }

    fn server(f: &Fx) -> Server {
        let s = Server::new(f.root.clone()).unwrap();
        s.set_cache(Ok(f.cache.clone()));
        s
    }

    fn src(f: &Fx) -> PathBuf {
        f.cache.join("npm-a-1.0.0-abc-integrity/node_modules/a/index.js")
    }

    #[test]
    fn destinations_must_be_in_a_node_modules_of_the_project() {
        let f = fx("y1-dest");
        let s = server(&f);
        assert!(s.check_dest(&f.root.join("node_modules/a/index.js")).is_ok());
        assert!(s.check_dest(&f.root.join("node_modules/a/lib/x.js")).is_ok());
        // Outside the project, beside it, without node_modules, not plain.
        assert!(s.check_dest(&f.dir.join("elsewhere/node_modules/a/index.js")).is_err());
        std::fs::create_dir_all(f.root.join("src")).unwrap();
        let e = s.check_dest(&f.root.join("src/index.js")).unwrap_err();
        assert!(e.contains("not inside a node_modules"), "{e}");
        assert!(s.check_dest(&f.root.join("node_modules/a/../../x.js")).is_err());
        assert!(s.check_dest(Path::new("node_modules/a/index.js")).is_err());
        // A parent that is a symlink out of the project.
        let out = f.dir.join("outside/node_modules/q");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&out, f.root.join("node_modules/q")).unwrap();
        let e = s.check_dest(&f.root.join("node_modules/q/index.js")).unwrap_err();
        assert!(e.contains("resolves outside the project"), "{e}");
        // The verdict is kept per parent directory.
        assert!(s.dest_dirs.lock().unwrap().contains_key(&f.root.join("node_modules/q")));
    }

    #[test]
    fn sources_must_be_regular_files_inside_the_cache() {
        let f = fx("y1-src");
        let s = server(&f);
        let cache = std::fs::canonicalize(&f.cache).unwrap();
        assert!(s.check_src(&src(&f), &cache).is_ok());
        // Outside the cache.
        let other = f.dir.join("other.js");
        std::fs::write(&other, "x").unwrap();
        let e = s.check_src(&other, &cache).unwrap_err();
        assert!(e.contains("outside Yarn's cache"), "{e}");
        // A symlink as the source (into the cache, even).
        let link = src(&f).with_file_name("link.js");
        std::os::unix::fs::symlink(src(&f), &link).unwrap();
        let e = s.check_src(&link, &cache).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        // A directory; a parent that is a symlink out of the cache.
        assert!(s.check_src(&src(&f).with_file_name("lib"), &cache).is_err());
        std::os::unix::fs::symlink(&f.dir, f.cache.join("escape")).unwrap();
        let e = s.check_src(&f.cache.join("escape/other.js"), &cache).unwrap_err();
        assert!(e.contains("outside Yarn's cache"), "{e}");
        assert!(s.check_src(Path::new("relative.js"), &cache).is_err());
    }

    fn ask(s: &Server, src: &Path, dest: &Path) -> Value {
        let line = json!({ "id": 1, "src": src, "dest": dest }).to_string();
        serde_json::from_str(&s.handle_line(&line, Instant::now())[0]).unwrap()
    }

    fn hello(s: &Server, facts: Value) {
        assert!(s.record_message(&json!({ "hello": true, "first": 128, "facts": facts })));
    }

    #[test]
    fn requests_are_copied_until_the_hello_and_the_cache_are_known() {
        let f = fx("y1-pending");
        let mut s = Server::new(f.root.clone()).unwrap();
        s.cache_wait = Duration::from_millis(50);
        let dest = f.root.join("node_modules/a/index.js");
        assert_eq!(ask(&s, &src(&f), &dest)["reason"], "no-hello");
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["ok"].clone(), a["reason"].clone(), a["fatal"].clone()), (json!(false), json!("cache-dir-pending"), json!(false)));
        s.set_cache(Err("yarn cache dir exited with 1".into()));
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["reason"].clone(), a["fatal"].clone()), (json!("cache-unusable"), json!(true)));
        assert!(!dest.exists());
        let st = s.stats.lock().unwrap();
        assert_eq!(st.copied.get("cache-dir-pending"), Some(&1));
        assert_eq!(st.cloned, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_request_waits_for_the_cache_folder() {
        let f = fx("y1-wait");
        let s = Server::new(f.root.clone()).unwrap();
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let dest = f.root.join("node_modules/a/index.js");
        let a = std::thread::scope(|sc| {
            let asked = sc.spawn(|| ask(&s, &src(&f), &dest));
            std::thread::sleep(Duration::from_millis(200));
            s.set_cache(Ok(f.cache.clone()));
            asked.join().unwrap()
        });
        assert_eq!(a["ok"], true, "{a}");
        assert!(dest.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_clone_gets_the_times_and_attributes_a_copy_would() {
        use std::os::unix::fs::MetadataExt;
        let f = fx("y1-clone");
        let s = server(&f);
        // Node >= 22's copy: empty files keep both times, others the atime.
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": true }));
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let set_old = |p: &Path| {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_times(std::fs::FileTimes::new().set_accessed(old).set_modified(old)).unwrap();
        };
        let empty_src = src(&f).with_file_name("lib/empty.js");
        set_old(&src(&f));
        set_old(&empty_src);
        let named = |p: &Path, n: &str| {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
            let k = std::ffi::CString::new(n).unwrap();
            assert_eq!(unsafe { libc::setxattr(c.as_ptr(), k.as_ptr(), b"1".as_ptr() as *const _, 1, 0, 0) }, 0);
        };
        named(&src(&f), "user.matchapack.test");
        let t = Instant::now();
        let dest = f.root.join("node_modules/a/index.js");
        assert_eq!(ask(&s, &src(&f), &dest)["ok"], true);
        let m = std::fs::symlink_metadata(&dest).unwrap();
        assert_eq!(m.accessed().unwrap(), old, "atime kept");
        assert!(m.modified().unwrap() > old, "mtime now");
        assert_eq!(crate::diff::xattr_names(&dest).unwrap().iter().filter(|n| *n != "com.apple.provenance").count(), 0, "extra xattr removed");
        assert_eq!(m.gid(), std::fs::metadata(src(&f)).unwrap().gid());
        let edest = f.root.join("node_modules/a/lib/empty.js");
        assert_eq!(ask(&s, &empty_src, &edest)["ok"], true);
        let m = std::fs::symlink_metadata(&edest).unwrap();
        assert_eq!((m.accessed().unwrap(), m.modified().unwrap()), (old, old), "an empty clone keeps both, as Node's copy");
        assert!(sys::shares_blocks(&src(&f), &dest));
        let st = s.stats.lock().unwrap();
        assert_eq!((st.cloned, st.cloned_bytes), (2, 1));
        assert!(t.elapsed() < LATE);
        drop(st);
        // Node 20's copy: nothing kept.
        let f2 = fx("y1-clone20");
        let s2 = server(&f2);
        hello(&s2, json!({ "empty_atime": false, "empty_mtime": false, "atime": false, "mtime": false, "group": false }));
        set_old(&src(&f2).with_file_name("lib/empty.js"));
        let edest = f2.root.join("node_modules/a/lib/empty.js");
        assert_eq!(ask(&s2, &src(&f2).with_file_name("lib/empty.js"), &edest)["ok"], true);
        let m = std::fs::symlink_metadata(&edest).unwrap();
        assert!(m.accessed().unwrap() > old && m.modified().unwrap() > old);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn failed_clones_and_fix_ups_leave_nothing_for_nodes_copy() {
        let f = fx("y1-fail");
        let s = server(&f);
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        // The destination exists: clonefile refuses (EEXIST), not fatal.
        let dest = f.root.join("node_modules/a/index.js");
        std::fs::write(&dest, "old").unwrap();
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["ok"].clone(), a["fatal"].clone()), (json!(false), json!(false)));
        assert_eq!(a["reason"], format!("clone-errno-{}", libc::EEXIST));
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        std::fs::remove_file(&dest).unwrap();
        // A fix-up that fails: the clone is removed.
        let mut s = s;
        s.test.fail_fixup_every = 1;
        let a = ask(&s, &src(&f), &dest);
        assert_eq!(a["reason"], "fixup-failed");
        assert!(!dest.exists(), "the clone was removed");
        // A request that waited too long is not cloned.
        let line = json!({ "id": 2, "src": src(&f), "dest": dest }).to_string();
        let a: Value = serde_json::from_str(&s.handle_line(&line, Instant::now() - LATE - Duration::from_secs(1))[0]).unwrap();
        assert_eq!(a["reason"], "late");
        assert!(!dest.exists());
        // A cache on another volume: fatal.
        let s3 = Server::new(f.root.clone()).unwrap();
        s3.set_cache(Ok(PathBuf::from("/dev")));
        hello(&s3, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let a = ask(&s3, &src(&f), &dest);
        assert_eq!((a["reason"].clone(), a["fatal"].clone()), (json!("cache-unusable"), json!(true)));
        assert!(s3.stats.lock().unwrap().cache.as_deref().is_some_and(|c| c.contains("another volume")));
    }

    #[test]
    fn link_step_messages_and_counts_are_recorded() {
        let f = fx("y1-msg");
        let s = server(&f);
        assert!(s.record_message(&json!({ "link_step": "start" })));
        assert!(s.record_message(&json!({ "link_step": "end", "counts": { "first-copies": 128, "not-eligible": 2, "x y\n": 1 } })));
        assert!(s.record_message(&json!({ "declined": "why" })));
        assert!(!s.record_message(&json!({ "id": 1 })));
        let st = s.stats.lock().unwrap();
        assert_eq!((st.link_steps, st.link_step_ends), (1, 1));
        assert_eq!(st.copied.get("first-copies"), Some(&128));
        assert_eq!(st.copied.get("xy"), Some(&1));
        assert_eq!(st.declined.as_deref(), Some("why"));
    }
}
