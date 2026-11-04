//! npm: package-lock v2/v3 → Plan, and the post-install step (npm rebuild +
//! hidden lockfile).

use crate::lock::{self, Entry, Lockfile};
use crate::plan::{Kind, Plan, PlanEntry, PostInstall, Source};
use crate::platform;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

/// Resolve dependency `name` required from the package at `from` the way Node
/// does: `<from>/node_modules/<name>`, then each ancestor directory.
fn resolve<'a>(keys: &HashSet<&'a str>, from: &str, name: &str) -> Option<&'a str> {
    let mut dir = from.to_string();
    loop {
        let cand = if dir.is_empty() { format!("node_modules/{name}") } else { format!("{dir}/node_modules/{name}") };
        if let Some(k) = keys.get(cand.as_str()) {
            return Some(k);
        }
        if dir.is_empty() {
            return None;
        }
        // Step up one directory; `a/node_modules/b` → `a` (skip the node_modules segment).
        dir = match dir.rfind('/') {
            Some(i) => {
                let up = &dir[..i];
                up.strip_suffix("/node_modules").or(if up == "node_modules" { Some("") } else { None }).unwrap_or(up).to_string()
            }
            None => String::new(),
        };
    }
}

/// Paths npm leaves out on this machine: optional packages whose os/cpu/libc do
/// not match, plus the dependencies only those packages need (npm's
/// optional-set removal, downward half).
fn platform_skips(lf: &Lockfile) -> Result<Vec<(String, String)>> {
    let by_path: HashMap<&str, &Entry> = lf.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let keys: HashSet<&str> = by_path.keys().copied().collect();
    // Edges: from → to, including link → target.
    let mut edges_in: HashMap<&str, Vec<&str>> = HashMap::new();
    // Non-optional edges only, to → from: the upward walk of optional-set.js.
    let mut required_in: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut edges_out: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &lf.entries {
        let mut outs = vec![];
        if e.link {
            if let Some(t) = e.resolved.as_deref().and_then(|r| keys.get(r)) {
                outs.push(*t);
            }
        }
        for d in &e.deps {
            if let Some(t) = resolve(&keys, &e.path, d) {
                outs.push(t);
                if !e.opt_deps.contains(d) {
                    required_in.entry(t).or_default().push(e.path.as_str());
                }
            }
        }
        for t in &outs {
            edges_in.entry(t).or_default().push(e.path.as_str());
        }
        edges_out.insert(e.path.as_str(), outs);
    }
    let mut skipped: Vec<(String, String)> = vec![];
    let mut set: HashSet<&str> = HashSet::new();
    let mut bad_required = vec![];
    for e in &lf.entries {
        if e.path.is_empty() || e.link || platform::matches(&e.os, &e.cpu, &e.libc) {
            continue;
        }
        if e.optional || e.dev_optional {
            set.insert(e.path.as_str());
            skipped.push((e.path.clone(), format!("platform (os {:?} cpu {:?} libc {:?})", e.os, e.cpu, e.libc)));
        } else {
            bad_required.push(e.path.clone());
        }
    }
    if !bad_required.is_empty() {
        bail!("EBADPLATFORM: non-optional packages unsupported on this platform: {bad_required:?}");
    }
    // Upward: whatever requires a skipped package through a NON-optional edge
    // cannot work without it and is removed too (arborist optional-set.js),
    // stopping at optional edges. The root is never removed.
    let mut frontier: Vec<&str> = set.iter().copied().collect();
    while let Some(n) = frontier.pop() {
        for f in required_in.get(n).map(|v| v.as_slice()).unwrap_or(&[]) {
            if f.is_empty() {
                bail!("a package required by the root project is unsupported on this platform: {n}");
            }
            if set.insert(f) {
                skipped.push((f.to_string(), format!("requires skipped optional package {n}")));
                frontier.push(f);
            }
        }
    }
    // Downward closure: a node all of whose dependents are in the set goes too.
    loop {
        let mut added = vec![];
        for n in set.iter() {
            for c in edges_out.get(n).map(|v| v.as_slice()).unwrap_or(&[]) {
                if set.contains(c) {
                    continue;
                }
                let ins = edges_in.get(c).map(|v| v.as_slice()).unwrap_or(&[]);
                if !ins.is_empty() && ins.iter().all(|f| set.contains(f)) {
                    added.push(*c);
                }
            }
        }
        if added.is_empty() {
            break;
        }
        for a in added {
            if set.insert(a) {
                skipped.push((a.to_string(), "only required by skipped optional packages".into()));
            }
        }
    }
    Ok(skipped)
}

/// Structural checks on lockfile paths, before anything reads a path from the
/// lockfile (sync check, dry-run copies, placement).
pub fn validate_paths(lf: &Lockfile) -> Result<()> {
    // Every path that becomes a delete, clone or symlink under the root must be
    // plain and relative (see util::safe_relative). A `file:../shared` target
    // also gets a `../shared` key in the lockfile, which this refuses, so such
    // projects go to npm.
    let unsafe_paths: Vec<&str> = lf
        .entries
        .iter()
        .filter(|e| !e.path.is_empty() && !crate::util::safe_relative(&e.path))
        .map(|e| e.path.as_str())
        .collect();
    if !unsafe_paths.is_empty() {
        bail!("lockfile paths that are not plain relative paths: {unsafe_paths:?}");
    }
    // An entry nested under a link entry would be placed through the symlink,
    // i.e. inside the link's target (possibly outside the project). npm never
    // writes such keys.
    let link_paths: Vec<&str> = lf.entries.iter().filter(|e| e.link).map(|e| e.path.as_str()).collect();
    if let Some(e) = lf.entries.iter().find(|e| link_paths.iter().any(|l| e.path.starts_with(&format!("{l}/")))) {
        bail!("lockfile entry {} lies under a link", e.path);
    }
    Ok(())
}

/// Stand-in for npm ci's "package.json and package-lock.json are in sync" check.
/// For the root and each workspace, every declared dependency must either equal
/// the lockfile's record, or be a semver range (or `npm:name@range` alias) that
/// the locked version still satisfies -- judged by npm's own bundled `semver`,
/// so range semantics are npm's. Added or removed dependencies and non-registry
/// specs that changed are refused (npm ci would resolve or prune them).
/// Not covered: a change to `overrides`.
pub fn check_in_sync(root: &Path, lf: &Lockfile) -> Result<Option<Ideal>> {
    validate_paths(lf)?;
    const FIELDS: [&str; 4] = ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"];
    let pkgs = lf.raw.get("packages").and_then(|v| v.as_object()).context("no packages")?;
    let keys: HashSet<&str> = pkgs.keys().map(|k| k.as_str()).collect();
    let mut dirs = vec![String::new()];
    for e in &lf.entries {
        if e.link {
            if let Some(t) = &e.resolved {
                if crate::util::safe_relative(t) && !t.contains("node_modules/") && !dirs.contains(t) {
                    dirs.push(t.clone());
                }
            }
        }
    }
    let mut problems = vec![];
    // (label, locked version, range) to check with npm's semver.
    let mut checks: Vec<(String, String, String)> = vec![];
    for d in dirs {
        let label = if d.is_empty() { "<root>".to_string() } else { d.clone() };
        let pj = root.join(&d).join("package.json");
        let text = std::fs::read_to_string(&pj).with_context(|| format!("reading {}", pj.display()))?;
        let pj: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", pj.display()))?;
        let Some(lock) = pkgs.get(&d) else {
            problems.push(format!("{label}: no lockfile entry"));
            continue;
        };
        for f in FIELDS {
            let empty = Map::new();
            let a = pj.get(f).and_then(|v| v.as_object()).unwrap_or(&empty);
            let b = lock.get(f).and_then(|v| v.as_object()).unwrap_or(&empty);
            for name in a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))) {
                let (want, had) = (a.get(name).and_then(|v| v.as_str()), b.get(name).and_then(|v| v.as_str()));
                if want == had {
                    continue;
                }
                let (Some(want), Some(_)) = (want, had) else {
                    problems.push(format!("{label}: `{f}.{name}` was added or removed"));
                    continue;
                };
                // Peer dependencies are not necessarily installed; npm ci does
                // not fail on a changed peer range alone.
                let Some(target) = resolve(&keys, &d, name) else {
                    if f != "peerDependencies" {
                        problems.push(format!("{label}: `{f}.{name}` is not installed in the lockfile"));
                    }
                    continue;
                };
                let entry = &pkgs[target];
                let version = entry.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let pkg_name = entry.get("name").and_then(|v| v.as_str()).unwrap_or(name);
                let range = match want.strip_prefix("npm:") {
                    Some(alias) => match alias.rsplit_once('@').filter(|(n, _)| !n.is_empty()) {
                        Some((n, r)) if n == pkg_name => r.to_string(),
                        Some(_) => {
                            problems.push(format!("{label}: `{f}.{name}` alias now points at another package"));
                            continue;
                        }
                        None => "*".to_string(),
                    },
                    None if want.contains(':') || want.contains('/') => {
                        problems.push(format!("{label}: `{f}.{name}` changed to a non-registry spec {want:?}"));
                        continue;
                    }
                    None => want.to_string(),
                };
                checks.push((format!("{label}: `{f}.{name}` {want:?} vs locked {version}"), version, range));
            }
        }
    }
    if !checks.is_empty() && problems.is_empty() {
        let semver = crate::util::npm_dir().context("cannot locate npm's installation")?.join("node_modules/semver");
        let pairs: Vec<(&str, &str)> = checks.iter().map(|(_, v, r)| (v.as_str(), r.as_str())).collect();
        let js = "const s=require(process.argv[1]);const p=JSON.parse(process.argv[2]);\
                  console.log(JSON.stringify(p.map(([v,r])=>s.satisfies(v,r,{includePrerelease:false}))))";
        let out = Command::new("node")
            .args(["-e", js])
            .arg(&semver)
            .arg(serde_json::to_string(&pairs)?)
            .output()
            .context("running node semver check")?;
        if !out.status.success() {
            bail!("semver check failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let ok: Vec<bool> = serde_json::from_slice(&out.stdout).context("parsing semver check output")?;
        for ((label, _, _), good) in checks.iter().zip(ok) {
            if !good {
                problems.push(format!("{label}: locked version no longer satisfies the range"));
            }
        }
    }
    if !problems.is_empty() {
        bail!(
            "package.json and package-lock.json are not in sync (run `npm install` to update the lockfile, or plain `npm ci`):\n  {}",
            problems.join("\n  ")
        );
    }
    // `overrides` are not recorded in the lockfile, so an edit to them is
    // invisible to the comparison above, yet npm ci refuses such a tree
    // (verified: overriding `ms` without updating the lockfile → EUSAGE "not in
    // sync"). Override semantics (nested selectors, `$ref`) belong to npm, so ask
    // npm itself: `npm ci --dry-run` loads the lockfile, builds the ideal tree,
    // validates, and writes nothing (~1 s small, ~2.5 s on a large workspaces monorepo).
    // Run it for EVERY project, not only those with overrides: without it a
    // lockfile missing a transitive entry (a hand-merged lockfile) installs a
    // broken tree where npm ci refuses (EUSAGE "Missing: … from lock file"),
    // and an unreferenced extra entry would be placed (found in review).
    {
        // Run it in a scratch copy without node_modules (package.json, lockfile,
        // .npmrc and each workspace's package.json): with a node_modules on disk
        // npm would diff against it. Under the store's tmp/ (named `<pid>-dryrun`,
        // so Store::new removes it if this process dies).
        let scratch = crate::default_store(None).join("tmp").join(format!("{}-dryrun", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let scratch = std::fs::canonicalize(&scratch)?;
        let t = std::time::Instant::now();
        let result = dry_run_ideal(root, &scratch, lf);
        crate::util::record("npm ci --dry-run", t.elapsed().as_secs_f64());
        let _ = std::fs::remove_dir_all(&scratch);
        result.map(Some)
    }
}

/// npm ci's ideal tree: the location of every node npm would install, and the
/// dep flags (dev / optional / devOptional / peer) npm computed for it.
#[derive(Debug, Default)]
pub struct Ideal {
    pub paths: HashSet<String>,
    pub flags: HashMap<String, Map<String, Value>>,
}

/// The four flags npm recomputes for every node (arborist calc-dep-flags.js).
const DEP_FLAGS: [&str; 4] = ["dev", "optional", "devOptional", "peer"];

fn dry_run_ideal(root: &Path, scratch: &Path, lf: &Lockfile) -> Result<Ideal> {
    for f in ["package.json", "package-lock.json", ".npmrc"] {
        if root.join(f).is_file() {
            std::fs::copy(root.join(f), scratch.join(f))?;
        }
    }
    for e in &lf.entries {
        if let (true, Some(t)) = (e.link, &e.resolved) {
            if crate::util::safe_relative(t) && !t.contains("node_modules/") && root.join(t).join("package.json").is_file() {
                std::fs::create_dir_all(scratch.join(t))?;
                std::fs::copy(root.join(t).join("package.json"), scratch.join(t).join("package.json"))?;
            }
        }
    }
    // npm's own CLI runs `ci --dry-run --ignore-scripts` inside the helper, which
    // keeps the ideal tree arborist built and writes each node's location and
    // flags (see ideal-tree.js). --ignore-scripts: ci.js runs the root lifecycle
    // scripts even under --dry-run (measured: `husky` exit 127, a root script
    // building a workspace failing, with no node_modules yet).
    let npm_dir = crate::util::npm_dir().context("cannot locate npm's installation")?;
    let helper = scratch.with_file_name(format!("{}-ideal-tree.js", std::process::id()));
    let out_file = scratch.with_file_name(format!("{}-ideal.json", std::process::id()));
    let _ = std::fs::remove_file(&helper);
    let _ = std::fs::remove_file(&out_file);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&helper)?;
        f.write_all(include_str!("ideal-tree.js").as_bytes())?;
    }
    let out = crate::util::node_cmd()
        .arg(&helper)
        .arg(&npm_dir)
        .arg(&out_file)
        .current_dir(scratch)
        .output();
    let _ = std::fs::remove_file(&helper);
    let out = out.context("running npm ci --dry-run")?;
    let text = std::fs::read_to_string(&out_file);
    let _ = std::fs::remove_file(&out_file);
    if !out.status.success() {
        bail!("`npm ci --dry-run` rejected this lockfile:\n{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = text.context("npm ci --dry-run reported no ideal tree")?;
    let json: Value = serde_json::from_str(&text).context("parsing the ideal tree")?;
    parse_ideal(&json, lf)
}

/// The helper's output: `{"nodes": {"<location>": {"dev": true, ...}}}`.
/// npm ci installs its IDEAL tree, not the lockfile text, and its sync check
/// only asks that every ideal node be in the lockfile, not the reverse. With
/// overrides the lockfile npm itself writes can hold redundant nested copies
/// that the ideal tree dedupes (measured: three nested ms@2.1.2 identical to the
/// root one; npm ci installed 344 of 347 entries).
fn parse_ideal(json: &Value, lf: &Lockfile) -> Result<Ideal> {
    let nodes = json.get("nodes").and_then(|v| v.as_object()).context("ideal tree without `nodes`")?;
    let in_lock: HashSet<&str> = lf.entries.iter().map(|e| e.path.as_str()).collect();
    let mut ideal = Ideal::default();
    let mut moved = vec![];
    for (loc, f) in nodes {
        if !in_lock.contains(loc.as_str()) {
            moved.push(loc.clone());
            continue;
        }
        let f = f.as_object().context("ideal tree node is not an object")?;
        let mut flags = Map::new();
        for k in DEP_FLAGS {
            if f.get(k).and_then(|v| v.as_bool()) == Some(true) {
                flags.insert(k.to_string(), Value::Bool(true));
            }
        }
        ideal.flags.insert(loc.clone(), flags);
        ideal.paths.insert(loc.clone());
    }
    if !moved.is_empty() {
        moved.sort();
        bail!("npm's ideal tree places packages at paths the lockfile does not record (use `npm ci`): {moved:?}");
    }
    Ok(ideal)
}

/// Replace the lockfile's dep flags with the ones npm computed, for every node
/// of the ideal tree: npm writes its own (calcDepFlags) to the hidden lockfile
/// and uses them to decide which optional packages it may leave out, so a
/// lockfile with stale flags (measured: 11 `"peer": true` that npm ci does not
/// write) must not be copied. Entries outside the ideal tree keep theirs.
pub fn apply_flags(lf: &mut Lockfile, ideal: &Ideal) {
    if let Some(Value::Object(pkgs)) = lf.raw.get_mut("packages") {
        for (loc, flags) in &ideal.flags {
            if let Some(Value::Object(entry)) = pkgs.get_mut(loc) {
                for k in DEP_FLAGS {
                    entry.shift_remove(k);
                }
                // Key order is restored when the hidden lockfile is written.
                for (k, v) in flags {
                    entry.insert(k.clone(), v.clone());
                }
            }
        }
    }
    for e in &mut lf.entries {
        if let Some(f) = ideal.flags.get(&e.path) {
            e.optional = f.contains_key("optional");
            e.dev_optional = f.contains_key("devOptional");
        }
    }
}

/// Reorder one lockfile entry's keys the way npm writes them
/// (json-stringify-nice with Shrinkwrap's key order, npm 10.9 and 11.19):
/// values that are not objects first, then objects; within each group the
/// keys in `PREFERRED` in that order, then the rest by `localeCompare('en')`.
/// Lockfile field names are ASCII letters, where that collation is
/// case-insensitive with lowercase first, which is what this compares. Only
/// the entry's own keys are ordered: nested objects (dependency names, where
/// `@` and `-` collate differently) keep the order npm wrote in the lockfile.
fn npm_key_order(entry: &mut Map<String, Value>) {
    const PREFERRED: [&str; 8] = ["name", "version", "lockfileVersion", "resolved", "integrity", "requires", "packages", "dependencies"];
    let rank = |(k, v): (&String, &Value)| {
        let pref = PREFERRED.iter().position(|p| p == k).unwrap_or(PREFERRED.len());
        (v.is_object(), pref, k.to_lowercase(), k.chars().map(|c| c.is_uppercase()).collect::<Vec<_>>())
    };
    let mut items: Vec<(String, Value)> = std::mem::take(entry).into_iter().collect();
    items.sort_by_cached_key(|(k, v)| rank((k, v)));
    entry.extend(items);
}

const EXTRANEOUS: &str = "extraneous";

/// Workspace patterns from the lockfile's root entry (`workspaces`, as an array
/// or `{ "packages": [...] }`), which npm copies from package.json.
fn workspace_globs(lf: &Lockfile) -> Vec<String> {
    let root = lf.raw.get("packages").and_then(|p| p.get("")).and_then(|r| r.get("workspaces"));
    let list = match root {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Object(o)) => o.get("packages").and_then(|p| p.as_array()).cloned().unwrap_or_default(),
        _ => vec![],
    };
    list.iter().filter_map(|v| v.as_str()).map(|s| s.trim_start_matches("./").trim_end_matches('/').to_string()).collect()
}

/// Minimal glob for workspace patterns: `*` matches one path segment, `**` any
/// number of segments; other segments match literally.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(p: &[&str], s: &[&str]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(&"**") => (0..=s.len()).any(|i| rec(&p[1..], &s[i..])),
            Some(seg) => !s.is_empty() && seg_match(seg, s[0]) && rec(&p[1..], &s[1..]),
        }
    }
    fn seg_match(p: &str, s: &str) -> bool {
        match p.split_once('*') {
            None => p == s,
            Some((pre, post)) => s.len() >= pre.len() + post.len() && s.starts_with(pre) && s.ends_with(post),
        }
    }
    let p: Vec<&str> = pattern.split('/').filter(|x| !x.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    rec(&p, &s)
}

/// `ideal`: when npm's ideal tree is known (see `check_in_sync`), lockfile
/// entries outside it are not installed, as npm ci does.
pub fn build_plan(lf: &Lockfile, ideal: Option<&HashSet<String>>) -> Result<Plan> {
    let mut plan = Plan { owned_roots: vec![String::new()], ..Default::default() };
    plan.skipped = platform_skips(lf)?;
    // npm ci does not install entries the lockfile marks extraneous. Decided
    // before the ideal-tree filter: the reason matters, since npm's hidden
    // lockfile keeps extraneous entries but not deduplicated ones.
    for e in &lf.entries {
        if e.extraneous && !plan.skipped.iter().any(|(p, _)| p == &e.path) {
            plan.skipped.push((e.path.clone(), format!("{EXTRANEOUS} in the lockfile")));
        }
    }
    if let Some(ideal) = ideal {
        for e in &lf.entries {
            if lock::depth(&e.path) > 0
                && !e.in_bundle
                && !ideal.contains(&e.path)
                && !plan.skipped.iter().any(|(p, _)| p == &e.path)
            {
                plan.skipped.push((e.path.clone(), "not in npm's ideal tree (deduplicated by npm ci)".into()));
            }
        }
    }
    let skipped: HashSet<&str> = plan.skipped.iter().map(|(p, _)| p.as_str()).collect();
    let under_skipped = |p: &str| {
        skipped.contains(p) || skipped.iter().any(|s| p.starts_with(s) && p[s.len()..].starts_with("/node_modules/"))
    };
    let mut unsupported = vec![];
    validate_paths(lf)?;
    // npm ci empties node_modules only in the root and in the DECLARED
    // workspaces (ci.js: getWorkspaces). Other in-project link targets
    // (`file:./lib`) keep their node_modules; if one has anything in it, the
    // tree npm ci leaves is not one thinclone reproduces, so hand over.
    let ws_globs = workspace_globs(lf);
    for e in &lf.entries {
        if e.link {
            let target = e.resolved.clone().context("link entry without resolved")?;
            if !crate::util::safe_relative(&target) || target.contains("node_modules/") || plan.owned_roots.contains(&target) {
                // not an in-project directory we could own
            } else if ws_globs.iter().any(|g| glob_match(g, &target)) {
                plan.owned_roots.push(target.clone());
            } else if std::fs::read_dir(lf.root.join(&target).join("node_modules")).map(|mut d| d.next().is_some()).unwrap_or(false) {
                bail!("`file:` dependency directory {target} has its own node_modules, which npm ci leaves in place");
            }
        }
        if lock::depth(&e.path) == 0 || e.in_bundle || under_skipped(&e.path) {
            continue;
        }
        let name = e.name.clone().unwrap_or_else(|| lock::name_from_path(&e.path).to_string());
        let kind = if e.link {
            Kind::Link { target: e.resolved.clone().unwrap() }
        } else {
            match (&e.resolved, &e.integrity) {
                (Some(r), Some(i)) if r.starts_with("https://") || r.starts_with("http://") => {
                    Kind::Package { integrity: Some(i.clone()), source: Source::Tarball { url: r.clone() } }
                }
                // npm sometimes writes entries with only a version; npm ci then
                // resolves name@version against the registry.
                (None, _) if e.version.is_some() => Kind::Package {
                    integrity: e.integrity.clone(),
                    source: Source::Registry { name: name.clone(), version: e.version.clone().unwrap() },
                },
                _ => {
                    unsupported.push(format!("{} ({})", e.path, e.resolved.as_deref().unwrap_or("no resolved, no version")));
                    continue;
                }
            }
        };
        plan.entries.push(PlanEntry {
            install_path: e.path.clone(),
            name,
            version: e.version.clone(),
            kind,
            has_install_script: false,
        });
    }
    if !unsupported.is_empty() {
        bail!("unsupported lockfile entries (git / file / missing integrity): {unsupported:?}");
    }
    Ok(plan)
}

pub struct NpmRebuild {
    pub lock_raw: Map<String, Value>,
    pub ignore_scripts: bool,
    pub extra_args: Vec<String>,
}

impl NpmRebuild {
    /// npm's hidden lockfile: the lockfile's packages minus the root and minus
    /// what was not installed, in lockfile order.
    fn hidden_lockfile(&self, plan: &Plan) -> Value {
        // Measured on compass (2026-10-31): npm's hidden lockfile KEEPS
        // extraneous node_modules entries even though they are not installed,
        // and drops entries outside node_modules that no link points at.
        let skipped: HashSet<&str> = plan
            .skipped
            .iter()
            .filter(|(_, why)| !why.starts_with(EXTRANEOUS))
            .map(|(p, _)| p.as_str())
            .collect();
        let pkgs = self.lock_raw.get("packages").and_then(|v| v.as_object());
        let link_targets: HashSet<&str> = pkgs
            .map(|p| {
                p.values()
                    .filter(|v| v.get("link").and_then(|x| x.as_bool()) == Some(true))
                    .filter_map(|v| v.get("resolved").and_then(|x| x.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Map::new();
        for k in ["name", "version"] {
            if let Some(v) = self.lock_raw.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
        out.insert("lockfileVersion".into(), Value::from(3));
        if let Some(v) = self.lock_raw.get("requires") {
            out.insert("requires".into(), v.clone());
        }
        let mut pk = Map::new();
        if let Some(Value::Object(p)) = self.lock_raw.get("packages") {
            for (k, v) in p {
                if k.is_empty() || skipped.contains(k.as_str()) {
                    continue;
                }
                if !k.contains("node_modules/") && !link_targets.contains(k.as_str()) {
                    continue;
                }
                if skipped.iter().any(|s| k.starts_with(s) && k[s.len()..].starts_with("/node_modules/")) {
                    continue;
                }
                let mut v = v.clone();
                if let Value::Object(entry) = &mut v {
                    // npm re-serializes every entry (it does not keep the
                    // lockfile's key order), so neither may thinclone.
                    npm_key_order(entry);
                }
                pk.insert(k.clone(), v);
            }
        }
        out.insert("packages".into(), Value::Object(pk));
        Value::Object(out)
    }
}

impl PostInstall for NpmRebuild {
    /// Mirrors `npm ci` (npm/lib/commands/ci.js): reify = dependency scripts +
    /// bin links + hidden lockfile, THEN the root project's seven lifecycle
    /// events. The order matters: root scripts that touch the tree (e.g. a
    /// `postinstall` that builds a workspace) leave npm's hidden lockfile stale,
    /// and later npm commands then read the tree from disk instead.
    fn run(&self, root: &Path, plan: &Plan) -> Result<()> {
        // 1. Dependencies only: select every installed node by name; the root
        //    is excluded unless a dependency shares its name (then by directory).
        let root_name = self.lock_raw.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let mut specs: Vec<String> = vec![];
        let mut seen = HashSet::new();
        for e in &plan.entries {
            // npm rebuild matches the NODE name, i.e. the folder name: for an
            // alias (`"typescript7": "npm:typescript@7"`) that is not the package name.
            let node_name = lock::name_from_path(&e.install_path);
            if node_name == root_name {
                specs.push(root.join(&e.install_path).to_string_lossy().into_owned());
            } else if seen.insert(node_name.to_string()) {
                specs.push(format!("{node_name}@*"));
            }
        }
        let t = std::time::Instant::now();
        if !specs.is_empty() {
            let mut cmd = crate::util::npm_cmd();
            cmd.arg("rebuild").current_dir(root);
            if self.ignore_scripts {
                cmd.arg("--ignore-scripts");
            }
            cmd.args(&self.extra_args).args(&specs);
            if crate::util::quiet() {
                cmd.stdout(std::process::Stdio::null());
            }
            let st = cmd.status().context("running npm rebuild")?;
            if !st.success() {
                // Not reported as a script failure: npm ci drops an OPTIONAL
                // dependency whose install script fails and carries on, which
                // only npm ci itself reproduces. The wrapper hands over to it.
                bail!("npm rebuild failed: {st}");
            }
        }
        crate::util::record("npm rebuild", t.elapsed().as_secs_f64());
        crate::info!("  npm rebuild (deps): {:.2}s", t.elapsed().as_secs_f64());
        // 2. Hidden lockfile.
        // npm writes the hidden lockfile whenever node_modules exists, even with
        // no packages in it, but never creates node_modules just for it
        // (measured on a project without dependencies, with and without an
        // existing empty node_modules).
        let hidden = self.hidden_lockfile(plan);
        if root.join("node_modules").is_dir() {
            let text = serde_json::to_string_pretty(&hidden)? + "\n";
            std::fs::write(root.join("node_modules/.package-lock.json"), text)?;
        }
        // 3. Root lifecycle scripts, as ci.js runs them.
        let t = std::time::Instant::now();
        if !self.ignore_scripts {
            let npm_dir = crate::util::npm_dir().context("cannot locate npm's installation")?;
            // Under the store's tmp dir (user-owned), created fresh: never through
            // a pre-existing path in a shared /tmp.
            let helper = crate::default_store(None).join("tmp").join(format!("{}-root-scripts.js", std::process::id()));
            let _ = std::fs::remove_file(&helper);
            {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&helper)?;
                f.write_all(include_str!("root-scripts.js").as_bytes())?;
            }
            let st = crate::util::npm_cmd()
                .args(["exec", "--no", "--", "node"])
                .arg(&helper)
                .arg(&npm_dir)
                .arg(root)
                .current_dir(root)
                .status()
                .context("running root lifecycle scripts")?;
            let _ = std::fs::remove_file(&helper);
            if !st.success() {
                return Err(crate::util::ScriptFailure(format!("root lifecycle script failed: {st}")).into());
            }
        }
        crate::util::record("root scripts", t.elapsed().as_secs_f64());
        crate::info!("  root lifecycle scripts: {:.2}s", t.elapsed().as_secs_f64());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lockfile(packages: Value) -> Lockfile {
        let raw = json!({ "name": "p", "version": "1.0.0", "lockfileVersion": 3, "requires": true, "packages": packages });
        let Value::Object(raw) = raw else { unreachable!() };
        Lockfile::from_raw(Path::new("/nonexistent"), raw).unwrap()
    }

    fn pkg(extra: Value) -> Value {
        let mut v = json!({ "version": "1.0.0", "resolved": "https://registry.npmjs.org/x/-/x-1.0.0.tgz",
                            "integrity": "sha512-AAAA" });
        if let (Value::Object(m), Value::Object(x)) = (&mut v, extra) {
            m.extend(x);
        }
        v
    }

    fn hidden(lf: &Lockfile, plan: &Plan) -> Value {
        NpmRebuild { lock_raw: lf.raw.clone(), ignore_scripts: true, extra_args: vec![] }.hidden_lockfile(plan)
    }

    #[test]
    fn npm_flags_replace_stale_lockfile_flags_in_the_hidden_lockfile() {
        // R9: the lockfile says peer, npm computes dev; another entry gains optional.
        let mut lf = lockfile(json!({
            "": { "name": "p" },
            "node_modules/a": pkg(json!({ "peer": true, "license": "MIT" })),
            "node_modules/b": pkg(json!({})),
            "node_modules/c": pkg(json!({ "dev": true })),
        }));
        let ideal = parse_ideal(
            &json!({ "nodes": { "node_modules/a": { "dev": true }, "node_modules/b": { "optional": true },
                                "node_modules/c": { "dev": true, "peer": false } } }),
            &lf,
        )
        .unwrap();
        apply_flags(&mut lf, &ideal);
        let plan = build_plan(&lf, Some(&ideal.paths)).unwrap();
        let h = hidden(&lf, &plan);
        let p = &h["packages"];
        assert_eq!(p["node_modules/a"]["dev"], json!(true));
        assert!(p["node_modules/a"].get("peer").is_none(), "stale peer flag copied: {}", p["node_modules/a"]);
        assert_eq!(p["node_modules/a"]["license"], json!("MIT"), "other fields kept");
        assert_eq!(p["node_modules/b"]["optional"], json!(true));
        assert!(p["node_modules/c"].get("peer").is_none(), "a false flag is not written");
        // The parsed entries follow npm too (they drive the optional-package skip).
        let b = lf.entries.iter().find(|e| e.path == "node_modules/b").unwrap();
        assert!(b.optional && !b.dev_optional);
    }

    #[test]
    fn entries_outside_the_ideal_tree_keep_their_flags() {
        let mut lf = lockfile(json!({
            "": { "name": "p" },
            "node_modules/a": pkg(json!({ "peer": true })),
            "node_modules/x": pkg(json!({ "extraneous": true, "dev": true })),
        }));
        let ideal = parse_ideal(&json!({ "nodes": { "node_modules/a": {} } }), &lf).unwrap();
        apply_flags(&mut lf, &ideal);
        let plan = build_plan(&lf, Some(&ideal.paths)).unwrap();
        let h = hidden(&lf, &plan);
        assert!(h["packages"]["node_modules/a"].get("peer").is_none());
        // npm's hidden lockfile keeps extraneous entries, as the lockfile has them.
        assert_eq!(h["packages"]["node_modules/x"]["dev"], json!(true));
        assert!(plan.skipped.iter().any(|(p, why)| p == "node_modules/x" && why.starts_with(EXTRANEOUS)));
    }

    #[test]
    fn ideal_node_missing_from_the_lockfile_is_refused() {
        let lf = lockfile(json!({ "": { "name": "p" }, "node_modules/a": pkg(json!({})) }));
        let err = parse_ideal(&json!({ "nodes": { "node_modules/a": {}, "node_modules/b": {} } }), &lf).unwrap_err();
        assert!(format!("{err}").contains("node_modules/b"), "{err}");
        assert!(parse_ideal(&json!({}), &lf).is_err(), "no `nodes`");
    }

    #[test]
    fn deduplicated_nested_entry_is_not_installed_nor_written() {
        // npm ci dedupes a nested copy the lockfile still records (overrides case).
        let mut lf = lockfile(json!({
            "": { "name": "p" },
            "node_modules/a": pkg(json!({})),
            "node_modules/a/node_modules/ms": pkg(json!({})),
            "node_modules/ms": pkg(json!({})),
        }));
        let ideal = parse_ideal(&json!({ "nodes": { "node_modules/a": {}, "node_modules/ms": {} } }), &lf).unwrap();
        apply_flags(&mut lf, &ideal);
        let plan = build_plan(&lf, Some(&ideal.paths)).unwrap();
        assert!(!plan.entries.iter().any(|e| e.install_path == "node_modules/a/node_modules/ms"));
        assert!(hidden(&lf, &plan)["packages"].get("node_modules/a/node_modules/ms").is_none());
    }

    #[test]
    fn extraneous_is_classified_before_the_ideal_filter() {
        // An extraneous entry is never in npm's ideal tree; it must still be
        // skipped AS extraneous, which keeps it in the hidden lockfile.
        let lf = lockfile(json!({
            "": { "name": "p" },
            "node_modules/x": pkg(json!({ "extraneous": true })),
        }));
        let plan = build_plan(&lf, Some(&HashSet::new())).unwrap();
        let why = &plan.skipped.iter().find(|(p, _)| p == "node_modules/x").unwrap().1;
        assert!(why.starts_with(EXTRANEOUS), "{why}");
        assert!(hidden(&lf, &plan)["packages"].get("node_modules/x").is_some());
    }

    #[test]
    fn lockfile_paths_that_escape_or_sit_under_a_link_are_refused() {
        for bad in ["../outside", "/abs/node_modules/a", "node_modules/../../x", "./node_modules/a"] {
            let lf = lockfile(json!({ "": {}, bad: pkg(json!({})) }));
            assert!(validate_paths(&lf).is_err(), "{bad} accepted");
        }
        let lf = lockfile(json!({
            "": {},
            "node_modules/ws": { "resolved": "packages/ws", "link": true },
            "node_modules/ws/node_modules/evil": pkg(json!({})),
        }));
        assert!(validate_paths(&lf).is_err(), "entry under a link accepted");
        let ok = lockfile(json!({ "": {}, "node_modules/a/node_modules/b": pkg(json!({})), "packages/ws": {} }));
        assert!(validate_paths(&ok).is_ok());
    }

    #[test]
    fn key_order_matches_what_npm_wrote() {
        // Every entry of a lockfile npm wrote, keys reversed, must sort back to
        // npm's order (fixtures/small: 347 entries, flags, bins, engines, funding).
        let lock: Value = serde_json::from_str(include_str!("../fixtures/small/package-lock.json")).unwrap();
        let mut checked = 0;
        for (loc, entry) in lock["packages"].as_object().unwrap() {
            let entry = entry.as_object().unwrap();
            let mut shuffled: Map<String, Value> = entry.iter().rev().map(|(k, v)| (k.clone(), v.clone())).collect();
            npm_key_order(&mut shuffled);
            let want: Vec<&String> = entry.keys().collect();
            let got: Vec<&String> = shuffled.keys().collect();
            assert_eq!(got, want, "{loc}");
            checked += 1;
        }
        assert!(checked > 300);
    }

    #[test]
    fn hidden_lockfile_entries_are_laid_out_as_npm_writes_them() {
        // A flag appended at the end (a hand-edited lockfile, or a flag npm
        // added) goes where npm writes it, whether or not npm changed it.
        let mut lf = lockfile(json!({
            "": {},
            "node_modules/a": pkg(json!({ "license": "MIT", "bin": { "a": "a.js" }, "dev": true })),
            "node_modules/b": pkg(json!({ "license": "MIT", "peer": true, "dependencies": { "a": "1" } })),
        }));
        let ideal = parse_ideal(&json!({ "nodes": { "node_modules/a": { "dev": true }, "node_modules/b": { "dev": true } } }), &lf).unwrap();
        apply_flags(&mut lf, &ideal);
        let plan = build_plan(&lf, Some(&ideal.paths)).unwrap();
        let h = hidden(&lf, &plan);
        let keys = |p: &str| h["packages"][p].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys("node_modules/a"), ["version", "resolved", "integrity", "dev", "license", "bin"]);
        assert_eq!(keys("node_modules/b"), ["version", "resolved", "integrity", "dev", "license", "dependencies"]);
    }

    #[test]
    fn workspace_globs_match_like_npm() {
        assert!(glob_match("packages/*", "packages/a"));
        assert!(!glob_match("packages/*", "packages/a/b"));
        assert!(glob_match("packages/**", "packages/a/b"));
        assert!(glob_match("apps/*-web", "apps/admin-web"));
        assert!(!glob_match("apps/*-web", "apps/admin-api"));
        assert!(glob_match("tools/cli", "tools/cli"));
        let lf = lockfile(json!({ "": { "workspaces": { "packages": ["./packages/*", "apps/web/"] } } }));
        assert_eq!(workspace_globs(&lf), vec!["packages/*", "apps/web"]);
        let lf = lockfile(json!({ "": { "workspaces": ["libs/*"] } }));
        assert_eq!(workspace_globs(&lf), vec!["libs/*"]);
    }

    #[test]
    fn dependency_resolution_walks_up_like_node() {
        let keys: HashSet<&str> = ["node_modules/a", "node_modules/b", "node_modules/a/node_modules/b"].into_iter().collect();
        assert_eq!(resolve(&keys, "node_modules/a", "b"), Some("node_modules/a/node_modules/b"));
        assert_eq!(resolve(&keys, "node_modules/b", "a"), Some("node_modules/a"));
        assert_eq!(resolve(&keys, "packages/ws", "b"), Some("node_modules/b"));
        assert_eq!(resolve(&keys, "", "c"), None);
    }
}
