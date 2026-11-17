//! `matchapack uninstall`: remove matchapack's own state (the store, the
//! trial logs, the settings directory), and say what else to remove by hand
//! (the shell integration line, the binary). Without `--yes` it only prints
//! what it would do and writes nothing.
//!
//! It deletes directories, so it deletes only what it can recognise as
//! matchapack's:
//! - the three directories are resolved by the code the rest of the tool
//!   uses (`default_store`, `trial::cache_dir`, `util::config_dir`), then
//!   normalised (a trailing `/` and `.` dropped); a path that is relative,
//!   holds `..`, is a symbolic link, is `/`, or is the home directory or one
//!   of its parents is refused;
//! - the store must hold the layout directory (`store::LAYOUT`), or only
//!   what a store the Yarn engine alone has used holds (`tmp/` with
//!   matchapack's run directories, `lock`, `projects`);
//! - inside each directory only the names matchapack makes are removed. A
//!   directory a MATCHAPACK_* variable names that holds anything else is
//!   refused as a whole (it may not be matchapack's); in a default directory
//!   other names are left in place, and the directory with them;
//! - each directory is opened once (O_NOFOLLOW | O_DIRECTORY) and must be
//!   the directory planned (device and inode); everything is removed
//!   relative to that descriptor (openat / unlinkat), each subdirectory
//!   opened the same way and checked against what was looked at, so no
//!   symbolic link is followed and a path swapped after the plan is not
//!   entered;
//! - the store is moved aside (renamed in its parent) while uninstall holds
//!   its lock exclusively, before anything in it is removed: an install that
//!   waited on the lock then finds the lock file gone from the store's path
//!   and opens a fresh store there (`Store::open`);
//! - it refuses while an install holds the store lock, or while any
//!   matchapack process holds a directory in the store's `tmp/` (the Yarn
//!   engine's helper files live there and take no store lock).
//!
//! Any refusal found before the removal starts removes nothing at all.

use crate::store::{self, Integrity};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// Top-level names in a store: the current layout and every name a build
/// has made there (git history of src/store.rs).
const STORE_NAMES: &[&str] = &[store::LAYOUT, "v1", "tmp", "lock", "sizes", "made-by", "locks", "declined", "declined-keys", "projects"];
/// What a store holds that only the Yarn engine has used (its run
/// directories are made under `tmp/`; `lock` and `projects` by any
/// `Store::open`): no layout directory.
const YARN_ONLY_STORE_NAMES: &[&str] = &["tmp", "lock", "projects"];
/// The settings directory's files (trial scope; `npm-verified`: older builds).
const CONFIG_NAMES: &[&str] = &["paths", "npm-verified"];

/// A file's identity: (device, inode).
pub type Id = (u64, u64);

/// Directory operations relative to an open directory, never following a
/// symbolic link at the name given.
mod at {
    use super::Id;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn c(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL byte in a name"))
    }

    fn check(r: libc::c_int) -> io::Result<libc::c_int> {
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
    }

    fn changed() -> io::Error {
        io::Error::other("it changed while it was being removed (not the directory looked at)")
    }

    /// Open the directory at `path`, following links in it (a parent of a
    /// target may be one, as the user configured it).
    pub fn open_path(path: &OsStr) -> io::Result<OwnedFd> {
        let p = c(path)?;
        let fd = check(unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Open the directory `name` in `dir`; a link there is refused (ELOOP /
    /// ENOTDIR). `dir` may be AT_FDCWD with an absolute `name`.
    pub fn open_dir(dir: RawFd, name: &OsStr) -> io::Result<OwnedFd> {
        let n = c(name)?;
        let fd = check(unsafe { libc::openat(dir, n.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Open the regular file `name` in `dir` for reading and writing, not
    /// creating it, not following a link.
    pub fn open_file(dir: RawFd, name: &OsStr) -> io::Result<std::fs::File> {
        let n = c(name)?;
        let fd = check(unsafe { libc::openat(dir, n.as_ptr(), libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC) })?;
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    fn id_of(st: &libc::stat) -> Id {
        (st.st_dev as u64, st.st_ino as u64)
    }

    pub fn fid(fd: RawFd) -> io::Result<Id> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstat(fd, &mut st) })?;
        Ok(id_of(&st))
    }

    /// The identity of `name` in `dir`, whether it is a directory, and
    /// whether a regular file; a link is neither.
    pub fn lstat(dir: RawFd, name: &OsStr) -> io::Result<(Id, bool, bool)> {
        let n = c(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatat(dir, n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        let fmt = st.st_mode & libc::S_IFMT;
        Ok((id_of(&st), fmt == libc::S_IFDIR, fmt == libc::S_IFREG))
    }

    /// The names in the open directory `dir` (not `.` and `..`).
    pub fn names(dir: RawFd) -> io::Result<Vec<OsString>> {
        let d = check(unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) })?;
        let dp = unsafe { libc::fdopendir(d) };
        if dp.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(d) };
            return Err(e);
        }
        unsafe { libc::rewinddir(dp) };
        let mut v = vec![];
        loop {
            let ent = unsafe { libc::readdir(dp) };
            if ent.is_null() {
                break;
            }
            let b = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if b != b"." && b != b".." {
                v.push(OsString::from_vec(b.to_vec()));
            }
        }
        unsafe { libc::closedir(dp) };
        Ok(v)
    }

    /// Remove `name` in `dir`: a directory with everything in it, each
    /// directory opened without following a link and checked to be what
    /// was looked at; anything else (a file, a link) by itself.
    pub fn remove(dir: RawFd, name: &OsStr) -> io::Result<()> {
        let (id, is_dir, _) = match lstat(dir, name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            r => r?,
        };
        let n = c(name)?;
        if is_dir {
            let sub = open_dir(dir, name)?;
            if fid(sub.as_raw_fd())? != id {
                return Err(changed());
            }
            for child in names(sub.as_raw_fd())? {
                remove(sub.as_raw_fd(), &child)?;
            }
            check(unsafe { libc::unlinkat(dir, n.as_ptr(), libc::AT_REMOVEDIR) })?;
        } else {
            check(unsafe { libc::unlinkat(dir, n.as_ptr(), 0) })?;
        }
        Ok(())
    }

    /// Remove the empty directory `name` in `dir` if it is still `id`.
    pub fn rmdir(dir: RawFd, name: &OsStr, id: Id) -> io::Result<()> {
        let (now, is_dir, _) = lstat(dir, name)?;
        if !is_dir || now != id {
            return Err(changed());
        }
        let n = c(name)?;
        check(unsafe { libc::unlinkat(dir, n.as_ptr(), libc::AT_REMOVEDIR) }).map(|_| ())
    }

    pub fn rename(dir: RawFd, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let (f, t) = (c(from)?, c(to)?);
        check(unsafe { libc::renameat(dir, f.as_ptr(), dir, t.as_ptr()) }).map(|_| ())
    }
}

/// Where matchapack keeps its state, resolved as the tool resolves it, and
/// where to look for what the user added by hand.
pub struct Env {
    pub home: PathBuf,
    pub store: PathBuf,
    pub store_set: bool,
    pub logs: PathBuf,
    pub logs_set: bool,
    pub config: PathBuf,
    pub config_set: bool,
    /// Shell startup files to search for matchapack's lines.
    pub rc_files: Vec<PathBuf>,
    /// The running binary, and cargo's bin directory.
    pub exe: Option<PathBuf>,
    pub cargo_bin: PathBuf,
}

impl Env {
    /// Refused without HOME: the default directories and the startup files
    /// are found from it.
    pub fn from_process() -> anyhow::Result<Env> {
        if std::env::var_os("HOME").is_none_or(|h| h.is_empty()) {
            anyhow::bail!("HOME is not set: uninstall finds matchapack's default directories and your shell startup files from it; set HOME and run it again");
        }
        let home = crate::home();
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let zdot = var("ZDOTDIR").unwrap_or_else(|| home.clone());
        let xdg = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        // ~/.zshenv first: zsh reads it before ZDOTDIR's files, and it is
        // where ZDOTDIR is usually set.
        let mut rc_files: Vec<PathBuf> = vec![home.join(".zshenv")];
        rc_files.extend([".zshenv", ".zprofile", ".zshrc", ".zlogin"].iter().map(|f| zdot.join(f)));
        rc_files.extend([".profile", ".bash_profile", ".bash_login", ".bashrc"].iter().map(|f| home.join(f)));
        rc_files.push(xdg.join("fish/config.fish"));
        let mut confd: Vec<PathBuf> = std::fs::read_dir(xdg.join("fish/conf.d"))
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "fish")).collect())
            .unwrap_or_default();
        confd.sort();
        rc_files.extend(confd);
        // fish's universal variables (`set -U MATCHAPACK_…`).
        rc_files.push(xdg.join("fish/fish_variables"));
        let mut seen = HashSet::new();
        rc_files.retain(|f| seen.insert(f.clone()));
        Ok(Env {
            store: crate::default_store(None),
            store_set: std::env::var_os("MATCHAPACK_STORE").is_some(),
            logs: crate::trial::cache_dir(),
            logs_set: std::env::var_os("MATCHAPACK_LOG_DIR").is_some(),
            config: crate::util::config_dir(),
            config_set: std::env::var_os("MATCHAPACK_CONFIG_DIR").is_some(),
            rc_files,
            exe: std::env::current_exe().ok(),
            cargo_bin: var("CARGO_HOME").unwrap_or_else(|| home.join(".cargo")).join("bin"),
            home,
        })
    }

    /// convert-existing.sh's backups: always under the home directory.
    fn backups(&self) -> PathBuf {
        self.home.join(".cache/matchapack/backup")
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Store,
    Logs,
    Config,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Store => "store",
            Kind::Logs => "trial logs",
            Kind::Config => "settings",
        }
    }
    fn var(self) -> &'static str {
        match self {
            Kind::Store => "MATCHAPACK_STORE",
            Kind::Logs => "MATCHAPACK_LOG_DIR",
            Kind::Config => "MATCHAPACK_CONFIG_DIR",
        }
    }
}

#[derive(Debug)]
pub enum State {
    Absent,
    Refused(String),
    /// `id`: the directory's identity when planned; `delete`: the names to
    /// remove, with their size on disk; `kept`: names left in place and why;
    /// `nested`: names that are another target (the default store is in the
    /// default log directory), with its kind.
    Remove { id: Id, delete: Vec<(OsString, u64)>, kept: Vec<(OsString, String)>, nested: Vec<(OsString, Kind)> },
}

#[derive(Debug)]
pub struct Target {
    pub kind: Kind,
    /// Normalised (no trailing `/`, no `.`), when it could be.
    pub path: PathBuf,
    pub set: bool,
    pub state: State,
}

/// `p` without a trailing `/` and `.` components, or why it is refused:
/// empty, relative, holding `..` (it would be resolved through links the
/// checks below do not see), or `/`.
pub fn normalise(p: &Path) -> Result<PathBuf, String> {
    if p.as_os_str().is_empty() {
        return Err("is empty".into());
    }
    if !p.is_absolute() {
        return Err("is not an absolute path".into());
    }
    if p.components().any(|c| c == Component::ParentDir) {
        return Err("contains `..`: name the directory without it".into());
    }
    let n: PathBuf = p.components().collect();
    if n.parent().is_none() {
        return Err("is /".into());
    }
    Ok(n)
}

/// Why `p` (normalised) must not be removed as a directory of matchapack's,
/// if it must not; None when it does not exist.
pub fn refuse_reason(p: &Path, home: &Path) -> Option<String> {
    if !p.is_absolute() {
        return Some("is not an absolute path".into());
    }
    let m = std::fs::symlink_metadata(p).ok()?;
    if m.file_type().is_symlink() {
        return Some("is a symbolic link (uninstall follows no link: remove the link, and what it points to if that is matchapack's, yourself)".into());
    }
    if !m.is_dir() {
        return Some("is not a directory".into());
    }
    let canon = match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(e) => return Some(format!("cannot be resolved ({e})")),
    };
    if canon.parent().is_none() {
        return Some("is /".into());
    }
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if home.starts_with(&canon) {
        return Some("is your home directory or contains it".into());
    }
    None
}

/// Size on disk of what is at `p`, following no link (not even `p`).
fn on_disk(p: &Path) -> u64 {
    walkdir::WalkDir::new(p).follow_root_links(false).into_iter().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.blocks() * 512).sum()
}

fn lid(p: &Path) -> Option<Id> {
    std::fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

/// `<pid>[-<nanos>]-run`, `-x<seq>-<hex>` or `-gc<seq>`: the directories
/// matchapack makes in a store's `tmp/` (store::process_tag).
fn is_tmp_name(n: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some((pid, mut rest)) = n.split_once('-') else { return false };
    if !digits(pid) {
        return false;
    }
    if let Some((nanos, r)) = rest.split_once('-') {
        if digits(nanos) {
            rest = r;
        }
    }
    if rest == "run" {
        return true;
    }
    if let Some(seq) = rest.strip_prefix("gc") {
        return digits(seq);
    }
    if let Some(x) = rest.strip_prefix('x') {
        let (seq, hex) = x.split_once('-').unwrap_or((x, ""));
        return digits(seq) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    false
}

/// Whether the store directory `path`, holding `names`, is matchapack's: it
/// has the layout directory, or holds only what a store the Yarn engine
/// alone has used holds, or nothing.
fn store_recognised(path: &Path, names: &[String]) -> bool {
    if std::fs::symlink_metadata(path.join(store::LAYOUT)).is_ok_and(|m| m.is_dir()) {
        return true;
    }
    names.iter().all(|n| YARN_ONLY_STORE_NAMES.contains(&n.as_str()))
        && std::fs::read_dir(path.join("tmp")).map(|d| d.flatten().all(|e| is_tmp_name(&e.file_name().to_string_lossy()))).unwrap_or(true)
}

/// What removing `path` (a `kind` directory) would do. `others`: the other
/// targets that are directories, by identity.
fn plan_one(kind: Kind, path: &Path, set: bool, env: &Env, others: &[(Id, Kind)], store_id: Option<Id>) -> Target {
    let path = match normalise(path) {
        Ok(p) => p,
        Err(why) => return Target { kind, path: path.to_path_buf(), set, state: State::Refused(why) },
    };
    let target = |state| Target { kind, path: path.clone(), set, state };
    let Some(id) = lid(&path) else { return target(State::Absent) };
    if let Some(why) = refuse_reason(&path, &env.home) {
        return target(State::Refused(why));
    }
    let mut names: Vec<OsString> = match std::fs::read_dir(&path) {
        Ok(d) => d.flatten().map(|e| e.file_name()).collect(),
        Err(e) => return target(State::Refused(format!("cannot be read ({e})"))),
    };
    names.sort();
    if kind == Kind::Store {
        let strs: Vec<String> = names.iter().map(|n| n.to_string_lossy().into_owned()).collect();
        if !store_recognised(&path, &strs) {
            return target(State::Refused(format!(
                "has no {}/ directory, and holds more than a store only the Yarn engine has used (tmp/ with matchapack's run directories, lock, projects): not recognisable as a matchapack store",
                store::LAYOUT
            )));
        }
    }
    let (mut delete, mut kept, mut nested, mut unknown) = (vec![], vec![], vec![], vec![]);
    for n in names {
        let p = path.join(&n);
        let pid = lid(&p);
        if let Some((_, k)) = others.iter().find(|(o, _)| Some(*o) == pid) {
            nested.push((n, *k));
            continue;
        }
        let s = n.to_string_lossy();
        let ours = match kind {
            Kind::Store => STORE_NAMES.contains(&s.as_ref()),
            Kind::Config => CONFIG_NAMES.contains(&s.as_ref()),
            Kind::Logs => {
                if s == "backup" && pid.is_some() && pid == lid(&env.backups()) {
                    kept.push((n, "convert-existing.sh's backups (below)".to_string()));
                    continue;
                }
                if s == "store" && pid != store_id {
                    kept.push((n, "a store at the default place, not the one MATCHAPACK_STORE names: unset MATCHAPACK_STORE and run uninstall again to remove it".to_string()));
                    continue;
                }
                s == "log.jsonl" || s == "verify.jsonl" || (s.starts_with("repair-") && s.ends_with(".list"))
            }
        };
        if ours {
            let size = on_disk(&p);
            delete.push((n, size));
        } else {
            unknown.push(n);
        }
    }
    if !unknown.is_empty() {
        let list = unknown.iter().map(|n| n.to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
        if set {
            return target(State::Refused(format!(
                "holds names matchapack does not make ({list}): it may not be matchapack's directory; remove matchapack's files in it yourself, or unset {} if it is not the one you used",
                kind.var()
            )));
        }
        kept.extend(unknown.into_iter().map(|n| (n, "not made by matchapack".to_string())));
    }
    target(State::Remove { id, delete, kept, nested })
}

pub fn plan(env: &Env) -> Vec<Target> {
    let all = [(Kind::Store, &env.store, env.store_set), (Kind::Logs, &env.logs, env.logs_set), (Kind::Config, &env.config, env.config_set)];
    // Identities, so that one directory spelled two ways is one directory.
    let ids: Vec<(Option<Id>, Kind)> = all
        .iter()
        .map(|(k, p, _)| (normalise(p).ok().filter(|n| std::fs::symlink_metadata(n).is_ok_and(|m| m.is_dir())).and_then(|n| lid(&n)), *k))
        .collect();
    let store_id = ids[0].0;
    all.iter()
        .map(|(k, p, set)| {
            let others: Vec<(Id, Kind)> = ids.iter().filter(|(_, o)| o != k).filter_map(|(i, o)| i.map(|i| (i, *o))).collect();
            plan_one(*k, p, *set, env, &others, store_id)
        })
        .collect()
}

/// Whether a matchapack process uses the store now.
pub enum Busy {
    Free(Option<std::fs::File>),
    Held(String),
    /// The lock could not be taken for another reason (no flock here).
    Unknown(String),
}

/// With the store open as `dir`: take its lock exclusively (as `store gc`
/// does), without creating it, and check that no process holds a directory
/// in `tmp/`. The returned file keeps the lock until it is dropped.
fn busy_at(dir: &impl AsRawFd, root: &Path) -> Busy {
    let d = dir.as_raw_fd();
    let lock = std::ffi::OsStr::new("lock");
    let f = match at::lstat(d, lock) {
        Err(_) => None,
        Ok((_, _, false)) => return Busy::Unknown(format!("{} is not a regular file", root.join("lock").display())),
        Ok(_) => match at::open_file(d, lock) {
            Ok(f) => match store::flock(&f, libc::LOCK_EX | libc::LOCK_NB) {
                Ok(()) => Some(f),
                Err(e) if store::would_block(&e) => return Busy::Held("an install is using the store (it holds the store lock)".into()),
                Err(e) => return Busy::Unknown(format!("cannot lock the store ({e})")),
            },
            Err(e) => return Busy::Unknown(format!("cannot open {} ({e})", root.join("lock").display())),
        },
    };
    if let Ok(tmp) = at::open_dir(d, std::ffi::OsStr::new("tmp")) {
        for name in at::names(tmp.as_raw_fd()).unwrap_or_default() {
            let Ok(sub) = at::open_dir(tmp.as_raw_fd(), &name) else { continue };
            let file = std::fs::File::from(sub);
            if let Err(err) = store::flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
                if store::would_block(&err) {
                    return Busy::Held(format!("a matchapack process is running (it holds {})", root.join("tmp").join(&name).display()));
                }
            }
        }
    }
    Busy::Free(f)
}

/// The store at `root` (normalised), opened without following a link, and
/// checked to be `id`.
fn store_busy(root: &Path, id: Id) -> Busy {
    match at::open_dir(libc::AT_FDCWD, root.as_os_str()) {
        Ok(fd) if at::fid(fd.as_raw_fd()).ok() == Some(id) => busy_at(&fd, root),
        Ok(_) => Busy::Unknown(format!("{} changed since it was checked", root.display())),
        Err(e) => Busy::Unknown(format!("cannot open {} ({e})", root.display())),
    }
}

/// The store's bytes on disk that projects recorded as still installed
/// share (their entries), and how many such projects: removing the store
/// frees roughly its size less this.
fn shared_with_projects(root: &Path) -> (u64, usize) {
    let mut live: HashSet<Integrity> = HashSet::new();
    let mut projects = 0;
    for ent in std::fs::read_dir(root.join("projects")).into_iter().flatten().flatten() {
        let Some(rec) = std::fs::read_to_string(ent.path()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { continue };
        let Some(dir) = rec.get("path").and_then(|x| x.as_str()).map(PathBuf::from) else { continue };
        if crate::gc::has_node_modules(&dir) {
            projects += 1;
            live.extend(rec.get("entries").and_then(|e| e.as_array()).into_iter().flatten().filter_map(|k| k.as_str().and_then(Integrity::from_key)));
        }
    }
    let bytes = live.iter().map(|i| store::entry_dir(root, i)).filter(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir())).map(|d| on_disk(&d)).sum();
    (bytes, projects)
}

fn size(b: u64) -> String {
    const K: f64 = 1024.0;
    let b = b as f64;
    if b >= K * K * K {
        format!("{:.2} GiB", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.1} MiB", b / (K * K))
    } else if b >= K {
        format!("{:.0} KiB", b / K)
    } else {
        format!("{b} B")
    }
}

/// The lines of the user's startup files that mention matchapack.
pub fn rc_lines(files: &[PathBuf]) -> Vec<(PathBuf, usize, String)> {
    let mut out = vec![];
    for f in files {
        let Ok(text) = std::fs::read(f) else { continue };
        for (i, line) in String::from_utf8_lossy(&text).lines().enumerate() {
            if line.to_ascii_lowercase().contains("matchapack") {
                out.push((f.clone(), i + 1, line.to_string()));
            }
        }
    }
    out
}

/// A target opened for removal: its parent, its name there, and itself,
/// checked to be the directory planned.
struct Opened {
    parent: std::os::fd::OwnedFd,
    name: OsString,
    dir: std::os::fd::OwnedFd,
}

fn open_target(path: &Path, id: Id) -> Result<Opened, String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Err("has no parent".into()) };
    let parent = at::open_path(parent.as_os_str()).map_err(|e| format!("its parent cannot be opened ({e})"))?;
    match at::lstat(parent.as_raw_fd(), name) {
        Ok((now, true, _)) if now == id => {}
        Ok(_) => return Err("changed since it was checked (another directory, or a link, is there now): left alone".into()),
        Err(e) => return Err(format!("cannot be looked at ({e})")),
    }
    let dir = at::open_dir(parent.as_raw_fd(), name).map_err(|e| format!("cannot be opened without following a link ({e}): left alone"))?;
    if at::fid(dir.as_raw_fd()).ok() != Some(id) {
        return Err("changed since it was checked: left alone".into());
    }
    Ok(Opened { parent, name: name.to_os_string(), dir })
}

/// Remove `delete` (names) in the opened directory; `last` goes after the
/// others. Returns the failures.
fn remove_names(o: &Opened, path: &Path, delete: &[(OsString, u64)], last: &str) -> Vec<String> {
    let mut failed = vec![];
    let mut order: Vec<&OsString> = delete.iter().map(|d| &d.0).filter(|n| n.as_os_str() != last).collect();
    order.extend(delete.iter().map(|d| &d.0).filter(|n| n.as_os_str() == last));
    for n in order {
        if let Err(e) = at::remove(o.dir.as_raw_fd(), n) {
            failed.push(format!("{}: {e}", path.join(n).display()));
        }
    }
    failed
}

/// Remove the store: under its exclusive lock, moved aside first, then its
/// names through the descriptor. Ok: the failures (empty when all went);
/// Err: refused before anything was removed.
fn remove_store(t: &Target, id: Id, delete: &[(OsString, u64)], keep_some: bool, out: &mut dyn Write) -> std::io::Result<Result<Vec<String>, String>> {
    #[cfg(test)]
    tests::hook(&t.path, "before-open");
    let o = match open_target(&t.path, id) {
        Ok(o) => o,
        Err(why) => return Ok(Err(format!("{}: {why}", t.path.display()))),
    };
    let lock = match busy_at(&o.dir, &t.path) {
        Busy::Free(f) => f,
        Busy::Held(why) => return Ok(Err(format!("{why}; run `matchapack uninstall` again when it has finished"))),
        Busy::Unknown(why) => return Ok(Err(format!("{why}: refusing to remove the store without its lock"))),
    };
    // Moved aside while locked: an install waiting on the lock finds no
    // lock file at the store's path once it gets the lock, and opens a
    // fresh store there (Store::open), instead of using this one while it
    // is emptied.
    let aside = OsString::from(format!(".{}.uninstall-{}", o.name.to_string_lossy(), std::process::id()));
    if let Err(e) = at::rename(o.parent.as_raw_fd(), &o.name, &aside) {
        return Ok(Err(format!("{}: cannot be moved aside ({e})", t.path.display())));
    }
    if at::lstat(o.parent.as_raw_fd(), &aside).ok().map(|s| s.0) != Some(id) {
        let _ = at::rename(o.parent.as_raw_fd(), &aside, &o.name);
        return Ok(Err(format!("{}: changed while it was moved aside: left alone", t.path.display())));
    }
    #[cfg(test)]
    tests::hook(&t.path, "after-lock");
    let aside_path = t.path.with_file_name(&aside);
    let mut failed = remove_names(&o, &aside_path, delete, "lock");
    drop(lock);
    if !keep_some && failed.is_empty() {
        if let Err(e) = at::rmdir(o.parent.as_raw_fd(), &aside, id) {
            failed.push(format!("{}: {e}", aside_path.display()));
        }
    } else if at::lstat(o.parent.as_raw_fd(), &o.name).is_err() {
        // What is left (names not matchapack's, or what failed) goes back.
        let _ = at::rename(o.parent.as_raw_fd(), &aside, &o.name);
    }
    if at::lstat(o.parent.as_raw_fd(), &aside).is_ok() {
        writeln!(out, "  what is left of the store is in {}", aside_path.display())?;
    }
    Ok(Ok(failed))
}

/// Run `matchapack uninstall`; returns the exit status. With `yes` false
/// nothing is written.
pub fn run(env: &Env, yes: bool, out: &mut dyn Write) -> std::io::Result<i32> {
    let targets = plan(env);
    execute(env, &targets, yes, out)
}

/// Print `targets` (from `plan`) and, with `yes`, remove them.
pub fn execute(env: &Env, targets: &[Target], yes: bool, out: &mut dyn Write) -> std::io::Result<i32> {
    let store_t = &targets[0];
    let store_plan = match &store_t.state {
        State::Remove { id, delete, kept, .. } => Some((*id, delete, !kept.is_empty())),
        _ => None,
    };
    let refused = targets.iter().any(|t| matches!(t.state, State::Refused(_)));
    let busy = store_plan.as_ref().map(|(id, _, _)| store_busy(&store_t.path, *id));
    let busy_why = match &busy {
        Some(Busy::Held(why)) => Some(format!("{why}: {}; run `matchapack uninstall` again when it has finished.", if yes { "refused, nothing was removed" } else { "--yes refuses until then" })),
        Some(Busy::Unknown(why)) => Some(format!("{why}: {}", if yes { "refusing to remove the store without its lock; nothing was removed" } else { "--yes would refuse to remove the store without its lock" })),
        _ => None,
    };
    // The dry run's lock attempt ends here.
    drop(busy);
    let removing = yes && !refused && busy_why.is_none();

    if removing {
        writeln!(out, "matchapack uninstall: removing matchapack's own state")?;
    } else if yes {
        writeln!(out, "matchapack uninstall: refused (below), nothing is removed; this is what it would remove")?;
    } else {
        writeln!(out, "matchapack uninstall: dry run, nothing is changed; `matchapack uninstall --yes` does what it lists")?;
    }
    writeln!(out)?;
    writeln!(out, "matchapack's own state:")?;
    for t in targets {
        let from = if t.set { format!(" ({})", t.kind.var()) } else { " (the default place)".to_string() };
        match &t.state {
            State::Absent => writeln!(out, "  {}: {}{from}: not there", t.kind.label(), t.path.display())?,
            State::Refused(why) => writeln!(out, "  {}: {}{from}: REFUSED: it {why}", t.kind.label(), t.path.display())?,
            State::Remove { delete, kept, nested, .. } => {
                let total: u64 = delete.iter().map(|d| d.1).sum();
                let names = delete.iter().map(|d| d.0.to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
                let what = if !names.is_empty() {
                    format!("{} on disk ({names})", size(total))
                } else if !kept.is_empty() {
                    "nothing (the directory stays: other names in it)".to_string()
                } else if !nested.is_empty() {
                    "the directory, after what it holds (below)".to_string()
                } else {
                    "the empty directory".to_string()
                };
                writeln!(out, "  {}: {}{from}: {} {what}", t.kind.label(), t.path.display(), if removing { "removing" } else { "would remove" })?;
                for (n, k) in nested {
                    writeln!(out, "    {}: the {} (above)", t.path.join(n).display(), k.label())?;
                }
                for (n, why) in kept {
                    writeln!(out, "    kept: {}: {why}", t.path.join(n).display())?;
                }
            }
        }
    }
    let backups = env.backups();
    if std::fs::symlink_metadata(&backups).is_ok_and(|m| m.is_dir()) {
        writeln!(
            out,
            "  kept: {} ({} on disk): node_modules backups scripts/convert-existing.sh kept when a conversion did not finish; check them, then remove them yourself: rm -rf {}",
            backups.display(),
            size(on_disk(&backups)),
            crate::wrapper::shell_quote(&backups.to_string_lossy())
        )?;
    }
    if let Some((_, delete, _)) = &store_plan {
        let total: u64 = delete.iter().map(|d| d.1).sum();
        let (shared, projects) = shared_with_projects(&store_t.path);
        writeln!(out)?;
        writeln!(
            out,
            "Space: the store takes {} on disk, but its blocks are shared with the projects installed from it (their files are clones). Removing it frees only blocks no project shares: {} of it {} recorded as used by {projects} project(s) that still have node_modules, so about {} comes back (an estimate: projects installed before records existed are not counted; `matchapack store adopt` records them).{}",
            size(total),
            size(shared),
            if shared == 0 { "is" } else { "are" },
            size(total.saturating_sub(shared)),
            if yes { "" } else { " The real run reports the volume's free space before and after." }
        )?;
    }

    let mut code = 0;
    if refused {
        writeln!(out)?;
        writeln!(
            out,
            "Refused: nothing {} removed. uninstall deletes only what it recognises as matchapack's: for each REFUSED line, remove what is matchapack's there yourself, or set the variable named there to the directory matchapack used (or unset it).",
            if yes { "was" } else { "would be" }
        )?;
        code = 1;
    }
    if let Some(why) = &busy_why {
        writeln!(out)?;
        writeln!(out, "{why}")?;
        if yes {
            code = 1;
        }
    }

    if removing {
        // The store's volume, through the nearest directory that is still
        // there after the removal.
        let free = || store_t.path.ancestors().skip(1).find(|a| a.is_dir()).and_then(store::free_bytes);
        let free0 = free();
        let mut failed = vec![];
        writeln!(out)?;
        let mut stop = None;
        if let Some((id, delete, keep_some)) = &store_plan {
            match remove_store(store_t, *id, delete, *keep_some, out)? {
                Ok(f) => failed.extend(f),
                // Refused before anything was removed: nothing else is.
                Err(why) => stop = Some(why),
            }
        }
        if let Some(why) = stop {
            writeln!(out, "refused, nothing was removed: {why}")?;
            return Ok(1);
        }
        for t in &targets[1..] {
            let State::Remove { id, delete, kept, .. } = &t.state else { continue };
            let o = match open_target(&t.path, *id) {
                Ok(o) => o,
                Err(why) => {
                    failed.push(format!("{}: {why}", t.path.display()));
                    continue;
                }
            };
            let f = remove_names(&o, &t.path, delete, "");
            if kept.is_empty() && f.is_empty() {
                // Fails when something is left (a nested store that stayed);
                // reported below.
                let _ = at::rmdir(o.parent.as_raw_fd(), &o.name, *id);
            }
            failed.extend(f);
        }
        for t in targets {
            if let State::Remove { .. } = t.state {
                let gone = std::fs::symlink_metadata(&t.path).is_err();
                writeln!(out, "{}: {} {}", t.kind.label(), t.path.display(), if gone { "removed" } else { "emptied of matchapack's files; the directory stays (something else is in it)" })?;
            }
        }
        for f in &failed {
            writeln!(out, "  could not remove {f}")?;
        }
        if !failed.is_empty() {
            code = 1;
        }
        if let (true, Some(a), Some(b)) = (store_plan.is_some(), free0, free()) {
            writeln!(
                out,
                "free space on the store's volume: {} before, {} after ({:+.2} GiB; other activity on the volume counts too)",
                size(a),
                size(b),
                (b as f64 - a as f64) / 1024f64.powi(3)
            )?;
        }
    }

    writeln!(out)?;
    writeln!(
        out,
        "Your projects keep working: their node_modules files are independent clones, not links into the store (scripts/test-store.sh case 14 removes a project's store entries and its files read the same). Yarn's cache is Yarn's own and is not touched. A later `npm ci` / `yarn` without matchapack is plain npm / Yarn."
    )?;

    writeln!(out)?;
    let lines = rc_lines(&env.rc_files);
    if lines.is_empty() {
        let read: Vec<String> = env.rc_files.iter().filter(|f| f.exists()).map(|f| f.display().to_string()).collect();
        writeln!(
            out,
            "Shell integration: no line mentions matchapack in {}. If you added `matchapack init` elsewhere (a file your startup files source, a dotfiles manager), delete it there.",
            if read.is_empty() { "your shell startup files (none found)".to_string() } else { read.join(", ") }
        )?;
    } else {
        writeln!(out, "Shell integration: uninstall never edits these files. Delete the lines that are matchapack's (the `matchapack init` line; any MATCHAPACK_* setting):")?;
        for (f, n, l) in &lines {
            writeln!(out, "  {}:{n}: {l}", f.display())?;
        }
    }
    writeln!(
        out,
        "Shells already open keep the npm (and yarn) function until they exit; once the binary is gone it runs plain npm (and Yarn). A bash or zsh shell whose function came from a matchapack older than this one may keep calling the removed binary: run `hash -r` there, or open a new shell."
    )?;

    writeln!(out)?;
    match &env.exe {
        Some(exe) if is_cargo_install(exe, &env.cargo_bin) => writeln!(out, "Last, the binary ({}), installed by cargo install: cargo uninstall matchapack", exe.display())?,
        Some(exe) => writeln!(out, "Last, the binary: rm {}", crate::wrapper::shell_quote(&exe.to_string_lossy()))?,
        None => writeln!(out, "Last, the binary: `cargo uninstall matchapack` if cargo installed it, otherwise delete the file `command -v matchapack` names")?,
    }
    Ok(code)
}

fn is_cargo_install(exe: &Path, cargo_bin: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    exe.parent().is_some_and(|d| canon(d) == canon(cargo_bin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::scratch;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::Mutex;

    /// Tests' hooks in `remove_store`, by the store's path (tests run in
    /// parallel) and stage: "before-open" (before the store is opened for
    /// removal) and "after-lock" (its lock held, before it is moved aside).
    #[allow(clippy::type_complexity)]
    static HOOKS: Mutex<Vec<(PathBuf, &'static str, Box<dyn FnOnce() + Send>)>> = Mutex::new(Vec::new());

    fn set_hook(store: &Path, stage: &'static str, f: Box<dyn FnOnce() + Send>) {
        HOOKS.lock().unwrap().push((store.to_path_buf(), stage, f));
    }

    pub(super) fn hook(store: &Path, stage: &str) {
        let f = {
            let mut h = HOOKS.lock().unwrap();
            h.iter().position(|(p, s, _)| p == store && *s == stage).map(|i| h.remove(i).2)
        };
        if let Some(f) = f {
            f();
        }
    }

    /// A home with a filled store, logs and settings at their default
    /// places, everything under `d`. Never the real ones: every path is built
    /// from `d`, and Env is never read from this process's environment.
    fn setup(d: &Path) -> Env {
        let home = d.join("home");
        let logs = home.join(".cache/matchapack");
        let store = logs.join("store");
        let config = home.join(".config/matchapack");
        let entry = store.join(store::LAYOUT).join("sha512/ab/cdef");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("index.js"), "x".repeat(10_000)).unwrap();
        for dir in ["tmp", "projects", "sizes/sha512/ab", "made-by", "locks"] {
            fs::create_dir_all(store.join(dir)).unwrap();
        }
        fs::write(store.join("lock"), "").unwrap();
        fs::write(logs.join("log.jsonl"), "{}\n").unwrap();
        fs::write(logs.join("verify.jsonl"), "{}\n").unwrap();
        fs::write(logs.join("repair-1-2.list"), "").unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("paths"), "/x\n").unwrap();
        fs::write(home.join(".zshrc"), "export PATH=$HOME/.cargo/bin:$PATH\ncommand -v matchapack >/dev/null && eval \"$(matchapack init zsh --alias mp)\"\n").unwrap();
        fs::create_dir_all(home.join(".config/fish")).unwrap();
        fs::write(home.join(".config/fish/config.fish"), "set -x MATCHAPACK_PATHS /x\n").unwrap();
        Env {
            store,
            store_set: false,
            logs,
            logs_set: false,
            config,
            config_set: false,
            rc_files: vec![home.join(".zshrc"), home.join(".bashrc"), home.join(".config/fish/config.fish")],
            exe: Some(home.join(".cargo/bin/matchapack")),
            cargo_bin: home.join(".cargo/bin"),
            home,
        }
    }

    /// Every path under `d` with its type, size, mode and mtime.
    fn snapshot(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = walkdir::WalkDir::new(d)
            .into_iter()
            .flatten()
            .map(|e| {
                let m = e.path().symlink_metadata().unwrap();
                format!("{} {:?} {} {:o} {}.{}", e.path().display(), m.file_type(), m.len(), m.mode(), m.mtime(), m.mtime_nsec())
            })
            .collect();
        v.sort();
        v
    }

    fn run_to_string(env: &Env, yes: bool) -> (i32, String) {
        let mut out = vec![];
        let code = run(env, yes, &mut out).unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn dry_run_writes_nothing() {
        let d = scratch("uninstall-dry");
        let env = setup(&d);
        let before = snapshot(&d);
        let (code, out) = run_to_string(&env, false);
        assert_eq!(code, 0, "{out}");
        assert_eq!(snapshot(&d), before, "a dry run changes nothing:\n{out}");
        assert!(out.contains("would remove"), "{out}");
        assert!(out.contains(&format!("{}:2: command -v matchapack", env.home.join(".zshrc").display())), "rc line by file:line:\n{out}");
        assert!(out.contains("config.fish:1: set -x MATCHAPACK_PATHS"), "{out}");
        assert!(out.contains("cargo uninstall matchapack"), "{out}");
        assert!(out.contains("Your projects keep working"), "{out}");
        // Nothing there: nothing is created either (no store, no lock file).
        let empty = scratch("uninstall-dry-empty");
        let mut env = setup(&empty);
        env.store = empty.join("none/store");
        env.logs = empty.join("none/logs");
        env.config = empty.join("none/config");
        let (code, out) = run_to_string(&env, false);
        assert_eq!(code, 0, "{out}");
        assert!(!empty.join("none").exists(), "{out}");
        assert_eq!(out.matches(": not there").count(), 3, "{out}");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn removes_only_matchapacks_and_follows_no_link() {
        let d = scratch("uninstall-yes");
        let env = setup(&d);
        // Outside the store: a link to it from inside must not lead the
        // removal there.
        let outside = d.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "mine").unwrap();
        symlink(&outside, env.store.join(store::LAYOUT).join("sha512/ab/link")).unwrap();
        symlink(&outside, env.store.join("tmp/link")).unwrap();
        // The default settings directory holds a file of the user's: kept,
        // with the directory.
        fs::write(env.config.join("notes.txt"), "mine").unwrap();
        // convert-existing.sh's backups: kept.
        fs::create_dir_all(env.logs.join("backup/p-1")).unwrap();
        let (code, out) = run_to_string(&env, true);
        assert_eq!(code, 0, "{out}");
        assert!(!env.store.exists(), "{out}");
        assert_eq!(fs::read_to_string(outside.join("keep.txt")).unwrap(), "mine", "no link followed");
        assert_eq!(fs::read_to_string(env.config.join("notes.txt")).unwrap(), "mine");
        assert!(!env.config.join("paths").exists());
        assert!(env.logs.join("backup/p-1").is_dir(), "backups kept");
        assert!(!env.logs.join("log.jsonl").exists() && !env.logs.join("verify.jsonl").exists() && !env.logs.join("repair-1-2.list").exists());
        assert!(out.contains("remove them yourself"), "the backups' removal is the user's: {out}");
        assert!(fs::read_to_string(env.home.join(".zshrc")).unwrap().contains("matchapack init"), "rc files are never edited");
        // Without the user's file and the backups, the directories go too.
        let d2 = scratch("uninstall-yes-all");
        let env = setup(&d2);
        let (code, out) = run_to_string(&env, true);
        assert_eq!(code, 0, "{out}");
        assert!(!env.logs.exists() && !env.config.exists(), "{out}");
        assert!(env.home.join(".zshrc").exists());
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    /// Each refusal removes nothing at all, with --yes too.
    #[test]
    fn refuses_what_it_cannot_recognise() {
        let d = scratch("uninstall-refuse");
        let check = |name: &str, env: &Env, want: &str| {
            let before = snapshot(&d);
            let (code, out) = run_to_string(env, true);
            assert_eq!(code, 1, "{name}: {out}");
            assert!(out.contains("REFUSED") && out.contains(want), "{name}: want {want:?} in\n{out}");
            assert!(out.contains("nothing was removed"), "{name}: {out}");
            assert_eq!(snapshot(&d), before, "{name}: nothing removed");
        };
        let env0 = || setup(&d);
        let mut env = env0();
        env.store = PathBuf::from("/");
        env.store_set = true;
        check("store /", &env, "it is /");
        let mut env = env0();
        env.config = env.home.clone();
        env.config_set = true;
        check("config = home", &env, "home directory or contains it");
        let mut env = env0();
        env.logs = d.clone();
        env.logs_set = true;
        check("logs = a parent of home", &env, "home directory or contains it");
        let mut env = env0();
        env.store = PathBuf::from("rel/store");
        env.store_set = true;
        check("relative", &env, "not an absolute path");
        // An existing relative directory (the tests run in the checkout):
        // refused before it is looked at, by plan and by refuse_reason.
        let mut env = env0();
        env.config = PathBuf::from("src");
        env.config_set = false;
        check("relative, existing", &env, "not an absolute path");
        assert_eq!(refuse_reason(Path::new("src"), &env.home).as_deref(), Some("is not an absolute path"));
        let mut env = env0();
        let link = d.join("store-link");
        let _ = fs::remove_file(&link);
        symlink(&env.store, &link).unwrap();
        env.store = link;
        env.store_set = true;
        check("symlinked store", &env, "symbolic link");
        // A store without the layout directory: not recognisable.
        let mut env = env0();
        let fake = d.join("not-a-store");
        fs::create_dir_all(fake.join("projects")).unwrap();
        fs::create_dir_all(fake.join("sizes")).unwrap();
        env.store = fake;
        env.store_set = true;
        check("no layout marker", &env, "has no v2/ directory");
        // A directory a variable names that holds other files: it may not be
        // matchapack's (a `paths` file of someone else's would go).
        let mut env = env0();
        let docs = d.join("docs");
        fs::create_dir_all(&docs).unwrap();
        fs::write(docs.join("paths"), "someone else's").unwrap();
        fs::write(docs.join("report.txt"), "").unwrap();
        env.config = docs;
        env.config_set = true;
        check("unknown names under a variable", &env, "holds names matchapack does not make (report.txt)");
        let _ = fs::remove_dir_all(&d);
    }

    /// Refused while an install holds the store lock (shared, as installs
    /// take it) or a process holds a directory in tmp/ (the Yarn engine's
    /// helper files); the dry run says so and still writes nothing.
    #[test]
    fn refuses_while_the_store_is_in_use() {
        let d = scratch("uninstall-lock");
        let env = setup(&d);
        let before = snapshot(&d);
        {
            let held = fs::OpenOptions::new().read(true).write(true).open(env.store.join("lock")).unwrap();
            store::flock(&held, libc::LOCK_SH | libc::LOCK_NB).unwrap();
            let (code, out) = run_to_string(&env, true);
            assert_eq!(code, 1, "{out}");
            assert!(out.contains("an install is using the store") && out.contains("nothing was removed"), "{out}");
            assert_eq!(snapshot(&d), before);
            let (code, out) = run_to_string(&env, false);
            assert_eq!(code, 0, "{out}");
            assert!(out.contains("--yes refuses until then"), "{out}");
            assert_eq!(snapshot(&d), before);
        }
        {
            let run_dir = env.store.join("tmp/123-456-run");
            fs::create_dir(&run_dir).unwrap();
            let before = snapshot(&d);
            let held = fs::File::open(&run_dir).unwrap();
            store::flock(&held, libc::LOCK_EX | libc::LOCK_NB).unwrap();
            let (code, out) = run_to_string(&env, true);
            assert_eq!(code, 1, "{out}");
            assert!(out.contains("a matchapack process is running"), "{out}");
            assert_eq!(snapshot(&d), before);
        }
        // Released: removed.
        let (code, out) = run_to_string(&env, true);
        assert_eq!(code, 0, "{out}");
        assert!(!env.store.exists(), "{out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn binary_channel() {
        let d = scratch("uninstall-bin");
        let mut env = setup(&d);
        fs::create_dir_all(&env.cargo_bin).unwrap();
        assert!(run_to_string(&env, false).1.contains("installed by cargo install: cargo uninstall matchapack"));
        env.exe = Some(d.join("bin/matchapack"));
        let out = run_to_string(&env, false).1;
        let want = format!("Last, the binary: rm {}", crate::wrapper::shell_quote(&d.join("bin/matchapack").to_string_lossy()));
        assert!(out.contains(&want) && !out.contains("cargo uninstall"), "{out}");
        let _ = fs::remove_dir_all(&d);
    }

    /// The store swapped after the plan, for a link to another directory or
    /// for another directory moved into its place: nothing of the victim is
    /// removed, and the run fails before removing anything.
    #[test]
    fn a_store_swapped_after_the_plan_is_left_alone() {
        for (how, during) in [("link", false), ("dir", false), ("link", true), ("dir", true)] {
            let how_s = format!("{how}{}", if during { "-during" } else { "" });
            let how = how_s.as_str();
            let d = scratch(&format!("uninstall-swap-{how}"));
            let link = how.starts_with("link");
            let mut env = setup(&d);
            // A store of its own, so that the swap leaves logs and settings
            // as planned.
            let store = d.join("own-store");
            fs::rename(&env.store, &store).unwrap();
            env.store = store.clone();
            env.store_set = true;
            let victim = d.join("victim");
            fs::create_dir_all(victim.join(store::LAYOUT)).unwrap();
            fs::write(victim.join("lock"), "").unwrap();
            fs::write(victim.join(store::LAYOUT).join("precious"), "mine").unwrap();
            let targets = plan(&env);
            assert!(matches!(targets[0].state, State::Remove { .. }), "{:?}", targets[0]);
            let swap = {
                let (store, victim, away) = (store.clone(), victim.clone(), d.join("moved-away"));
                move || {
                    fs::rename(&store, away).unwrap();
                    if link {
                        symlink(&victim, &store).unwrap();
                    } else {
                        fs::rename(&victim, &store).unwrap();
                    }
                }
            };
            if during {
                set_hook(&store, "before-open", Box::new(swap));
            } else {
                swap();
            }
            let mut out = vec![];
            let code = execute(&env, &targets, true, &mut out).unwrap();
            let out = String::from_utf8(out).unwrap();
            let precious = if link { victim.join(store::LAYOUT).join("precious") } else { store.join(store::LAYOUT).join("precious") };
            assert_eq!(fs::read_to_string(&precious).unwrap(), "mine", "{how}: the victim's file stays:\n{out}");
            assert_eq!(code, 1, "{how}: {out}");
            assert!(out.contains("nothing was removed"), "{how}: {out}");
            if during {
                assert!(out.contains("changed since it was checked"), "{how}: {out}");
            }
            assert!(env.config.join("paths").exists() && env.logs.join("log.jsonl").exists(), "{how}: nothing else removed either");
            let _ = fs::remove_dir_all(&d);
        }
    }

    /// The settings directory swapped after the plan: left alone, reported.
    #[test]
    fn a_settings_directory_swapped_after_the_plan_is_left_alone() {
        let d = scratch("uninstall-swap-config");
        let env = setup(&d);
        let victim = d.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("paths"), "mine").unwrap();
        let targets = plan(&env);
        fs::rename(&env.config, d.join("moved-away")).unwrap();
        symlink(&victim, &env.config).unwrap();
        let mut out = vec![];
        let code = execute(&env, &targets, true, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(fs::read_to_string(victim.join("paths")).unwrap(), "mine", "{out}");
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("changed since it was checked"), "{out}");
        let _ = fs::remove_dir_all(&d);
    }

    /// `<link>/` is the link (refused), not what it points to; `..` is
    /// refused; one store spelled through a linked parent is still the store
    /// inside the log directory (nested, removed), not a stray to keep.
    #[test]
    fn paths_are_normalised_and_compared_by_identity() {
        let d = scratch("uninstall-norm");
        let env0 = || setup(&d);
        let mut env = env0();
        let link = d.join("store-link");
        let _ = fs::remove_file(&link);
        symlink(&env.store, &link).unwrap();
        env.store = PathBuf::from(format!("{}/", link.display()));
        env.store_set = true;
        let (code, out) = run_to_string(&env, true);
        assert!(code == 1 && out.contains("REFUSED: it is a symbolic link"), "{out}");
        assert!(env0().store.join(store::LAYOUT).is_dir());
        let mut env = env0();
        env.store = env.logs.join("x/../store");
        env.store_set = true;
        let (code, out) = run_to_string(&env, true);
        assert!(code == 1 && out.contains("contains `..`"), "{out}");
        assert_eq!(normalise(Path::new("/a/b/./c/")).unwrap(), PathBuf::from("/a/b/c"));
        // Through a linked parent: the same directory.
        let mut env = env0();
        let alias = d.join("alias");
        let _ = fs::remove_file(&alias);
        symlink(&env.home, &alias).unwrap();
        env.store = alias.join(".cache/matchapack/store/");
        env.store_set = true;
        let (code, out) = run_to_string(&env, false);
        assert!(code == 0 && out.contains("store: the store (above)") && !out.contains("not the one MATCHAPACK_STORE names"), "{out}");
        let (code, out) = run_to_string(&env, true);
        assert_eq!(code, 0, "{out}");
        assert!(!env.logs.exists() && !env.config.exists(), "{out}");
        let _ = fs::remove_dir_all(&d);
    }

    /// A store only the Yarn engine has used (no v2/: tmp/ with run
    /// directories, lock, projects) is matchapack's; with anything else in
    /// tmp/ it is not, and the refusal says where the path came from.
    #[test]
    fn a_yarn_only_store_is_recognised() {
        let d = scratch("uninstall-yarn-only");
        let env = setup(&d);
        fs::remove_dir_all(env.store.join(store::LAYOUT)).unwrap();
        for n in ["sizes", "made-by", "locks"] {
            fs::remove_dir_all(env.store.join(n)).unwrap();
        }
        fs::create_dir_all(env.store.join("tmp/123-1763400000000000000-run")).unwrap();
        fs::create_dir_all(env.store.join("tmp/123-x4-0123abcd")).unwrap();
        let (code, out) = run_to_string(&env, true);
        assert_eq!(code, 0, "{out}");
        assert!(!env.store.exists() && !env.logs.exists() && !env.config.exists(), "{out}");
        assert!(is_tmp_name("1-2-run") && is_tmp_name("1-run") && is_tmp_name("1-2-gc3") && is_tmp_name("1-2-x3-ab09") && !is_tmp_name("node_modules") && !is_tmp_name("1-2-rm") && !is_tmp_name("a-2-run"));
        let d2 = scratch("uninstall-yarn-other");
        let env = setup(&d2);
        fs::remove_dir_all(env.store.join(store::LAYOUT)).unwrap();
        for n in ["sizes", "made-by", "locks"] {
            fs::remove_dir_all(env.store.join(n)).unwrap();
        }
        fs::create_dir_all(env.store.join("tmp/photos")).unwrap();
        let (code, out) = run_to_string(&env, true);
        assert!(code == 1 && out.contains("(the default place): REFUSED: it has no v2/ directory"), "{out}");
        assert!(env.store.join("tmp/photos").is_dir() && env.config.join("paths").exists());
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    /// An install that starts while uninstall holds the store lock waits,
    /// then gets a fresh store at the same path; the old store is gone and
    /// nothing is left aside.
    #[test]
    fn an_install_waiting_during_uninstall_gets_a_fresh_store() {
        let d = scratch("uninstall-waiting");
        let mut env = setup(&d);
        let store = d.join("own-store");
        fs::rename(&env.store, &store).unwrap();
        env.store = store.clone();
        env.store_set = true;
        let installer: std::sync::Arc<Mutex<Option<std::thread::JoinHandle<crate::store::Store>>>> = Default::default();
        let (inst, root, cacache) = (installer.clone(), store.clone(), d.join("cacache"));
        set_hook(
            &store,
            "after-lock",
            Box::new(move || {
                *inst.lock().unwrap() = Some(std::thread::spawn(move || crate::store::Store::new(root, cacache).unwrap()));
                // Long enough for it to open the lock file and wait on it.
                std::thread::sleep(std::time::Duration::from_millis(500));
            }),
        );
        let (code, out) = run_to_string(&env, true);
        let fresh = installer.lock().unwrap().take().expect("the hook ran").join().unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(!store.join(store::LAYOUT).join("sha512").exists(), "the old entries are gone: {out}");
        assert!(store.join(store::LAYOUT).is_dir() && store.join("lock").is_file(), "the install made a fresh store");
        assert_eq!(fs::read_dir(&d).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".uninstall-")).count(), 0, "nothing left aside: {out}");
        drop(fresh);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sizes_follow_no_link_and_paths_are_quoted() {
        let d = scratch("uninstall-misc");
        fs::create_dir_all(d.join("big")).unwrap();
        fs::write(d.join("big/f"), "x".repeat(100_000)).unwrap();
        symlink(d.join("big"), d.join("link")).unwrap();
        assert!(on_disk(&d.join("big")) >= 100_000);
        assert!(on_disk(&d.join("link")) < 100_000, "a link is not followed, not even at the root");
        // A home whose path needs quoting: the printed removal command is quoted.
        let home = d.join("a b'c");
        let mut env = setup(&d);
        fs::create_dir_all(home.join(".cache/matchapack/backup")).unwrap();
        env.home = home.clone();
        let (_, out) = run_to_string(&env, false);
        let want = crate::wrapper::shell_quote(&home.join(".cache/matchapack/backup").to_string_lossy());
        assert!(want.contains("'\\''") && out.contains(&format!("yourself: rm -rf {want}")), "{want}\n{out}");
        let _ = fs::remove_dir_all(&d);
    }

    /// Under a refusal the plan's lines say what would be removed, not
    /// "removing".
    #[test]
    fn no_removing_under_a_refusal() {
        let d = scratch("uninstall-wording");
        let mut env = setup(&d);
        env.config = env.home.clone();
        env.config_set = true;
        let (code, out) = run_to_string(&env, true);
        assert!(code == 1 && !out.contains("removing") && out.contains("): would remove") && out.contains("refused (below), nothing is removed"), "{out}");
        let _ = fs::remove_dir_all(&d);
    }
}
