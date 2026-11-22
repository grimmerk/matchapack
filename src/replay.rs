//! What a later sample needs to replay a run (scripts/verify-sample.sh, issue
//! #77): whether `node_modules` was there before the run, and a SHA-256 of
//! the lockfile and of each manifest before and after it. A run that started
//! with no `node_modules` has a known before-state (the manifests and the
//! lockfile), so it can be replayed on copies, plain and through matchapack,
//! like a full install (TESTING.md, "Sampled verification of real runs").
//!
//! The files are small (a lockfile of a large project is a few MiB): each is
//! read and hashed once before the run and once after it. Workspaces are
//! found from the root `package.json`'s `workspaces` (an array, or Yarn's
//! `{"packages": [...]}`) with `*`, `?` and `**` path segments and `{a,b}`
//! alternatives, as Yarn's and npm's globs expand them. Anything beyond that
//! makes the state incomplete (`complete: false`), and a sample does not
//! replay a run whose file set is not known: character classes, extended
//! globs, other brace forms, a negated pattern (npm and Yarn 1 read it
//! differently), a pattern outside the project, a symlink to a directory
//! on the way, a listing that failed, or the listing budget reached (one
//! budget for all patterns: after it, nothing more is listed).

use serde_json::{json, Value};
use sha2::Digest;
use std::path::{Path, PathBuf};

/// npm's lockfiles at the install root (npm-shrinkwrap.json wins when both are there).
pub const NPM_LOCKFILES: &[&str] = &["package-lock.json", "npm-shrinkwrap.json"];
pub const YARN_LOCKFILES: &[&str] = &["yarn.lock"];

/// At most this many directories are listed for all the workspace patterns
/// (a `**` over a large tree must not slow an install down); reaching it
/// makes the state incomplete and ends the listing.
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
#[cfg(test)]
pub fn files_state(root: &Path, lockfiles: &[&str]) -> (Value, bool) {
    let (dirs, complete) = workspaces(root);
    (hashes(root, lockfiles, &dirs), complete)
}

fn hashes(root: &Path, lockfiles: &[&str], dirs: &[String]) -> Value {
    let mut names: Vec<String> = lockfiles.iter().map(|s| s.to_string()).collect();
    names.push("package.json".into());
    names.extend(dirs.iter().map(|d| format!("{d}/package.json")));
    // The project's own rc files the install reads: npm the root's .npmrc;
    // Yarn .yarnrc and .npmrc in the root and in each workspace. Never the
    // user's or the global ones (~/.npmrc, ~/.yarnrc, the global npmrc):
    // they are not read here, and not part of a sample.
    if lockfiles == YARN_LOCKFILES {
        for d in std::iter::once(String::new()).chain(dirs.iter().map(|d| format!("{d}/"))) {
            names.push(format!("{d}.yarnrc"));
            names.push(format!("{d}.npmrc"));
        }
    } else {
        names.push(".npmrc".into());
    }
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
#[cfg(test)]
pub fn workspaces(root: &Path) -> (Vec<String>, bool) {
    let (dirs, walk) = walk_workspaces(root);
    (dirs, walk.why.is_none())
}

/// `workspaces`, with the walk (how many directories it listed).
fn walk_workspaces(root: &Path) -> (Vec<String>, Walk) {
    let mut walk = Walk { listed: 0, why: None };
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else { return (vec![], walk) };
    let Ok(pj) = serde_json::from_str::<Value>(&text) else { return (vec![], walk) };
    let list = match &pj["workspaces"] {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o.get("packages").and_then(|p| p.as_array()).cloned().unwrap_or_default(),
        Value::Null => vec![],
        _ => {
            walk.miss("`workspaces` is neither a list nor Yarn's {\"packages\": [...]}");
            return (vec![], walk);
        }
    };
    let mut keep = std::collections::BTreeSet::new();
    // Once the listing budget is spent the set is incomplete whatever the
    // other patterns find, and `Walk::expand` lists nothing more (bounded
    // cost per run).
    for p in list.iter() {
        let Some(p) = p.as_str() else {
            walk.miss("a workspace pattern that is not a string");
            continue;
        };
        // A negated pattern: npm drops its matches, Yarn 1 still treats them
        // as workspaces; which one runs is not known here.
        if p.starts_with('!') {
            walk.miss(&format!("a negated workspace pattern ({p}): npm drops its matches, Yarn 1 keeps them"));
            continue;
        }
        let Some(alternatives) = expand_braces(p) else {
            walk.miss(&format!("a brace form matchapack does not expand ({p}; only {{a,b}})"));
            continue;
        };
        for pat in alternatives {
            let pat = pat.trim_start_matches("./").trim_end_matches('/');
            let segs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
            if segs.is_empty() {
                continue;
            }
            // Outside the project (`../shared`, `/abs`): a copy of the
            // project cannot hold it.
            if pat.starts_with('/') || segs.contains(&"..") || pat.starts_with('~') {
                walk.miss(&format!("a workspace pattern outside the project ({pat})"));
                continue;
            }
            // Character classes and extended globs (`@(a|b)`, `+(…)`, `!(…)`,
            // `*(…)`, `?(…)`) are not expanded; `@`, `+` and `!` elsewhere in
            // a name are literal (`packages/@scope/*`, `packages/c++`).
            let extglob = |s: &str| ["@(", "+(", "!(", "*(", "?("].iter().any(|o| s.contains(o));
            if segs.iter().any(|s| s.contains(['[', ']', '{', '}']) || extglob(s)) {
                walk.miss(&format!("a workspace pattern matchapack does not expand ({pat}: a character class or an extended glob)"));
                continue;
            }
            let mut found = vec![];
            walk.expand(root, &segs, &mut found);
            for d in found {
                if d.join("package.json").is_file() {
                    if let Ok(rel) = d.strip_prefix(root) {
                        keep.insert(rel.to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    if walk.listed >= MAX_LISTED {
        walk.miss(&format!("the workspace listing budget ({MAX_LISTED} directories) was reached"));
    }
    (keep.into_iter().filter(|d| !d.is_empty()).collect(), walk)
}

/// `a/{b,c}/d` → `a/b/d`, `a/c/d` (every group, left to right); a pattern
/// without braces → itself. Only plain alternatives, two or more, are
/// expanded; None (the set is then incomplete) for anything else: a single
/// alternative (`{tools}`, which npm and Yarn keep as it is), a range
/// (`{1..3}`), an empty alternative, nested or unbalanced braces, or more
/// than MAX_BRACES alternatives.
fn expand_braces(p: &str) -> Option<Vec<String>> {
    let Some(open) = p.find('{') else {
        return (!p.contains('}')).then(|| vec![p.to_string()]);
    };
    let close = open + p[open..].find('}')?;
    let inner = &p[open + 1..close];
    let alts: Vec<&str> = inner.split(',').collect();
    if inner.contains('{') || inner.contains("..") || alts.len() < 2 || alts.iter().any(|a| a.is_empty()) {
        return None;
    }
    let (head, tail) = (&p[..open], &p[close + 1..]);
    let mut out = vec![];
    for alt in alts {
        for rest in expand_braces(tail)? {
            out.push(format!("{head}{alt}{rest}"));
            if out.len() > MAX_BRACES {
                return None;
            }
        }
    }
    Some(out)
}

/// The walk over the workspace patterns: how many directories it listed
/// (one budget for all of them), and why what it found is incomplete, if it
/// is (the first reason): a listing that failed, or a symlink to a directory
/// on the way (npm and Yarn may follow it, a copy holds no such link), or
/// one of the patterns' reasons above.
struct Walk {
    listed: usize,
    why: Option<String>,
}

impl Walk {
    fn miss(&mut self, why: &str) {
        self.why.get_or_insert_with(|| why.to_string());
    }

    fn expand(&mut self, dir: &Path, segs: &[&str], out: &mut Vec<PathBuf>) {
        let Some((seg, rest)) = segs.split_first() else {
            out.push(dir.to_path_buf());
            return;
        };
        if !seg.contains(['*', '?']) {
            let d = dir.join(seg);
            match std::fs::symlink_metadata(&d) {
                Ok(m) if m.file_type().is_symlink() => {
                    if d.is_dir() {
                        self.miss(&format!("a workspace that is a symlink ({})", d.display()));
                    }
                }
                Ok(m) if m.is_dir() => self.expand(&d, rest, out),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => self.miss(&format!("{} could not be read ({e})", d.display())),
            }
            return;
        }
        if *seg == "**" {
            self.expand(dir, rest, out);
        }
        if self.listed >= MAX_LISTED {
            return;
        }
        self.listed += 1;
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.miss(&format!("{} could not be listed ({e})", dir.display()));
                return;
            }
        };
        let mut subs: Vec<(String, PathBuf)> = vec![];
        for e in rd {
            let Ok(e) = e else {
                self.miss(&format!("an entry of {} could not be read", dir.display()));
                continue;
            };
            let Ok(name) = e.file_name().into_string() else {
                self.miss(&format!("a name in {} is not UTF-8", dir.display()));
                continue;
            };
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => subs.push((name, e.path())),
                Ok(t) if t.is_symlink() => {
                    if e.path().is_dir() && (*seg == "**" || matches(seg, &name)) {
                        self.miss(&format!("a workspace that is a symlink ({})", e.path().display()));
                    }
                }
                Ok(_) => {}
                Err(e2) => self.miss(&format!("the type of {} could not be read ({e2})", e.path().display())),
            }
        }
        subs.sort();
        for (name, p) in subs {
            if *seg == "**" {
                self.expand(&p, segs, out);
                continue;
            }
            // Yarn's matcher counts UTF-16 code units: `?` takes half of a
            // character outside the BMP (`packages/?*?` matches a name of one
            // such character there, not here). Not decided here.
            if seg.contains('?') && name.chars().any(|c| c as u32 > 0xFFFF) {
                self.miss(&format!("a name with characters Yarn's matcher counts differently ({})", p.display()));
                continue;
            }
            if matches(seg, &name) {
                self.expand(&p, rest, out);
            }
        }
    }
}

/// `*` (any run of characters) and `?` (one character, as Yarn's and npm's
/// matchers take it: `中` is one) against a name.
fn matches(pat: &str, name: &str) -> bool {
    fn m(pat: &[char], name: &[char]) -> bool {
        match (pat.first(), name.first()) {
            (None, None) => true,
            (Some('*'), _) => m(&pat[1..], name) || (!name.is_empty() && m(pat, &name[1..])),
            (Some('?'), Some(_)) => m(&pat[1..], &name[1..]),
            (Some(a), Some(b)) if a == b => m(&pat[1..], &name[1..]),
            _ => false,
        }
    }
    m(&pat.chars().collect::<Vec<_>>(), &name.chars().collect::<Vec<_>>())
}

/// The environment variables npm or Yarn read as their settings, as a run's
/// record keeps them: `{"names": [sorted names], "sha256": hash of the
/// sorted name=value lines}`. Never the values: they can hold tokens.
/// npm: `npm_config_*` (any case), Yarn: `YARN_*` and `npm_config_*`; both
/// `NODE_ENV`. Left out: what says where a cache is or whether to look for
/// an update (`npm_config_cache`, `YARN_CACHE_FOLDER`,
/// `YARN_DISABLE_SELF_UPDATE_CHECK`, `npm_config_update_notifier`): the
/// harness sets those for its own runs, and they do not change a tree.
pub fn env_settings(engine: &str) -> Value {
    env_settings_of(std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().unwrap_or_default()))), engine)
}

fn env_settings_of(vars: impl Iterator<Item = (String, String)>, engine: &str) -> Value {
    const LEFT_OUT: &[&str] = &["npm_config_cache", "yarn_cache_folder", "yarn_disable_self_update_check", "npm_config_update_notifier"];
    let mut pairs: Vec<(String, String)> = vars
        .filter(|(k, _)| {
            let l = k.to_ascii_lowercase();
            let read = l == "node_env" || l.starts_with("npm_config_") || (engine == "yarn1" && l.starts_with("yarn_"));
            read && !LEFT_OUT.contains(&l.as_str())
        })
        .collect();
    pairs.sort();
    let text: String = pairs.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    json!({ "names": pairs.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), "sha256": hex::encode(sha2::Sha256::digest(text.as_bytes())) })
}

/// The state at `root` now: `{"node_modules": …, "files": …, "complete": …}`
/// (before a run; `matchapack replay-state` prints it for the sampler).
pub fn state(root: &Path, lockfiles: &[&str]) -> Value {
    let (dirs, walk) = walk_workspaces(root);
    json!({ "node_modules": node_modules_state(root, &dirs), "files": hashes(root, lockfiles, &dirs), "complete": walk.why.is_none(), "incomplete": walk.why })
}

/// `state`, with this process's `env_settings` (`matchapack replay-state`:
/// the sampler compares them with a run's).
pub fn state_with_env(root: &Path, lockfiles: &[&str], engine: &str) -> Value {
    let mut v = state(root, lockfiles);
    v["env_settings"] = env_settings(engine);
    v
}

/// The record's fields for a run whose state before it was `before`
/// (`state`): `node_modules_before`, `files_before`, `files_after` (now) and
/// `files_complete` (both sets complete) with, when not, `files_incomplete`
/// (why).
pub fn record_fields(root: &Path, lockfiles: &[&str], before: &Value) -> serde_json::Map<String, Value> {
    let (dirs, walk) = walk_workspaces(root);
    let mut m = serde_json::Map::new();
    m.insert("node_modules_before".into(), before["node_modules"].clone());
    m.insert("files_before".into(), before["files"].clone());
    m.insert("files_after".into(), hashes(root, lockfiles, &dirs));
    m.insert("files_complete".into(), json!(before["complete"] == true && walk.why.is_none()));
    if let Some(why) = before["incomplete"].as_str().map(String::from).or(walk.why) {
        m.insert("files_incomplete".into(), json!(why));
    }
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
        assert!(!workspaces(&d).1, "a negated pattern: incomplete");
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/*", "tools/**"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/a".to_string(), "packages/b".into(), "packages/skip".into(), "tools/x/y".into()], true));
        let (f, complete) = files_state(&d, YARN_LOCKFILES);
        assert!(complete);
        assert_eq!(f["yarn.lock"], json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "sha256 of abc");
        assert_eq!(f["packages/a/package.json"], json!(hex::encode(sha2::Sha256::digest(b"packages/a"))));
        let keys: Vec<&String> = f.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "yarn.lock", "package.json", "packages/a/package.json", "packages/b/package.json", "packages/skip/package.json", "tools/x/y/package.json",
                ".yarnrc", ".npmrc", "packages/a/.yarnrc", "packages/a/.npmrc", "packages/b/.yarnrc", "packages/b/.npmrc", "packages/skip/.yarnrc",
                "packages/skip/.npmrc", "tools/x/y/.yarnrc", "tools/x/y/.npmrc"
            ]
        );
        assert_eq!(f[".yarnrc"], Value::Null, "absent");
        std::fs::write(d.join("packages/a/.yarnrc"), "ignore-scripts true\n").unwrap();
        assert!(files_state(&d, YARN_LOCKFILES).0["packages/a/.yarnrc"].is_string(), "a workspace's .yarnrc, hashed");
        let (n, _) = files_state(&d, NPM_LOCKFILES);
        assert_eq!((n["package-lock.json"].clone(), n["npm-shrinkwrap.json"].clone()), (Value::Null, Value::Null));
        let nkeys: Vec<&String> = n.as_object().unwrap().keys().filter(|k| k.contains("rc")).collect();
        assert_eq!(nkeys, [".npmrc"], "npm reads the root's .npmrc only");
        // Yarn's object form; braces.
        std::fs::write(d.join("package.json"), r#"{"workspaces": {"packages": ["packages/?"], "nohoist": ["**"]}}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/a".to_string(), "packages/b".into()], true));
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/{a,skip}", "tools/x/*"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/a".to_string(), "packages/skip".into(), "tools/x/y".into()], true));
        // What is not expanded, or not representable in a copy, makes the
        // set incomplete.
        for w in [
            r#"["packages/[ab]"]"#,
            r#"["packages/{a,{b}}"]"#,
            r#"["packages/{a"]"#,
            r#"["packages/@(a|b)"]"#,
            r#"[3]"#,
            r#""packages/*""#,
            r#"["{tools}/x/*"]"#,
            r#"["packages/{1..3}"]"#,
            r#"["packages/{a,}"]"#,
            r#"["packages/*", "!packages/skip"]"#,
            r#"["../shared"]"#,
            r#"["/abs"]"#,
        ] {
            std::fs::write(d.join("package.json"), format!(r#"{{"workspaces": {w}}}"#)).unwrap();
            assert!(!workspaces(&d).1, "{w}: incomplete");
        }
        std::fs::write(d.join("package.json"), "not json").unwrap();
        assert_eq!(workspaces(&d), (vec![], true));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_budget_hit_ends_the_listing() {
        // One `**` over more directories than the budget: incomplete, and
        // the patterns after it are not listed at all.
        let d = scratch("budget");
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["big/**", "apps/*"]}"#).unwrap();
        for i in 0..(MAX_LISTED + 10) {
            std::fs::create_dir_all(d.join(format!("big/d{i:05}"))).unwrap();
        }
        for a in ["apps/one", "apps/two"] {
            std::fs::create_dir_all(d.join(a)).unwrap();
            std::fs::write(d.join(a).join("package.json"), "{}").unwrap();
        }
        let (dirs, walk) = walk_workspaces(&d);
        assert!(walk.why.as_deref().is_some_and(|w| w.contains("budget")), "the big pattern ran out of the budget: {:?}", walk.why);
        assert!(walk.listed <= MAX_LISTED, "listed {} directories, the budget is {MAX_LISTED}", walk.listed);
        assert!(dirs.is_empty(), "nothing listed after the budget: {dirs:?}");
        // The same patterns the other way round: apps/* is found, and the
        // set is still incomplete.
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["apps/*", "big/**"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["apps/one".to_string(), "apps/two".into()], false));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_name_outside_the_bmp_under_a_question_mark_is_not_decided() {
        let d = scratch("astral");
        for w in ["packages/😀", "packages/ab"] {
            std::fs::create_dir_all(d.join(w)).unwrap();
            std::fs::write(d.join(w).join("package.json"), "{}").unwrap();
        }
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/?*?"]}"#).unwrap();
        let st = state(&d, YARN_LOCKFILES);
        assert_eq!(st["complete"], false);
        assert!(st["incomplete"].as_str().is_some_and(|w| w.contains("counts differently")), "{st}");
        // `*` alone matches it either way: decided.
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/*"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/ab".to_string(), "packages/😀".into()], true));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_multibyte_workspace_name_is_one_character() {
        let d = scratch("multibyte");
        for w in ["packages/@scope/中", "packages/中文"] {
            std::fs::create_dir_all(d.join(w)).unwrap();
            std::fs::write(d.join(w).join("package.json"), "{}").unwrap();
        }
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/@scope/?", "packages/中*"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/@scope/中".to_string(), "packages/中文".into()], true));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_settings_from_the_environment_are_hashed_not_logged() {
        let v = |pairs: &[(&str, &str)], engine: &str| env_settings_of(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())), engine);
        let a = v(&[("npm_config_ignore_scripts", "true"), ("NODE_ENV", "production"), ("PATH", "/bin"), ("NPM_CONFIG_//r.example/:_authToken", "SECRET123"), ("npm_config_cache", "/c")], "npm");
        assert_eq!(a["names"], json!(["NODE_ENV", "NPM_CONFIG_//r.example/:_authToken", "npm_config_ignore_scripts"]));
        assert!(!a.to_string().contains("SECRET123") && !a.to_string().contains("production"), "values are never kept: {a}");
        // Another value, the same names: another hash.
        let b = v(&[("npm_config_ignore_scripts", "false"), ("NODE_ENV", "production"), ("NPM_CONFIG_//r.example/:_authToken", "SECRET123")], "npm");
        assert_eq!(a["names"], b["names"]);
        assert_ne!(a["sha256"], b["sha256"]);
        // What the harness sets for its own runs, and PATH, change nothing.
        let c = v(&[("npm_config_ignore_scripts", "true"), ("NODE_ENV", "production"), ("NPM_CONFIG_//r.example/:_authToken", "SECRET123"), ("npm_config_update_notifier", "false"), ("HOME", "/h")], "npm");
        assert_eq!(a["sha256"], c["sha256"]);
        // Yarn also reads YARN_*; npm does not.
        assert_eq!(v(&[("YARN_PRODUCTION", "true"), ("YARN_CACHE_FOLDER", "/c"), ("YARN_DISABLE_SELF_UPDATE_CHECK", "true")], "yarn1")["names"], json!(["YARN_PRODUCTION"]));
        assert_eq!(v(&[("YARN_PRODUCTION", "true")], "npm")["names"], json!([]));
    }

    #[test]
    fn scoped_and_plus_names_are_literal() {
        let d = scratch("scoped");
        for w in ["packages/@scope/a", "packages/@scope/b", "packages/c++", "packages/x!y"] {
            std::fs::create_dir_all(d.join(w)).unwrap();
            std::fs::write(d.join(w).join("package.json"), "{}").unwrap();
        }
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/@scope/*", "packages/c++", "packages/x!y"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/@scope/a".to_string(), "packages/@scope/b".into(), "packages/c++".into(), "packages/x!y".into()], true));
        // Extended globs stay unsupported, with the reason in the state.
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/@(a|b)"]}"#).unwrap();
        let st = state(&d, YARN_LOCKFILES);
        assert_eq!(st["complete"], false);
        assert!(st["incomplete"].as_str().is_some_and(|w| w.contains("extended glob")), "{st}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn symlinks_and_failed_listings_make_the_set_incomplete() {
        let d = scratch("links");
        std::fs::create_dir_all(d.join("packages/a")).unwrap();
        std::fs::write(d.join("packages/a/package.json"), "{}").unwrap();
        std::fs::create_dir_all(d.join("elsewhere/b")).unwrap();
        std::fs::write(d.join("elsewhere/b/package.json"), "{}").unwrap();
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/*"]}"#).unwrap();
        assert_eq!(workspaces(&d), (vec!["packages/a".to_string()], true));
        // A workspace that is a symlink to a directory, under a glob.
        std::os::unix::fs::symlink(d.join("elsewhere/b"), d.join("packages/b")).unwrap();
        assert!(!workspaces(&d).1, "a symlinked workspace under a glob");
        // Named directly; the state says why.
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/a", "packages/b"]}"#).unwrap();
        assert!(!workspaces(&d).1, "a symlinked workspace named directly");
        assert!(state(&d, YARN_LOCKFILES)["incomplete"].as_str().is_some_and(|w| w.contains("a workspace that is a symlink")));
        // A symlink to a file under a glob is no workspace: still complete.
        std::fs::remove_file(d.join("packages/b")).unwrap();
        std::os::unix::fs::symlink(d.join("package.json"), d.join("packages/c")).unwrap();
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["packages/*"]}"#).unwrap();
        assert!(workspaces(&d).1, "a symlink to a file");
        // A directory that cannot be listed.
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(d.join("locked/x")).unwrap();
        std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(d.join("package.json"), r#"{"workspaces": ["locked/*"]}"#).unwrap();
        let unlistable = std::fs::read_dir(d.join("locked")).is_err();
        let (_, complete) = workspaces(&d);
        std::fs::set_permissions(d.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        if unlistable {
            assert!(!complete, "a listing that failed");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn braces_and_glob_segments() {
        assert_eq!(expand_braces("a/{b,c}/{d,e}").unwrap(), ["a/b/d", "a/b/e", "a/c/d", "a/c/e"]);
        assert_eq!(expand_braces("plain").unwrap(), ["plain"]);
        assert!(expand_braces("a/{b,{c}}").is_none() && expand_braces("a/b}").is_none() && expand_braces("a/{b").is_none());
        assert!(expand_braces("{tools}/x").is_none() && expand_braces("p/{1..3}").is_none() && expand_braces("p/{a,}").is_none());
        assert!(matches("*", "abc") && matches("a*c", "abbc") && matches("a?c", "abc") && matches("*", ""));
        assert!(!matches("a?c", "ac") && !matches("b*", "abc"));
        // One character, not one byte.
        assert!(matches("?", "中") && matches("a?", "a中") && matches("*文", "中文") && !matches("??", "中"));
    }
}
