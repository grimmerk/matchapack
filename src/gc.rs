//! `thinclone store gc`: remove the store entries no existing project uses.
//!
//! Reference-based, not age-based. Removing an entry never breaks an installed
//! node_modules (clones are independent files), but while a node_modules still
//! shares an entry's blocks, removing it frees nothing and costs that
//! project's next install a re-extraction. So an entry stays while any project
//! that was installed from it still has its node_modules.
//!
//! Which projects: every install records its directory and entries under
//! `<store>/projects/` (Store::record_project). Projects installed before
//! records existed are found through the trial log (outcome `installed`),
//! with the entries their hidden lockfile
//! (`node_modules/.package-lock.json`) lists.

use crate::store::{Integrity, Store, GRACE_SECS};
use anyhow::Result;
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Report {
    /// Projects whose entries are kept: (from records, from the trial log).
    pub projects: (usize, usize),
    /// Records of projects whose directory or node_modules is gone.
    pub records_dropped: usize,
    pub entries: usize,
    pub removed: usize,
    pub bytes_removed: u64,
    /// Unused, but younger than the grace period.
    pub recent: usize,
}

fn has_node_modules(dir: &Path) -> bool {
    dir.join("node_modules").is_dir()
}

/// The entries a hidden lockfile lists.
fn hidden_lock_entries(dir: &Path) -> Vec<Integrity> {
    let Ok(text) = std::fs::read_to_string(dir.join("node_modules/.package-lock.json")) else { return vec![] };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return vec![] };
    let Some(pkgs) = v.get("packages").and_then(|p| p.as_object()) else { return vec![] };
    pkgs.values().filter_map(|p| p.get("integrity")?.as_str()).filter_map(|s| Integrity::parse(s).ok()).collect()
}

/// `log`: the trial log (trial::log_path()). With `dry_run`, nothing is
/// removed, records included. The store must be opened `exclusive` unless
/// `dry_run`.
pub fn gc(store: &Store, log: &Path, dry_run: bool) -> Result<Report> {
    let mut r = Report::default();
    let mut live: HashSet<Integrity> = HashSet::new();
    let mut recorded: HashSet<PathBuf> = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(store.root.join("projects")) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(rec) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
            let Some(dir) = rec.get("path").and_then(|x| x.as_str()).map(PathBuf::from) else { continue };
            if has_node_modules(&dir) {
                r.projects.0 += 1;
                for k in rec.get("entries").and_then(|e| e.as_array()).into_iter().flatten() {
                    if let Some(i) = k.as_str().and_then(Integrity::from_key) {
                        live.insert(i);
                    }
                }
                recorded.insert(dir);
            } else {
                r.records_dropped += 1;
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
        .filter_map(|v| v["dir"].as_str().map(PathBuf::from))
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
    r.recent = recent.len();
    r.bytes_removed = old.iter().map(|i| store.entry_size(i)).sum();
    r.removed = old.len();
    if !dry_run {
        old.par_iter().try_for_each(|i| store.remove_entry(i))?;
        store.remove_orphan_locks();
    }
    Ok(r)
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
        std::fs::write(&log, format!("{}\n{}\n", serde_json::json!({"outcome":"installed","dir":c}), serde_json::json!({"outcome":"declined","dir":d.join("x")}))).unwrap();
        drop(store);

        let store = Store::exclusive(d.join("store"), cacache.clone()).unwrap();
        let dry = gc(&store, &log, true).unwrap();
        assert_eq!((dry.projects, dry.records_dropped, dry.removed, dry.recent), ((1, 1), 1, 2, 1));
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
        assert_eq!(std::fs::read_dir(d.join("store/tmp")).unwrap().count(), 0, "nothing left in tmp");
        let _ = std::fs::remove_dir_all(&d);
    }
}
