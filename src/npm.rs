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

pub fn build_plan(lf: &Lockfile) -> Result<Plan> {
    let mut plan = Plan { owned_roots: vec![String::new()], ..Default::default() };
    plan.skipped = platform_skips(lf)?;
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
                    Kind::Package { integrity: i.clone(), source: Source::Tarball { url: r.clone() } }
                }
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
        let skipped: HashSet<&str> = plan.skipped.iter().map(|(p, _)| p.as_str()).collect();
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
            if e.name == root_name {
                specs.push(root.join(&e.install_path).to_string_lossy().into_owned());
            } else if seen.insert(e.name.clone()) {
                specs.push(format!("{}@*", e.name));
            }
        }
        if !specs.is_empty() {
            let mut cmd = Command::new("npm");
            cmd.arg("rebuild").current_dir(root);
            if self.ignore_scripts {
                cmd.arg("--ignore-scripts");
            }
            cmd.args(&self.extra_args).args(&specs);
            let st = cmd.status().context("running npm rebuild")?;
            if !st.success() {
                bail!("npm rebuild failed: {st}");
            }
        }
        // 2. Hidden lockfile.
        let hidden = self.hidden_lockfile(plan);
        let text = serde_json::to_string_pretty(&hidden)? + "\n";
        std::fs::write(root.join("node_modules/.package-lock.json"), text)?;
        // 3. Root lifecycle scripts, as ci.js runs them.
        if !self.ignore_scripts {
            let out = Command::new("npm").args(["root", "-g"]).output().context("npm root -g")?;
            let npm_dir = Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("npm");
            let helper = std::env::temp_dir().join(format!("lockclone-root-scripts-{}.js", std::process::id()));
            std::fs::write(&helper, include_str!("root-scripts.js"))?;
            let st = Command::new("npm")
                .args(["exec", "--no", "--", "node"])
                .arg(&helper)
                .arg(&npm_dir)
                .arg(root)
                .current_dir(root)
                .status()
                .context("running root lifecycle scripts")?;
            let _ = std::fs::remove_file(&helper);
            if !st.success() {
                bail!("root lifecycle script failed: {st}");
            }
        }
        Ok(())
    }
}
