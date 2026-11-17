//! `matchapack uninstall`: remove matchapack's own state (the store, the
//! trial logs, the settings directory), and say what else to remove by hand
//! (the shell integration line, the binary). Without `--yes` it only prints
//! what it would do and writes nothing.
//!
//! It deletes directories, so it deletes only what it can recognise as
//! matchapack's:
//! - the three directories are resolved by the code the rest of the tool
//!   uses (`default_store`, `trial::cache_dir`, `util::config_dir`);
//! - a directory that is not absolute, is a symbolic link, is `/`, or is the
//!   home directory or one of its parents is refused;
//! - the store must hold the layout directory (`store::LAYOUT`);
//! - inside each directory only the names matchapack makes are removed. A
//!   directory a MATCHAPACK_* variable names that holds anything else is
//!   refused as a whole (it may not be matchapack's); in a default directory
//!   other names are left in place, and the directory with them;
//! - a removed directory's contents are removed without following any
//!   symbolic link (`std::fs::remove_dir_all` removes a link, never its
//!   target);
//! - it refuses while an install holds the store lock, or while any
//!   matchapack process holds a directory in the store's `tmp/` (the Yarn
//!   engine's helper files live there and take no store lock).
//!
//! Any refusal removes nothing at all.

use crate::store::{self, Integrity};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Top-level names in a store: the current layout and every name a build
/// has made there (git history of src/store.rs).
const STORE_NAMES: &[&str] = &[store::LAYOUT, "v1", "tmp", "lock", "sizes", "made-by", "locks", "declined", "declined-keys", "projects"];
/// The settings directory's files (trial scope; `npm-verified`: older builds).
const CONFIG_NAMES: &[&str] = &["paths", "npm-verified"];

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
    pub fn from_process() -> Env {
        let home = crate::home();
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let zdot = var("ZDOTDIR").unwrap_or_else(|| home.clone());
        let xdg = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let mut rc_files: Vec<PathBuf> = [".zshenv", ".zprofile", ".zshrc", ".zlogin"].iter().map(|f| zdot.join(f)).collect();
        rc_files.extend([".profile", ".bash_profile", ".bash_login", ".bashrc"].iter().map(|f| home.join(f)));
        rc_files.push(xdg.join("fish/config.fish"));
        let mut confd: Vec<PathBuf> = std::fs::read_dir(xdg.join("fish/conf.d"))
            .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "fish")).collect())
            .unwrap_or_default();
        confd.sort();
        rc_files.extend(confd);
        Env {
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
        }
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
    /// `delete`: the names to remove, with their size on disk; `kept`: names
    /// left in place and why; `nested`: another target inside this one.
    Remove { delete: Vec<(PathBuf, u64)>, kept: Vec<(PathBuf, String)>, nested: Vec<PathBuf> },
}

#[derive(Debug)]
pub struct Target {
    pub kind: Kind,
    pub path: PathBuf,
    pub set: bool,
    pub state: State,
}

/// Why `p` must not be removed as a directory of matchapack's, if it must
/// not; None when it does not exist.
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

fn on_disk(p: &Path) -> u64 {
    walkdir::WalkDir::new(p).into_iter().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.blocks() * 512).sum()
}

/// What removing `path` (a `kind` directory) would do. `others`: the other
/// targets' paths (the default store is inside the default log directory).
fn plan_one(kind: Kind, path: &Path, set: bool, env: &Env, others: &[&Path]) -> Target {
    let target = |state| Target { kind, path: path.to_path_buf(), set, state };
    if std::fs::symlink_metadata(path).is_err() {
        if path.is_absolute() || path.as_os_str().is_empty() {
            return target(if path.as_os_str().is_empty() { State::Refused("is empty".into()) } else { State::Absent });
        }
        return target(State::Refused("is not an absolute path".into()));
    }
    if let Some(why) = refuse_reason(path, &env.home) {
        return target(State::Refused(why));
    }
    if kind == Kind::Store && !std::fs::symlink_metadata(path.join(store::LAYOUT)).is_ok_and(|m| m.is_dir()) {
        return target(State::Refused(format!("has no {}/ directory: not recognisable as a matchapack store", store::LAYOUT)));
    }
    let mut names: Vec<_> = match std::fs::read_dir(path) {
        Ok(d) => d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(e) => return target(State::Refused(format!("cannot be read ({e})"))),
    };
    names.sort();
    let (mut delete, mut kept, mut nested, mut unknown) = (vec![], vec![], vec![], vec![]);
    for n in names {
        let p = path.join(&n);
        if others.iter().any(|o| *o == p) {
            nested.push(p);
            continue;
        }
        let ours = match kind {
            Kind::Store => STORE_NAMES.contains(&n.as_str()),
            Kind::Config => CONFIG_NAMES.contains(&n.as_str()),
            Kind::Logs => {
                if n == "backup" && p == env.backups() {
                    kept.push((p, "convert-existing.sh's backups (below)".to_string()));
                    continue;
                }
                n == "log.jsonl" || n == "verify.jsonl" || (n.starts_with("repair-") && n.ends_with(".list"))
            }
        };
        if ours {
            let size = on_disk(&p);
            delete.push((p, size));
        } else {
            unknown.push(p);
        }
    }
    if !unknown.is_empty() {
        let list = unknown.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
        if set {
            return target(State::Refused(format!(
                "holds names matchapack does not make ({list}): it may not be matchapack's directory; remove matchapack's files in it yourself, or unset {} if it is not the one you used",
                kind.var()
            )));
        }
        kept.extend(unknown.into_iter().map(|p| (p, "not made by matchapack".to_string())));
    }
    target(State::Remove { delete, kept, nested })
}

pub fn plan(env: &Env) -> Vec<Target> {
    let all = [(Kind::Store, &env.store, env.store_set), (Kind::Logs, &env.logs, env.logs_set), (Kind::Config, &env.config, env.config_set)];
    all.iter()
        .map(|(k, p, set)| {
            let others: Vec<&Path> = all.iter().filter(|(o, _, _)| o != k).map(|(_, q, _)| q.as_path()).collect();
            plan_one(*k, p, *set, env, &others)
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

/// Take the store lock exclusively (as `store gc` does), without creating
/// anything, and check that no process holds a directory in `tmp/`. The
/// returned file keeps the lock until it is dropped.
pub fn store_busy(root: &Path) -> Busy {
    let lock = root.join("lock");
    let f = match std::fs::symlink_metadata(&lock) {
        Err(_) => None,
        Ok(m) if !m.is_file() => return Busy::Unknown(format!("{} is not a regular file", lock.display())),
        Ok(_) => match std::fs::OpenOptions::new().read(true).write(true).open(&lock) {
            Ok(f) => match store::flock(&f, libc::LOCK_EX | libc::LOCK_NB) {
                Ok(()) => Some(f),
                Err(e) if store::would_block(&e) => return Busy::Held("an install is using the store (it holds the store lock)".into()),
                Err(e) => return Busy::Unknown(format!("cannot lock the store ({e})")),
            },
            Err(e) => return Busy::Unknown(format!("cannot open {} ({e})", lock.display())),
        },
    };
    if let Ok(rd) = std::fs::read_dir(root.join("tmp")) {
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(d) = std::fs::File::open(e.path()) else { continue };
            if let Err(err) = store::flock(&d, libc::LOCK_EX | libc::LOCK_NB) {
                if store::would_block(&err) {
                    return Busy::Held(format!("a matchapack process is running (it holds {})", e.path().display()));
                }
            }
        }
    }
    Busy::Free(f)
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

/// Remove one name in a target: a directory with everything in it (no link
/// followed), anything else (a file, a link) by itself.
fn remove(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
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

/// Run `matchapack uninstall`; returns the exit status. With `yes` false
/// nothing is written.
pub fn run(env: &Env, yes: bool, out: &mut dyn Write) -> std::io::Result<i32> {
    let targets = plan(env);
    let store_t = &targets[0];
    let has_store = matches!(store_t.state, State::Remove { .. });
    let refused: Vec<&Target> = targets.iter().filter(|t| matches!(t.state, State::Refused(_))).collect();
    let busy = if has_store { Some(store_busy(&store_t.path)) } else { None };

    if yes {
        writeln!(out, "matchapack uninstall: removing matchapack's own state")?;
    } else {
        writeln!(out, "matchapack uninstall: dry run, nothing is changed; `matchapack uninstall --yes` does what it lists")?;
    }
    writeln!(out)?;
    writeln!(out, "matchapack's own state:")?;
    for t in &targets {
        let from = if t.set { format!(" ({})", t.kind.var()) } else { String::new() };
        match &t.state {
            State::Absent => writeln!(out, "  {}: {}{from}: not there", t.kind.label(), t.path.display())?,
            State::Refused(why) => writeln!(out, "  {}: {}{from}: REFUSED: it {why}", t.kind.label(), t.path.display())?,
            State::Remove { delete, kept, nested } => {
                let total: u64 = delete.iter().map(|d| d.1).sum();
                let names = delete.iter().map(|d| d.0.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ");
                let what = if !names.is_empty() {
                    format!("{} on disk ({names})", size(total))
                } else if !kept.is_empty() {
                    "nothing (the directory stays: other names in it)".to_string()
                } else if !nested.is_empty() {
                    "the directory, after what it holds (below)".to_string()
                } else {
                    "the empty directory".to_string()
                };
                writeln!(out, "  {}: {}{from}: {} {what}", t.kind.label(), t.path.display(), if yes { "removing" } else { "would remove" })?;
                for n in nested {
                    writeln!(out, "    {}: the {} (above)", n.display(), targets.iter().find(|o| o.path == *n).map(|o| o.kind.label()).unwrap_or("?"))?;
                }
                for (p, why) in kept {
                    writeln!(out, "    kept: {}: {why}", p.display())?;
                }
            }
        }
    }
    let backups = env.backups();
    let backup_size = std::fs::symlink_metadata(&backups).is_ok_and(|m| m.is_dir()).then(|| on_disk(&backups));
    if let Some(b) = backup_size {
        writeln!(
            out,
            "  kept: {} ({} on disk): node_modules backups scripts/convert-existing.sh kept when a conversion did not finish; check them, then remove them yourself: rm -rf '{}'",
            backups.display(),
            size(b),
            backups.display()
        )?;
    }
    if let State::Remove { delete, .. } = &store_t.state {
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
    if !refused.is_empty() {
        writeln!(out)?;
        writeln!(out, "Refused: nothing {} removed. A directory uninstall cannot recognise as matchapack's is not deleted; fix or unset the variable named above, or remove its files yourself.", if yes { "was" } else { "would be" })?;
        code = 1;
    }
    let mut lock = None;
    match busy {
        Some(Busy::Held(why)) => {
            writeln!(out)?;
            writeln!(out, "{why}: {}; run `matchapack uninstall` again when it has finished.", if yes { "refused, nothing was removed" } else { "--yes refuses until then" })?;
            if yes {
                code = 1;
            }
        }
        Some(Busy::Unknown(why)) => {
            writeln!(out)?;
            writeln!(out, "{why}: {}", if yes { "refusing to remove the store without its lock; nothing was removed" } else { "--yes would refuse to remove the store without its lock" })?;
            if yes {
                code = 1;
            }
        }
        Some(Busy::Free(f)) => lock = f,
        None => {}
    }

    if yes && code == 0 {
        // The store's volume, through the nearest directory that is still
        // there after the removal.
        let free = || store_t.path.ancestors().skip(1).find(|a| a.is_dir()).and_then(store::free_bytes);
        let free0 = free();
        let mut failed = vec![];
        for t in &targets {
            let State::Remove { delete, kept, .. } = &t.state else { continue };
            // The store's lock file last: an install that starts meanwhile
            // waits on it.
            let mut order: Vec<&PathBuf> = delete.iter().map(|d| &d.0).filter(|p| !p.ends_with("lock")).collect();
            order.extend(delete.iter().map(|d| &d.0).filter(|p| p.ends_with("lock")));
            for p in order {
                if t.kind == Kind::Store && p.ends_with("lock") {
                    drop(lock.take());
                }
                if let Err(e) = remove(p) {
                    failed.push(format!("{}: {e}", p.display()));
                }
            }
            if kept.is_empty() {
                // Fails (and stays) when something is left, or a nested
                // target was refused; reported below.
                let _ = std::fs::remove_dir(&t.path);
            }
        }
        writeln!(out)?;
        for t in &targets {
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
        if let (true, Some(a), Some(b)) = (has_store, free0, free()) {
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
    writeln!(out, "Shells already open keep the npm (and yarn) function until they exit; once the binary is gone it runs plain npm (and Yarn).")?;

    writeln!(out)?;
    match &env.exe {
        Some(exe) if is_cargo_install(exe, &env.cargo_bin) => writeln!(out, "Last, the binary ({}), installed by cargo install: cargo uninstall matchapack", exe.display())?,
        Some(exe) => writeln!(out, "Last, the binary: rm '{}'", exe.display())?,
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
        env.store = fake;
        env.store_set = true;
        check("no layout marker", &env, "not recognisable as a matchapack store");
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
        assert!(out.contains(&format!("Last, the binary: rm '{}'", d.join("bin/matchapack").display())) && !out.contains("cargo uninstall"), "{out}");
        let _ = fs::remove_dir_all(&d);
    }
}
