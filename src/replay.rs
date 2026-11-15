//! What a later sample needs to replay a run (scripts/verify-sample.sh, issue
//! #77): whether `node_modules` was there before the run, and a SHA-256 of
//! the lockfile and of each manifest before and after it. A run that started
//! with no `node_modules` has a known before-state (the manifests and the
//! lockfile), so it can be replayed on copies, plain and through matchapack,
//! like a full install (NOTES "Sampled verification of real runs").
//!
//! The files are small (a lockfile of a large project is a few MiB): each is
//! read and hashed once before the run and once after it. Workspaces are
//! found from the root `package.json`'s `workspaces` (an array, or Yarn's
//! `{"packages": [...]}`) with `*`, `?` and `**` path segments and `{a,b}`
//! alternatives, as Yarn's and npm's globs expand them. A pattern beyond that
//! (character classes, extended globs, nested braces), or one whose listing
//! budget runs out, makes the state incomplete (`complete: false`): a sample
//! does not replay a run whose file set is not known.

use serde_json::{json, Value};
use sha2::Digest;
use std::path::{Path, PathBuf};

/// npm's lockfiles at the install root (npm-shrinkwrap.json wins when both are there).
pub const NPM_LOCKFILES: &[&str] = &["package-lock.json", "npm-shrinkwrap.json"];
pub const YARN_LOCKFILES: &[&str] = &["yarn.lock"];

/// At most this many directories are listed per workspace pattern while
/// expanding it (a `**` over a large tree must not slow an install down);
/// a pattern that reaches it makes the state incomplete.
const MAX_LISTED: usize = 2000;

/// At most this many alternatives per brace pattern.
const MAX_BRACES: usize = 256;

/// The project's `node_modules` before a run: "absent", "empty" (a
/// directory with no entries) or "present": the root's or any workspace's
/// has an entry (or is not a directory, or cannot be read). A workspace's
/// own `node_modules` counts: npm and Yarn leave packages there, and a
/// replay from clean copies would not have them.
pub fn node_modules_state(root: &Path, workspaces: &[String]) -> &'static str {
    let one = |nm: &Path| match std::fs::symlink_metadata(nm) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent",
        Ok(m) if m.is_dir() => match std::fs::read_dir(nm).map(|mut it| it.next().is_none()) {
            Ok(true) => "empty",
            _ => "present",
        },
        _ => "present",
    };
    let at_root = one(&root.join("node_modules"));
    if at_root == "present" || workspaces.iter().any(|w| one(&root.join(w).join("node_modules")) == "present") {
        return "present";
    }
    at_root
}

/// The files a replay depends on and whether that set is complete:
/// `({relative path: SHA-256 hex, or null when the file is not there}, complete)`
/// for the lockfiles named, the root `package.json` and each workspace's
/// `package.json`. A file that cannot be read is `"unreadable"`, which no
/// file's hash equals.
pub fn files_state(root: &Path, lockfiles: &[&str]) -> (Value, bool) {
    let (dirs, complete) = workspaces(root);
    (hashes(root, lockfiles, &dirs), complete)
}

fn hashes(root: &Path, lockfiles: &[&str], dirs: &[String]) -> Value {
    let mut names: Vec<String> = lockfiles.iter().map(|s| s.to_string()).collect();
    names.push("package.json".into());
    names.extend(dirs.iter().map(|d| format!("{d}/package.json")));
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
/// package.json, relative, sorted) from its package.json's `workspaces`,
/// and whether every pattern could be expanded in full.
pub fn workspaces(root: &Path) -> (Vec<String>, bool) {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else { return (vec![], true) };
    let Ok(pj) = serde_json::from_str::<Value>(&text) else { return (vec![], true) };
    let list = match &pj["workspaces"] {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o.get("packages").and_then(|p| p.as_array()).cloned().unwrap_or_default(),
        Value::Null => vec![],
        _ => return (vec![], false),
    };
    let mut complete = true;
    let (mut keep, mut drop) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    for p in list.iter() {
        let Some(p) = p.as_str() else {
            complete = false;
            continue;
        };
        let (neg, pat) = match p.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, p),
        };
        let Some(alternatives) = expand_braces(pat) else {
            complete = false;
            continue;
        };
        for pat in alternatives {
            let pat = pat.trim_start_matches("./").trim_end_matches('/');
            let segs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
            if pat.starts_with('/') || segs.is_empty() || segs.contains(&"..") {
                // Outside the project: npm and Yarn do not take it either.
                continue;
            }
            if segs.iter().any(|s| s.contains(['[', ']', '(', ')', '{', '}'])) {
                complete = false;
                continue;
            }
            // A budget per pattern: one large pattern cannot starve another.
            let mut listed = 0usize;
            let mut found = vec![];
            expand(root, &segs, &mut found, &mut listed);
            if listed >= MAX_LISTED {
                complete = false;
            }
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
    }
    (keep.into_iter().filter(|d| !d.is_empty() && !drop.contains(d)).collect(), complete)
}

/// `a/{b,c}/d` → `a/b/d`, `a/c/d` (every group, left to right); a pattern
/// without braces → itself. None for what this does not expand: nested or
/// unbalanced braces, or more than MAX_BRACES alternatives.
fn expand_braces(p: &str) -> Option<Vec<String>> {
    let Some(open) = p.find('{') else {
        return (!p.contains('}')).then(|| vec![p.to_string()]);
    };
    let close = open + p[open..].find('}')?;
    let inner = &p[open + 1..close];
    if inner.contains('{') {
        return None;
    }
    let (head, tail) = (&p[..open], &p[close + 1..]);
    let mut out = vec![];
    for alt in inner.split(',') {
        for rest in expand_braces(tail)? {
            out.push(format!("{head}{alt}{rest}"));
            if out.len() > MAX_BRACES {
                return None;
            }
        }
    }
    Some(out)
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

/// The state at `root` now: `{"node_modules": …, "files": …, "complete": …}`
/// (before a run; `matchapack replay-state` prints it for the sampler).
pub fn state(root: &Path, lockfiles: &[&str]) -> Value {
    let (dirs, complete) = workspaces(root);
    json!({ "node_modules": node_modules_state(root, &dirs), "files": hashes(root, lockfiles, &dirs), "complete": complete })
}

/// The record's fields for a run whose state before it was `before`
/// (`state`): `node_modules_before`, `files_before`, `files_after` (now) and
/// `files_complete` (both sets complete).
pub fn record_fields(root: &Path, lockfiles: &[&str], before: &Value) -> serde_json::Map<String, Value> {
    let (after, complete_after) = files_state(root, lockfiles);
    let mut m = serde_json::Map::new();
    m.insert("node_modules_before".into(), before["node_modules"].clone());
    m.insert("files_before".into(), before["files"].clone());
    m.insert("files_after".into(), after);
    m.insert("files_complete".into(), json!(before["complete"] == true && complete_after));
    m
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
        let none: Vec<String> = vec![];
        assert_eq!(node_modules_state(&d, &none), "absent");
        std::fs::create_dir(d.join("node_modules")).unwrap();
        assert_eq!(node_modules_state(&d, &none), "empty");
        // A workspace's own node_modules: empty is fine, an entry is not.
        let ws = vec!["packages/a".to_string()];
        std::fs::create_dir_all(d.join("packages/a/node_modules")).unwrap();
        assert_eq!(node_modules_state(&d, &ws), "empty");
        std::fs::create_dir(d.join("packages/a/node_modules/x")).unwrap();
        assert_eq!(node_modules_state(&d, &ws), "present", "a package in a workspace's node_modules");
        assert_eq!(node_modules_state(&d, &none), "empty", "not a workspace: not looked at");
        std::fs::write(d.join("node_modules/.package-lock.json"), "{}").unwrap();
        assert_eq!(node_modules_state(&d, &none), "present", "any entry, a hidden file too");
        let _ = std::fs::remove_dir_all(&d);
        let d = scratch("nm-file");
        std::fs::write(d.join("node_modules"), "").unwrap();
        assert_eq!(node_modules_state(&d, &none), "present", "not a directory");
        let _ = std::fs::remove_dir_all(&d);
        // From the root package.json's workspaces (`state`).
        let d = scratch("nm-state");
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/*"]}"#).unwrap();
        std::fs::create_dir_all(d.join("packages/w/node_modules/dep")).unwrap();
        std::fs::write(d.join("packages/w/package.json"), "{}").unwrap();
        assert_eq!(state(&d, YARN_LOCKFILES)["node_modules"], "present");
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
        assert_eq!(workspaces(&d), (vec!["packages/a".to_string(), "packages/b".into(), "tools/x/y".into()], true));
        let (f, complete) = files_state(&d, YARN_LOCKFILES);
        assert!(complete);
        assert_eq!(f["yarn.lock"], json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "sha256 of abc");
        assert_eq!(f["packages/a/package.json"], json!(hex::encode(sha2::Sha256::digest(b"packages/a"))));
        let keys: Vec<&String> = f.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["yarn.lock", "package.json", "packages/a/package.json", "packages/b/package.json", "tools/x/y/package.json"]);
        let (n, _) = files_state(&d, NPM_LOCKFILES);
        assert_eq!((n["package-lock.json"].clone(), n["npm-shrinkwrap.json"].clone()), (Value::Null, Value::Null));
        // Yarn's object form; braces.
        std::fs::write(d.join("package.json"), r#"{"workspaces": {"packages": ["packages/?"], "nohoist": ["**"]}}"#).unwrap();
        assert_eq!(workspaces(&d).0, ["packages/a", "packages/b"]);
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/{a,skip}", "{tools}/x/*"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/a".to_string(), "packages/skip".into(), "tools/x/y".into()], true));
        // What is not expanded makes the set incomplete.
        for w in [r#"["packages/[ab]"]"#, r#"["packages/{a,{b}}"]"#, r#"["packages/{a"]"#, r#"["packages/@(a|b)"]"#, r#"[3]"#, r#""packages/*""#] {
            std::fs::write(d.join("package.json"), format!(r#"{{"workspaces": {w}}}"#)).unwrap();
            assert!(!workspaces(&d).1, "{w}: incomplete");
        }
        std::fs::write(d.join("package.json"), "not json").unwrap();
        assert_eq!(workspaces(&d), (vec![], true));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn each_pattern_has_its_own_listing_budget() {
        // One `**` over more directories than its budget: incomplete, and a
        // second pattern after it is still listed in full.
        let d = scratch("budget");
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["big/**", "apps/*"]}"#).unwrap();
        for i in 0..(MAX_LISTED + 10) {
            std::fs::create_dir_all(d.join(format!("big/d{i:05}"))).unwrap();
        }
        for a in ["apps/one", "apps/two"] {
            std::fs::create_dir_all(d.join(a)).unwrap();
            std::fs::write(d.join(a).join("package.json"), "{}").unwrap();
        }
        let (dirs, complete) = workspaces(&d);
        assert!(!complete, "the big pattern ran out of its budget");
        assert_eq!(dirs, ["apps/one", "apps/two"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn braces_and_glob_segments() {
        assert_eq!(expand_braces("a/{b,c}/{d,e}").unwrap(), ["a/b/d", "a/b/e", "a/c/d", "a/c/e"]);
        assert_eq!(expand_braces("plain").unwrap(), ["plain"]);
        assert!(expand_braces("a/{b,{c}}").is_none() && expand_braces("a/b}").is_none() && expand_braces("a/{b").is_none());
        assert!(matches(b"*", b"abc") && matches(b"a*c", b"abbc") && matches(b"a?c", b"abc") && matches(b"*", b""));
        assert!(!matches(b"a?c", b"ac") && !matches(b"b*", b"abc"));
    }
}
