//! `matchapack store gc`: remove the store entries no existing project uses.
//!
//! Reference-based, not age-based. Removing an entry never breaks an installed
//! node_modules (clones are independent files), but while a node_modules still
//! shares an entry's blocks, removing it frees nothing and costs that
//! project's next install a re-extraction. So an entry stays while any project
//! that was installed from it still has its node_modules.
//!
//! Which projects: every install records its directory and entries under
//! `<store>/projects/` (Store::update_record). Projects installed before
//! records existed are found through the trial log (outcome `installed`; the
//! run's `root`, or its `dir` in logs from before npm reported its root),
//! with the entries their hidden lockfile
//! (`node_modules/.package-lock.json`) lists, or recorded by `store adopt`
//! (`adopt` below, #30), which finds them by walking directories.

use crate::store::{Integrity, Store, GRACE_SECS};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Report {
    /// Projects whose entries are kept: (from records, from the trial log).
    pub projects: (usize, usize),
    /// Records of projects whose directory or node_modules is gone
    /// ("records without a project"), sorted: projects moved, renamed, or
    /// copied and the original deleted, or deleted (`store adopt` records a
    /// moved one where it is now).
    pub records_dropped: Vec<PathBuf>,
    /// Of the entries removed (or, dry run, to remove), those a dropped
    /// record listed: what only those projects used.
    pub dropped_entries_removed: usize,
    /// Records kept because whether their node_modules still exists could
    /// not be told (`node_modules_state`), with why; sorted.
    pub records_unchecked: Vec<(PathBuf, String)>,
    pub entries: usize,
    pub removed: usize,
    pub bytes_removed: u64,
    /// Unused, but younger than the grace period.
    pub recent: usize,
}

fn has_node_modules(dir: &Path) -> bool {
    dir.join("node_modules").is_dir()
}

/// Whether a recorded project still has its node_modules: Ok(true) when it
/// is a directory, Ok(false) when it or the project is absent (or not a
/// directory), Err(why) when that cannot be told: any other error (a parent
/// directory this process cannot search), or, on macOS, a path on a volume
/// under /Volumes that is not mounted (absent there means nothing about the
/// project). gc keeps such a record: dropping it would remove entries a
/// project may still share.
fn node_modules_state(dir: &Path) -> std::result::Result<bool, String> {
    match std::fs::metadata(dir.join("node_modules")) {
        Ok(m) => Ok(m.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR) => {
            let mut parts = dir.components().skip(1);
            if cfg!(target_os = "macos") && parts.next().is_some_and(|c| c.as_os_str() == "Volumes") {
                if let Some(vol) = parts.next() {
                    let vol = Path::new("/Volumes").join(vol.as_os_str());
                    if !vol.exists() {
                        return Err(format!("{} is not mounted", vol.display()));
                    }
                }
            }
            Ok(false)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The entries a hidden lockfile lists.
fn hidden_lock_entries(dir: &Path) -> Vec<Integrity> {
    read_hidden_lock(dir).unwrap_or_default()
}

/// The integrities `<dir>/node_modules/.package-lock.json` lists, or why it
/// could not be read. Every package with one (links and bundled packages
/// have none): each was extracted from its tarball, so its integrity names
/// the store entry an install would have cloned it from.
pub(crate) fn read_hidden_lock(dir: &Path) -> std::result::Result<Vec<Integrity>, String> {
    let text = std::fs::read_to_string(dir.join("node_modules/.package-lock.json")).map_err(|e| format!("hidden lockfile unreadable: {e}"))?;
    let v = serde_json::from_str::<Value>(&text).map_err(|e| format!("hidden lockfile is not JSON: {e}"))?;
    let pkgs = v.get("packages").and_then(|p| p.as_object()).ok_or("hidden lockfile has no `packages`")?;
    Ok(pkgs.values().filter_map(|p| p.get("integrity")?.as_str()).filter_map(|s| Integrity::parse(s).ok()).collect())
}

/// A directory `store adopt` does not enter: a node_modules (what is inside
/// is packages, not projects), a `.git`, the store.
fn pruned(dir: &Path, store_root: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "node_modules" || n == ".git") || dir == store_root
}

#[derive(Default)]
struct Walked {
    dirs: usize,
    found: Vec<PathBuf>,
    no_hidden_lock: usize,
    skipped: Vec<(PathBuf, String)>,
}

impl Walked {
    fn merge(mut self, o: Walked) -> Walked {
        self.dirs += o.dirs;
        self.found.extend(o.found);
        self.no_hidden_lock += o.no_hidden_lock;
        self.skipped.extend(o.skipped);
        self
    }
}

/// The projects under `dir`, walked in parallel without following
/// symlinks: a directory entry's type comes from readdir, and a symlink is
/// never a directory to enter. (No child processes: threads only.)
fn walk(dir: &Path, store_root: &Path) -> Walked {
    let mut w = Walked { dirs: 1, ..Default::default() };
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            w.skipped.push((dir.to_path_buf(), format!("unreadable: {e}")));
            return w;
        }
    };
    let mut sub = vec![];
    for ent in rd {
        let (ent, ft) = match ent.and_then(|e| e.file_type().map(|t| (e, t))) {
            Ok(x) => x,
            Err(e) => {
                w.skipped.push((dir.to_path_buf(), format!("unreadable: {e}")));
                continue;
            }
        };
        if ent.file_name() == "node_modules" {
            if ft.is_symlink() {
                w.skipped.push((dir.to_path_buf(), "node_modules is a symlink".into()));
            } else if ft.is_dir() {
                match std::fs::symlink_metadata(ent.path().join(".package-lock.json")) {
                    Ok(m) if m.is_file() => w.found.push(dir.to_path_buf()),
                    Ok(m) if m.file_type().is_symlink() => w.skipped.push((dir.to_path_buf(), "hidden lockfile is a symlink".into())),
                    Ok(_) => w.skipped.push((dir.to_path_buf(), "hidden lockfile is not a file".into())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => w.no_hidden_lock += 1,
                    // Not "no hidden lockfile": a node_modules this process
                    // cannot search may hold one, and its project would go
                    // unrecorded without a word.
                    Err(e) => w.skipped.push((dir.to_path_buf(), format!("hidden lockfile unreadable: {e}"))),
                }
            }
        }
        // pruned() is the one place that keeps node_modules from being
        // entered, for the roots and here.
        if ft.is_dir() {
            let p = ent.path();
            if !pruned(&p, store_root) {
                sub.push(p);
            }
        }
    }
    let rest = sub.into_par_iter().map(|d| walk(&d, store_root)).reduce(Walked::default, Walked::merge);
    w.merge(rest)
}

/// What `store adopt` found and recorded (or, dry run, would record).
#[derive(Debug, Default)]
pub struct AdoptReport {
    /// Directories looked at (node_modules and .git are not entered).
    pub dirs: usize,
    /// Projects found: a node_modules directory holding npm's hidden
    /// lockfile (`node_modules/.package-lock.json`).
    pub projects: usize,
    /// node_modules directories without a hidden lockfile (Yarn, npm 6).
    pub no_hidden_lock: usize,
    /// New records; existing records that gained entries.
    pub written: usize,
    pub extended: usize,
    /// Projects whose record already lists every store entry they use.
    pub complete: usize,
    /// Projects none of whose packages is in the store: nothing to record.
    pub no_entry: usize,
    /// Distinct store entries gc will keep for the projects found: those
    /// adopted plus those their existing records list.
    pub entries: usize,
    /// Distinct integrities their hidden lockfiles list that are not in the store.
    pub not_in_store: usize,
    /// What was skipped, and why.
    pub skipped: Vec<(PathBuf, String)>,
    /// Per project: what was done (`new`, `added`, `complete`, `none`),
    /// its entries in the store, the integrities it lists that are not.
    pub list: Vec<(PathBuf, &'static str, usize, usize)>,
}

/// `store adopt`: record the projects under `dirs` that have npm's hidden
/// lockfile, with the store entries it lists, so that `gc` keeps them. For
/// projects installed before installs wrote records (#30).
///
/// Errs on the side of keeping: a project installed by plain npm has a
/// hidden lockfile too, and its entries are kept as well (nothing can tell
/// cheaply whether a node_modules file is a clone of a store entry). A
/// project's existing record is never narrowed: adopting adds to it.
///
/// Walks without following symlinks; never enters a node_modules (nested
/// node_modules, and projects inside packages, are not projects), a `.git`,
/// or the store itself. A node_modules or hidden lockfile that is a symlink
/// is skipped (an install refuses a symlinked node_modules, and it may lead
/// out of `dirs`); so is anything unreadable. With `dry_run`, no record is
/// written. The store must be opened `exclusive` unless `dry_run`, so no
/// install rewrites a record while it is merged here.
pub fn adopt(store: &Store, dirs: &[PathBuf], dry_run: bool) -> Result<AdoptReport> {
    let mut r = AdoptReport::default();
    let in_store: HashSet<Integrity> = store.entries().into_iter().collect();
    let store_root = std::fs::canonicalize(&store.root).unwrap_or_else(|_| store.root.clone());
    let mut found: Vec<PathBuf> = vec![];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for d in dirs {
        let root = std::fs::canonicalize(d).with_context(|| format!("resolving {}", d.display()))?;
        if pruned(&root, &store_root) {
            continue;
        }
        let w = walk(&root, &store_root);
        r.dirs += w.dirs;
        r.no_hidden_lock += w.no_hidden_lock;
        r.skipped.extend(w.skipped);
        found.extend(w.found.into_iter().filter(|p| seen.insert(p.clone())));
    }

    let read: Vec<(PathBuf, std::result::Result<Vec<Integrity>, String>)> = found.into_par_iter().map(|p| {
        let l = read_hidden_lock(&p);
        (p, l)
    }).collect();
    let mut used_all: HashSet<Integrity> = HashSet::new();
    let mut missing: HashSet<&Integrity> = HashSet::new();
    for (dir, listed) in &read {
        let listed = match listed {
            Ok(l) => l,
            Err(why) => {
                r.skipped.push((dir.clone(), why.clone()));
                continue;
            }
        };
        r.projects += 1;
        let (here, absent): (Vec<&Integrity>, Vec<&Integrity>) = listed.iter().partition(|i| in_store.contains(*i));
        r.list.push((dir.clone(), "none", here.len(), absent.len()));
        missing.extend(absent);
        // gc keeps what the project's record already lists (adopting never
        // narrows it) as well as what is adopted now: count both.
        let old = store.recorded_entries(dir);
        used_all.extend(old.iter().flatten().filter_map(|k| Integrity::from_key(k)).filter(|i| in_store.contains(i)));
        if here.is_empty() {
            r.no_entry += 1;
            continue;
        }
        used_all.extend(here.iter().map(|i| (*i).clone()));
        let keys: Vec<String> = here.iter().map(|i| i.key()).collect();
        match old {
            Some(old) => {
                let old: HashSet<String> = old.into_iter().collect();
                if keys.iter().all(|k| old.contains(k)) {
                    r.complete += 1;
                    r.list.last_mut().unwrap().1 = "complete";
                    continue;
                }
                r.extended += 1;
                r.list.last_mut().unwrap().1 = "added";
            }
            None => {
                r.written += 1;
                r.list.last_mut().unwrap().1 = "new";
            }
        }
        if !dry_run {
            store.adopt_project(dir, keys).with_context(|| format!("recording {}", dir.display()))?;
        }
    }
    r.list.sort();
    r.entries = used_all.len();
    r.not_in_store = missing.len();
    Ok(r)
}

/// `log`: the trial log (trial::log_path()). With `dry_run`, nothing is
/// removed, records included. The store must be opened `exclusive` unless
/// `dry_run`.
pub fn gc(store: &Store, log: &Path, dry_run: bool) -> Result<Report> {
    let mut r = Report::default();
    let mut live: HashSet<Integrity> = HashSet::new();
    let mut recorded: HashSet<PathBuf> = HashSet::new();
    let mut dropped_used: HashSet<Integrity> = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(store.root.join("projects")) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(rec) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
            let Some(dir) = rec.get("path").and_then(|x| x.as_str()).map(PathBuf::from) else { continue };
            let listed = rec.get("entries").and_then(|e| e.as_array()).into_iter().flatten().filter_map(|k| k.as_str().and_then(Integrity::from_key));
            let state = node_modules_state(&dir);
            if let Err(why) = state {
                live.extend(listed);
                recorded.insert(dir.clone());
                r.records_unchecked.push((dir, why));
            } else if state == Ok(true) {
                r.projects.0 += 1;
                live.extend(listed);
                recorded.insert(dir);
            } else {
                dropped_used.extend(listed);
                r.records_dropped.push(dir);
                if !dry_run {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
    // Projects installed by a build that wrote no record.
    let logged: HashSet<PathBuf> = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["outcome"] == "installed")
        // npm's install root (a workspace root, a --prefix) where the run
        // has one; older runs installed into `dir`.
        .filter_map(|v| v["root"].as_str().or(v["dir"].as_str()).map(PathBuf::from))
        .collect();
    for dir in logged {
        let canon = std::fs::canonicalize(&dir).unwrap_or(dir);
        if recorded.contains(&canon) || !has_node_modules(&canon) {
            continue;
        }
        let used = hidden_lock_entries(&canon);
        if !used.is_empty() {
            r.projects.1 += 1;
            live.extend(used);
        }
    }

    let all = store.entries();
    r.entries = all.len();
    let unused: Vec<&Integrity> = all.iter().filter(|i| !live.contains(*i)).collect();
    // An entry extracted moments ago may belong to an install that has not
    // written its record yet on a store where flock does not reach.
    let (recent, old): (Vec<&Integrity>, Vec<&Integrity>) = unused.into_iter().partition(|i| young(&store.dir_for(i)));
    r.records_dropped.sort();
    r.records_unchecked.sort();
    r.recent = recent.len();
    r.bytes_removed = old.iter().map(|i| store.entry_size(i)).sum();
    r.removed = old.len();
    r.dropped_entries_removed = old.iter().filter(|i| dropped_used.contains(*i)).count();
    if !dry_run {
        old.par_iter().try_for_each(|i| store.remove_entry(i))?;
        store.remove_orphan_locks();
    }
    Ok(r)
}

/// How many records without a project `store gc` lists by path.
pub const DROPPED_SHOWN: usize = 5;

/// What `store gc` says about records without a project (from `r`:
/// Report::records_dropped and dropped_entries_removed): how many, the first
/// few paths (quoted and escaped: a path with a newline stays one line), and
/// that `store adopt` keeps a moved project's entries, before gc. Nothing
/// when there are none. A real gc has dropped the records already, so it
/// says "before the next gc", and how many entries only they used it removed
/// (none when they share them with other projects, or are younger than the
/// grace period); adopt keeps whatever of theirs is still in the store.
pub fn dropped_note(r: &Report, dry_run: bool) -> Vec<String> {
    let dropped = &r.records_dropped;
    if dropped.is_empty() {
        return vec![];
    }
    let mut out = vec![format!(
        "records without a project {}: {} (no node_modules at the recorded path: the project was moved, renamed, or copied and the original deleted, or deleted, or its node_modules removed)",
        if dry_run { "to drop" } else { "dropped" },
        dropped.len()
    )];
    out.extend(dropped.iter().take(DROPPED_SHOWN).map(|p| format!("  {p:?}")));
    if dropped.len() > DROPPED_SHOWN {
        out.push(format!("  ... and {} more", dropped.len() - DROPPED_SHOWN));
    }
    let n = r.dropped_entries_removed;
    let s = if n == 1 { "y" } else { "ies" };
    out.push(if dry_run {
        format!("if these projects were moved or renamed (or copied and the original deleted), run `matchapack store adopt <where they are now>` before gc: it would remove {n} entr{s} only they use")
    } else if n == 0 {
        "if these projects were moved or renamed (or copied and the original deleted), run `matchapack store adopt <where they are now>` before the next gc: this one removed none of their entries (other projects use them, or they are younger than an hour), and adopt keeps them".to_string()
    } else {
        format!("if these projects were moved or renamed (or copied and the original deleted), run `matchapack store adopt <where they are now>` before the next gc: this one removed {n} entr{s} only they used (their next install extracts those again); adopt keeps what of theirs is still in the store")
    });
    out
}

/// What `store gc` says about the records it kept because it could not
/// check them (Report::records_unchecked): how many, and the first few with
/// why. Nothing when there are none.
pub fn unchecked_note(r: &Report) -> Vec<String> {
    let u = &r.records_unchecked;
    if u.is_empty() {
        return vec![];
    }
    let mut out = vec![format!("records that could not be checked (kept, with their entries): {}", u.len())];
    out.extend(u.iter().take(DROPPED_SHOWN).map(|(p, why)| format!("  {p:?}: {why}")));
    if u.len() > DROPPED_SHOWN {
        out.push(format!("  ... and {} more", u.len() - DROPPED_SHOWN));
    }
    out
}

fn young(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|d| d.as_secs() <= GRACE_SECS).unwrap_or(true))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{age, cached_tarball, scratch};
    use base64::Engine;

    #[test]
    fn gc_keeps_what_existing_projects_use() {
        let d = scratch("gc");
        let cacache = d.join("cacache");
        let mk = |n: &str| cached_tarball(&cacache, &[("index.js", 0o644, n)]);
        let (used, gone, logged, fresh, stale) = (mk("a"), mk("b"), mk("c"), mk("d"), mk("e"));
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        for i in [&used, &gone, &logged, &fresh, &stale] {
            store.ensure(i, "http://x").unwrap();
            if i != &fresh {
                age(&store.dir_for(i), 2 * GRACE_SECS as i64);
            }
        }
        // A project with node_modules, recorded; one whose directory is gone.
        let a = d.join("proj-a");
        std::fs::create_dir_all(a.join("node_modules")).unwrap();
        store.record_project(&std::fs::canonicalize(&a).unwrap(), &[used.clone()]).unwrap();
        store.record_project(&d.join("proj-removed"), &[gone.clone()]).unwrap();
        // A project installed before records existed: only in the trial log,
        // its entries in the hidden lockfile.
        let c = d.join("proj-c");
        std::fs::create_dir_all(c.join("node_modules")).unwrap();
        let sri = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(hex::decode(&logged.hex).unwrap()));
        std::fs::write(c.join("node_modules/.package-lock.json"), format!(r#"{{"packages":{{"node_modules/c":{{"integrity":"{sri}"}}}}}}"#)).unwrap();
        let log = d.join("log.jsonl");
        // Started in a workspace member: npm's root (`root`) is the project,
        // not the start directory (`dir`).
        std::fs::write(&log, format!("{}\n{}\n", serde_json::json!({"outcome":"installed","dir":c.join("packages/m"),"root":c}), serde_json::json!({"outcome":"declined","dir":d.join("x")}))).unwrap();
        drop(store);

        let store = Store::exclusive(d.join("store"), cacache.clone()).unwrap();
        let dry = gc(&store, &log, true).unwrap();
        assert_eq!((dry.projects, dry.removed, dry.recent), ((1, 1), 2, 1));
        assert_eq!(dry.records_dropped, [d.join("proj-removed")], "the record without a project, by its path");
        assert_eq!(dry.dropped_entries_removed, 1, "`gone`, which only it used");
        assert_eq!(store.entries().len(), 5, "a dry run removes nothing");
        assert_eq!(std::fs::read_dir(d.join("store/projects")).unwrap().count(), 2, "nor a record");

        let r = gc(&store, &log, false).unwrap();
        assert_eq!(r.removed, 2);
        let mut left = store.entries();
        left.sort();
        let mut want = vec![used, logged, fresh];
        want.sort();
        assert_eq!(left, want, "kept: recorded, logged, recent; removed: the gone project's and the unused old one");
        assert!(!store.dir_for(&gone).exists() && !store.dir_for(&stale).exists());
        assert_eq!(std::fs::read_dir(d.join("store/projects")).unwrap().count(), 1, "the gone project's record is dropped");
        assert_eq!((r.records_dropped.as_slice(), r.dropped_entries_removed), ([d.join("proj-removed")].as_slice(), 1));
        assert_eq!(std::fs::read_dir(d.join("store/tmp")).unwrap().count(), 0, "nothing left in tmp");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dropped_note_lists_the_first_paths_and_names_adopt() {
        let rep = |paths: &[PathBuf], removed: usize| Report { records_dropped: paths.to_vec(), dropped_entries_removed: removed, ..Default::default() };
        assert!(dropped_note(&rep(&[], 0), true).is_empty(), "nothing to say without records without a project");
        let mut paths: Vec<PathBuf> = (0..7).map(|n| PathBuf::from(format!("/p/{n}"))).collect();
        paths[1] = PathBuf::from("/p/1\n  /p/fake");
        let dry = dropped_note(&rep(&paths, 3), true);
        assert!(dry[0].starts_with("records without a project to drop: 7 ("), "{}", dry[0]);
        assert_eq!(&dry[1..=DROPPED_SHOWN], ["  \"/p/0\"", "  \"/p/1\\n  /p/fake\"", "  \"/p/2\"", "  \"/p/3\"", "  \"/p/4\""], "quoted; a newline stays escaped, on one line");
        assert_eq!(dry[DROPPED_SHOWN + 1], "  ... and 2 more");
        assert_eq!(dry.last().unwrap(), "if these projects were moved or renamed (or copied and the original deleted), run `matchapack store adopt <where they are now>` before gc: it would remove 3 entries only they use");
        assert_eq!(dry.len(), DROPPED_SHOWN + 3);
        let one = dropped_note(&rep(&paths[..1], 1), false);
        assert_eq!(one.len(), 3, "count, the path, the advice: {one:?}");
        assert!(one[0].starts_with("records without a project dropped: 1 (") && one[1] == "  \"/p/0\"", "{one:?}");
        assert!(one[2].contains("before the next gc: this one removed 1 entry only they used") && one[2].contains("adopt keeps what of theirs is still in the store"), "{}", one[2]);
        let none = dropped_note(&rep(&paths[..1], 0), false);
        assert!(none[2].contains("before the next gc: this one removed none of their entries"), "the real run says what it removed: {}", none[2]);
    }

    /// A record whose node_modules cannot be looked at (a parent directory
    /// this process cannot search) is kept with its entries and listed as
    /// could not be checked, not dropped as a project that is gone; so is
    /// one on an unmounted volume (macOS).
    #[test]
    fn gc_keeps_records_it_cannot_check() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::fs::canonicalize(scratch("gc-unchecked")).unwrap();
        let cacache = d.join("cacache");
        let a = cached_tarball(&cacache, &[("index.js", 0o644, "a")]);
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        store.ensure(&a, "http://x").unwrap();
        age(&store.dir_for(&a), 2 * GRACE_SECS as i64);
        let p = d.join("locked/p");
        std::fs::create_dir_all(p.join("node_modules")).unwrap();
        store.record_project(&p, &[a.clone()]).unwrap();
        std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::metadata(p.join("node_modules")).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        drop(store);
        let store = Store::exclusive(d.join("store"), cacache.clone()).unwrap();
        if denied {
            let r = gc(&store, &d.join("no-log"), false).unwrap();
            assert!(r.records_dropped.is_empty(), "{:?}", r.records_dropped);
            assert_eq!(r.records_unchecked.len(), 1);
            assert_eq!(r.records_unchecked[0].0, p);
            assert_eq!((r.removed, store.dir_for(&a).exists()), (0, true), "its entry is kept");
            assert_eq!(std::fs::read_dir(d.join("store/projects")).unwrap().count(), 1, "and its record");
            let note = unchecked_note(&r);
            assert!(note[0] == "records that could not be checked (kept, with their entries): 1" && note[1].starts_with(&format!("  {p:?}: ")), "{note:?}");
        } else {
            eprintln!("skipped: a mode-000 directory is searchable here (root?)");
        }
        std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(node_modules_state(&p), Ok(true));
        assert_eq!(node_modules_state(&d.join("gone")), Ok(false));
        assert!(unchecked_note(&Report::default()).is_empty());
        if cfg!(target_os = "macos") {
            let e = node_modules_state(Path::new("/Volumes/matchapack-test-no-such-volume/p")).unwrap_err();
            assert_eq!(e, "/Volumes/matchapack-test-no-such-volume is not mounted");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A hidden lockfile listing `ints`, at `<dir>/node_modules/.package-lock.json`.
    fn hidden(dir: &Path, ints: &[&Integrity]) {
        let pkgs: serde_json::Map<String, Value> =
            ints.iter().enumerate().map(|(n, i)| (format!("node_modules/p{n}"), serde_json::json!({ "integrity": i.sri() }))).collect();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/.package-lock.json"), serde_json::json!({ "packages": pkgs }).to_string()).unwrap();
    }

    fn record_of(store: &Store, dir: &Path) -> Vec<String> {
        let mut v = store.recorded_entries(dir).unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn adopt_records_projects_and_follows_nothing_out() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let d = std::fs::canonicalize(scratch("adopt")).unwrap();
        let cacache = d.join("cacache");
        let mk = |n: &str| cached_tarball(&cacache, &[("index.js", 0o644, n)]);
        let (a, b, c, absent) = (mk("a"), mk("b"), mk("c"), mk("absent"));
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        for i in [&a, &b, &c] {
            store.ensure(i, "http://x").unwrap();
            age(&store.dir_for(i), 2 * GRACE_SECS as i64);
        }
        let root = d.join("root");
        let out = d.join("outside");
        // p1: a project (one integrity not in the store); a package inside
        // its node_modules has a project of its own, never entered.
        hidden(&root.join("p1"), &[&a, &absent]);
        hidden(&root.join("p1/node_modules/pkg"), &[&b]);
        // Outside the scanned directory: a project using b, reached only
        // through symlinks: a symlinked directory, a symlinked node_modules,
        // a symlinked hidden lockfile, and a loop.
        hidden(&out.join("q"), &[&b]);
        symlink(&out, root.join("link")).unwrap();
        std::fs::create_dir_all(root.join("p2")).unwrap();
        symlink(out.join("q/node_modules"), root.join("p2/node_modules")).unwrap();
        std::fs::create_dir_all(root.join("p3/node_modules")).unwrap();
        symlink(out.join("q/node_modules/.package-lock.json"), root.join("p3/node_modules/.package-lock.json")).unwrap();
        symlink(&root, root.join("p1/loop")).unwrap();
        // Inside .git: not entered.
        hidden(&root.join(".git/modules/x"), &[&b]);
        // p4: installed and recorded with c; its hidden lockfile lists a.
        hidden(&root.join("p4"), &[&a]);
        store.record_project(&root.join("p4"), &[c.clone()]).unwrap();
        // A Yarn project: node_modules without a hidden lockfile.
        std::fs::create_dir_all(root.join("y/node_modules/left-pad")).unwrap();
        // A directory nobody can read.
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();

        let records = || std::fs::read_dir(d.join("store/projects")).unwrap().count();
        let dry = adopt(&store, &[root.clone()], true).unwrap();
        assert_eq!((dry.projects, dry.written, dry.extended, dry.complete, dry.no_hidden_lock), (2, 1, 1, 0, 1));
        assert_eq!((dry.entries, dry.not_in_store), (2, 1), "kept: a (adopted) and c (p4's record); `absent` is listed but not in the store");
        let mut why: Vec<&str> = dry.skipped.iter().map(|(_, w)| w.split(':').next().unwrap()).collect();
        why.sort();
        // Root (CAP_DAC_OVERRIDE, a Docker run) reads a mode-000 directory.
        let mut want = vec!["hidden lockfile is a symlink", "node_modules is a symlink"];
        if std::fs::read_dir(root.join("locked")).is_err() {
            want.push("unreadable");
        }
        assert_eq!(why, want);
        assert_eq!(adopt(&store, &[root.join("p1/node_modules")], true).unwrap().projects, 0, "a node_modules named as the directory is not entered either");
        assert_eq!(records(), 1, "a dry run writes no record");
        assert_eq!(record_of(&store, &root.join("p4")), [c.key()], "nor changes one");

        drop(store);
        let store = Store::exclusive(d.join("store"), cacache.clone()).unwrap();
        let r = adopt(&store, &[root.clone()], false).unwrap();
        assert_eq!((r.written, r.extended, r.entries), (1, 1, 2));
        assert_eq!(records(), 2, "p1 and p4 only");
        assert_eq!(record_of(&store, &root.join("p1")), [a.key()], "only entries in the store");
        let mut both = vec![a.key(), c.key()];
        both.sort();
        assert_eq!(record_of(&store, &root.join("p4")), both, "an existing record is added to, never narrowed");
        let again = adopt(&store, &[root.clone(), root.join("p1")], false).unwrap();
        assert_eq!((again.projects, again.written, again.extended, again.complete, again.entries), (2, 0, 0, 2, 2), "adopting twice changes nothing");

        // gc keeps what adopted projects use and removes b, which only
        // projects behind symlinks, inside a package and inside .git use.
        let g = gc(&store, &d.join("no-log"), false).unwrap();
        assert_eq!(g.removed, 1);
        assert!(!store.dir_for(&b).exists() && store.dir_for(&a).exists() && store.dir_for(&c).exists());
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn adopt_reports_a_node_modules_it_cannot_search() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::fs::canonicalize(scratch("adopt-nosearch")).unwrap();
        let cacache = d.join("cacache");
        let a = cached_tarball(&cacache, &[("index.js", 0o644, "a")]);
        let store = Store::new(d.join("store"), cacache.clone()).unwrap();
        store.ensure(&a, "http://x").unwrap();
        // A project whose node_modules this process cannot search: its
        // hidden lockfile cannot even be looked at.
        let p = d.join("root/p");
        hidden(&p, &[&a]);
        let nm = p.join("node_modules");
        std::fs::set_permissions(&nm, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::symlink_metadata(nm.join(".package-lock.json")).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        if denied {
            let r = adopt(&store, &[d.join("root")], true).unwrap();
            assert_eq!((r.projects, r.no_hidden_lock), (0, 0), "not counted as a node_modules without a hidden lockfile");
            assert_eq!(r.skipped.len(), 1);
            assert_eq!(r.skipped[0].0, p, "reported with the project's path");
            assert!(r.skipped[0].1.starts_with("hidden lockfile unreadable: "), "{}", r.skipped[0].1);
        } else {
            // Root (CAP_DAC_OVERRIDE) searches it anyway: nothing to test.
            eprintln!("skipped: a mode-000 node_modules is searchable here (root?)");
        }
        std::fs::set_permissions(&nm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
