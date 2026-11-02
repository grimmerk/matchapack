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

/// Stand-in for npm ci's "package.json and package-lock.json are in sync" check.
/// For the root and each workspace, every declared dependency must either equal
/// the lockfile's record, or be a semver range (or `npm:name@range` alias) that
/// the locked version still satisfies -- judged by npm's own bundled `semver`,
/// so range semantics are npm's. Added or removed dependencies and non-registry
/// specs that changed are refused (npm ci would resolve or prune them).
/// Not covered: a change to `overrides`.
pub fn check_in_sync(root: &Path, lf: &Lockfile) -> Result<Option<HashSet<String>>> {
    const FIELDS: [&str; 4] = ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"];
    let pkgs = lf.raw.get("packages").and_then(|v| v.as_object()).context("no packages")?;
    let keys: HashSet<&str> = pkgs.keys().map(|k| k.as_str()).collect();
    let mut dirs = vec![String::new()];
    for e in &lf.entries {
        if e.link {
            if let Some(t) = &e.resolved {
                if !t.starts_with("..") && !t.contains("node_modules/") && !dirs.contains(t) {
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
        let out = crate::util::npm_cmd().args(["root", "-g"]).output().context("npm root -g")?;
        let semver = Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("npm/node_modules/semver");
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
    // validates, and writes nothing (~1 s small, ~2.5 s monorepo-A).
    let root_pj: Value = serde_json::from_str(&std::fs::read_to_string(root.join("package.json"))?)?;
    let has_overrides = root_pj.get("overrides").and_then(|o| o.as_object()).is_some_and(|o| !o.is_empty());
    if has_overrides {
        // The dry-run's `add` list is a DIFF against the node_modules on disk, so
        // run it in a scratch copy that has none: package.json, lockfile, .npmrc
        // and each workspace's package.json. Then `add` is the whole ideal tree.
        let scratch = std::env::temp_dir().join(format!("lockclone-dryrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let scratch = std::fs::canonicalize(&scratch)?;
        let result = dry_run_ideal(root, &scratch, lf);
        let _ = std::fs::remove_dir_all(&scratch);
        return result.map(Some);
    }
    Ok(None)
}

fn dry_run_ideal(root: &Path, scratch: &Path, lf: &Lockfile) -> Result<HashSet<String>> {
    for f in ["package.json", "package-lock.json", ".npmrc"] {
        if root.join(f).is_file() {
            std::fs::copy(root.join(f), scratch.join(f))?;
        }
    }
    for e in &lf.entries {
        if let (true, Some(t)) = (e.link, &e.resolved) {
            if !t.starts_with("..") && !t.contains("node_modules/") && root.join(t).join("package.json").is_file() {
                std::fs::create_dir_all(scratch.join(t))?;
                std::fs::copy(root.join(t).join("package.json"), scratch.join(t).join("package.json"))?;
            }
        }
    }
    let root = scratch;
    {
        let out = crate::util::npm_cmd()
            // --ignore-scripts: ci.js runs the root lifecycle scripts even under
            // --dry-run (measured: `husky` exit 127, app's workspace build
            // failing, with no node_modules yet); validation does not need them.
            .args(["ci", "--dry-run", "--json", "--ignore-scripts", "--no-audit", "--no-fund"])
            .current_dir(root)
            .output()
            .context("running npm ci --dry-run")?;
        if !out.status.success() {
            bail!(
                "`npm ci --dry-run` rejected this lockfile (package.json has `overrides`):\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // npm ci installs its IDEAL tree, not the lockfile text, and its sync
        // check only asks that every ideal node be in the lockfile, not the
        // reverse. With overrides the lockfile npm itself writes can hold
        // redundant nested copies that the ideal tree dedupes (measured: three
        // nested ms@2.1.2 identical to the root one; npm ci installed 344 of 347
        // entries). The dry-run's JSON summary (printed after the "add x y"
        // lines) lists every ideal node's path: use it as the authority.
        let text = String::from_utf8_lossy(&out.stdout);
        let start = text
            .find("\n{")
            .map(|i| i + 1)
            .or_else(|| text.starts_with('{').then_some(0))
            .context("no JSON summary in `npm ci --dry-run --json` output")?;
        let summary: Value = serde_json::from_str(&text[start..]).context("parsing npm ci --dry-run --json summary")?;
        let prefix = format!("{}/", root.display());
        let mut ideal = HashSet::new();
        for a in summary.get("add").and_then(|v| v.as_array()).context("dry-run summary has no `add` list")? {
            let p = a.get("path").and_then(|v| v.as_str()).context("dry-run `add` entry without path")?;
            match p.strip_prefix(&prefix) {
                Some(rel) => {
                    ideal.insert(rel.to_string());
                }
                None if p == root.display().to_string() => {}
                None => bail!("npm's ideal tree has a node outside the project: {p}"),
            }
        }
        let in_lock: HashSet<&str> = lf.entries.iter().map(|e| e.path.as_str()).collect();
        let moved: Vec<&String> = ideal.iter().filter(|p| !in_lock.contains(p.as_str())).collect();
        if !moved.is_empty() {
            bail!("npm's ideal tree places packages at paths the lockfile does not record (use `npm ci`): {moved:?}");
        }
        Ok(ideal)
    }
}

const EXTRANEOUS: &str = "extraneous";

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
    for e in &lf.entries {
        if e.link {
            let target = e.resolved.clone().context("link entry without resolved")?;
            if !target.starts_with("..") && !target.contains("node_modules/") && !plan.owned_roots.contains(&target) {
                plan.owned_roots.push(target.clone());
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
                    unsupported.push(format!("{} (resolved {:?})", e.path, e.resolved));
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
                pk.insert(k.clone(), v.clone());
            }
        }
        out.insert("packages".into(), Value::Object(pk));
        Value::Object(out)
    }
}

impl PostInstall for NpmRebuild {
    /// Mirrors `npm ci` (npm/lib/commands/ci.js): reify = dependency scripts +
    /// bin links + hidden lockfile, THEN the root project's seven lifecycle
    /// events. The order matters: root scripts that touch the tree (monorepo-A
    /// builds a workspace in `postinstall`) leave npm's hidden lockfile stale,
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
                return Err(crate::util::ScriptFailure(format!("npm rebuild failed: {st}")).into());
            }
        }
        crate::info!("  npm rebuild (deps): {:.2}s", t.elapsed().as_secs_f64());
        // 2. Hidden lockfile.
        let hidden = self.hidden_lockfile(plan);
        let text = serde_json::to_string_pretty(&hidden)? + "\n";
        std::fs::write(root.join("node_modules/.package-lock.json"), text)?;
        // 3. Root lifecycle scripts, as ci.js runs them.
        let t = std::time::Instant::now();
        if !self.ignore_scripts {
            let out = crate::util::npm_cmd().args(["root", "-g"]).output().context("npm root -g")?;
            let npm_dir = Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("npm");
            let helper = std::env::temp_dir().join(format!("lockclone-root-scripts-{}.js", std::process::id()));
            std::fs::write(&helper, include_str!("root-scripts.js"))?;
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
        crate::info!("  root lifecycle scripts: {:.2}s", t.elapsed().as_secs_f64());
        Ok(())
    }
}
