//! What a later sample needs to replay a run (scripts/verify-sample.sh, issue
//! #77): whether the install root's `node_modules` was there before the run,
//! and a SHA-256 of the lockfile and of each manifest before and after it.
//! A run that started with no `node_modules` has a known before-state (the
//! manifests and the lockfile), so it can be replayed on copies, plain and
//! through matchapack, like a full install (NOTES "Sampled verification").
//!
//! The files are small (a lockfile of a large project is a few MiB): each is
//! read and hashed once before the run and once after it. Workspaces are
//! found from the root `package.json`'s `workspaces` (an array, or Yarn's
//! `{"packages": [...]}`) with `*`, `?` and `**` path segments, as the
//! common globs are written; a pattern beyond that (braces, classes) finds
//! fewer workspaces, and a replay then checks fewer files, never others.

use serde_json::{json, Value};
use sha2::Digest;
use std::path::{Path, PathBuf};

/// npm's lockfiles at the install root (npm-shrinkwrap.json wins when both are there).
pub const NPM_LOCKFILES: &[&str] = &["package-lock.json", "npm-shrinkwrap.json"];
pub const YARN_LOCKFILES: &[&str] = &["yarn.lock"];

/// At most this many directories are listed while expanding the workspace
/// globs (a `**` over a large tree must not slow an install down).
const MAX_LISTED: usize = 2000;

/// The root's `node_modules` before a run: "absent", "empty" (a directory
/// with no entries) or "present" (anything else, an unreadable one too).
pub fn node_modules_state(root: &Path) -> &'static str {
    let nm = root.join("node_modules");
    match std::fs::symlink_metadata(&nm) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent",
        Ok(m) if m.is_dir() => match std::fs::read_dir(&nm).map(|mut it| it.next().is_none()) {
            Ok(true) => "empty",
            _ => "present",
        },
        _ => "present",
    }
}

/// `{relative path: SHA-256 hex, or null when the file is not there}` for
/// the lockfiles named, the root `package.json` and each workspace's
/// `package.json`. A file that cannot be read is `"unreadable"`, which no
/// file's hash equals.
pub fn files_state(root: &Path, lockfiles: &[&str]) -> Value {
    let mut names: Vec<String> = lockfiles.iter().map(|s| s.to_string()).collect();
    names.push("package.json".into());
    names.extend(workspace_dirs(root).into_iter().map(|d| format!("{d}/package.json")));
    let mut m = serde_json::Map::new();
    for n in names {
        let v = match std::fs::read(root.join(&n)) {
            Ok(b) => json!(hex::encode(sha2::Sha256::digest(&b))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(_) => json!("unreadable"),
        };
        m.insert(n, v);
    }
    Value::Object(m)
}

/// The workspaces of the project at `root` (directories holding a
/// package.json, relative, sorted), from its package.json's `workspaces`.
pub fn workspace_dirs(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else { return vec![] };
    let Ok(pj) = serde_json::from_str::<Value>(&text) else { return vec![] };
    let list = match &pj["workspaces"] {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o.get("packages").and_then(|p| p.as_array()).cloned().unwrap_or_default(),
        _ => vec![],
    };
    let mut listed = 0usize;
    let (mut keep, mut drop) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    for p in list.iter().filter_map(|p| p.as_str()) {
        let (neg, pat) = match p.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, p),
        };
        let pat = pat.trim_start_matches("./").trim_end_matches('/');
        let segs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        if pat.starts_with('/') || segs.is_empty() || segs.contains(&"..") {
            continue;
        }
        let mut found = vec![];
        expand(root, &segs, &mut found, &mut listed);
        for d in found {
            if d.join("package.json").is_file() {
                if let Ok(rel) = d.strip_prefix(root) {
                    let rel = rel.to_string_lossy().to_string();
                    if neg {
                        drop.insert(rel);
                    } else {
                        keep.insert(rel);
                    }
                }
            }
        }
    }
    keep.into_iter().filter(|d| !d.is_empty() && !drop.contains(d)).collect()
}

fn expand(dir: &Path, segs: &[&str], out: &mut Vec<PathBuf>, listed: &mut usize) {
    let Some((seg, rest)) = segs.split_first() else {
        out.push(dir.to_path_buf());
        return;
    };
    let glob = seg.contains(['*', '?']);
    if !glob {
        let d = dir.join(seg);
        if d.is_dir() {
            expand(&d, rest, out, listed);
        }
        return;
    }
    if *seg == "**" {
        expand(dir, rest, out, listed);
    }
    if *listed >= MAX_LISTED {
        return;
    }
    *listed += 1;
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut subs: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok().map(|n| (n, e.path())))
        .filter(|(n, _)| n != "node_modules" && !n.starts_with('.'))
        .collect();
    subs.sort();
    for (name, p) in subs {
        if *seg == "**" {
            expand(&p, segs, out, listed);
        } else if matches(seg.as_bytes(), name.as_bytes()) {
            expand(&p, rest, out, listed);
        }
    }
}

/// `*` (any run of characters) and `?` (one character) against a name.
fn matches(pat: &[u8], name: &[u8]) -> bool {
    match (pat.first(), name.first()) {
        (None, None) => true,
        (Some(b'*'), _) => matches(&pat[1..], name) || (!name.is_empty() && matches(pat, &name[1..])),
        (Some(b'?'), Some(_)) => matches(&pat[1..], &name[1..]),
        (Some(a), Some(b)) if a == b => matches(&pat[1..], &name[1..]),
        _ => false,
    }
}

/// The state before a run at `root`: `{"node_modules": …, "files": …}`.
pub fn before(root: &Path, lockfiles: &[&str]) -> Value {
    json!({ "node_modules": node_modules_state(root), "files": files_state(root, lockfiles) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("matchapack-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn node_modules_absent_empty_present() {
        let d = scratch("nm");
        assert_eq!(node_modules_state(&d), "absent");
        std::fs::create_dir(d.join("node_modules")).unwrap();
        assert_eq!(node_modules_state(&d), "empty");
        std::fs::write(d.join("node_modules/.package-lock.json"), "{}").unwrap();
        assert_eq!(node_modules_state(&d), "present", "any entry, a hidden file too");
        let _ = std::fs::remove_dir_all(&d);
        let d = scratch("nm-file");
        std::fs::write(d.join("node_modules"), "").unwrap();
        assert_eq!(node_modules_state(&d), "present", "not a directory");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hashes_of_the_lockfile_and_the_manifests() {
        let d = scratch("files");
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/*", "tools/**", "!packages/skip", "../out", "/abs"]}"#).unwrap();
        std::fs::write(d.join("yarn.lock"), "abc").unwrap();
        for w in ["packages/a", "packages/b", "packages/skip", "packages/nopj", "tools/x/y", "tools/node_modules/z", "packages/node_modules/q"] {
            std::fs::create_dir_all(d.join(w)).unwrap();
            if !w.ends_with("nopj") {
                std::fs::write(d.join(w).join("package.json"), w).unwrap();
            }
        }
        assert_eq!(workspace_dirs(&d), ["packages/a", "packages/b", "tools/x/y"]);
        let f = files_state(&d, YARN_LOCKFILES);
        assert_eq!(f["yarn.lock"], json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "sha256 of abc");
        assert_eq!(f["packages/a/package.json"], json!(hex::encode(sha2::Sha256::digest(b"packages/a"))));
        let keys: Vec<&String> = f.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["yarn.lock", "package.json", "packages/a/package.json", "packages/b/package.json", "tools/x/y/package.json"]);
        let n = files_state(&d, NPM_LOCKFILES);
        assert_eq!((n["package-lock.json"].clone(), n["npm-shrinkwrap.json"].clone()), (Value::Null, Value::Null));
        // Yarn's object form.
        std::fs::write(d.join("package.json"), r#"{"workspaces": {"packages": ["packages/?"], "nohoist": ["**"]}}"#).unwrap();
        assert_eq!(workspace_dirs(&d), ["packages/a", "packages/b"]);
        std::fs::write(d.join("package.json"), "not json").unwrap();
        assert!(workspace_dirs(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn glob_segments() {
        assert!(matches(b"*", b"abc") && matches(b"a*c", b"abbc") && matches(b"a?c", b"abc") && matches(b"*", b""));
        assert!(!matches(b"a?c", b"ac") && !matches(b"b*", b"abc"));
    }
}
