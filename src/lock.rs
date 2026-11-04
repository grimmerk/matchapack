//! package-lock.json (v2/v3) reading. The lockfile is the only source of truth:
//! every package is placed at exactly the key path recorded in `packages`.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Entry {
    /// Key in `packages`, e.g. `node_modules/a/node_modules/b`.
    pub path: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub resolved: Option<String>,
    pub integrity: Option<String>,
    pub link: bool,
    pub optional: bool,
    pub dev_optional: bool,
    pub in_bundle: bool,
    pub extraneous: bool,
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
    /// Names of dependencies (deps + optional + peer), used for the optional-skip closure.
    pub deps: Vec<String>,
    /// The subset of `deps` that are optional edges (optionalDependencies, or a
    /// peer marked optional in peerDependenciesMeta).
    pub opt_deps: Vec<String>,
}

pub struct Lockfile {
    /// The project directory the lockfile belongs to.
    pub root: std::path::PathBuf,
    pub raw: Map<String, Value>,
    pub entries: Vec<Entry>,
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    }
}

fn dep_names(v: &Map<String, Value>) -> Vec<String> {
    let mut out = vec![];
    for k in ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"] {
        if let Some(Value::Object(m)) = v.get(k) {
            out.extend(m.keys().cloned());
        }
    }
    out
}

fn opt_dep_names(v: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = match v.get("optionalDependencies") {
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        _ => vec![],
    };
    if let Some(Value::Object(meta)) = v.get("peerDependenciesMeta") {
        for (k, m) in meta {
            if m.get("optional").and_then(|x| x.as_bool()) == Some(true) {
                out.push(k.clone());
            }
        }
    }
    out
}

impl Lockfile {
    pub fn load(root: &Path) -> Result<Lockfile> {
        let p = root.join("package-lock.json");
        let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let raw: Map<String, Value> = serde_json::from_str(&text).context("parsing package-lock.json")?;
        Lockfile::from_raw(root, raw)
    }

    /// A lockfile already parsed as JSON (`load` without the file read).
    pub fn from_raw(root: &Path, raw: Map<String, Value>) -> Result<Lockfile> {
        let ver = raw.get("lockfileVersion").and_then(|v| v.as_u64()).unwrap_or(0);
        if ver < 2 {
            bail!("lockfileVersion {ver} is not supported (need 2 or 3: the `packages` section)");
        }
        let pkgs = raw
            .get("packages")
            .and_then(|v| v.as_object())
            .context("package-lock.json has no `packages` object")?;
        let mut entries = Vec::with_capacity(pkgs.len());
        for (k, v) in pkgs {
            let o = v.as_object().context("package entry is not an object")?;
            let b = |f: &str| o.get(f).and_then(|x| x.as_bool()).unwrap_or(false);
            let s = |f: &str| o.get(f).and_then(|x| x.as_str()).map(String::from);
            entries.push(Entry {
                path: k.clone(),
                name: s("name"),
                version: s("version"),
                resolved: s("resolved"),
                integrity: s("integrity"),
                link: b("link"),
                optional: b("optional"),
                dev_optional: b("devOptional"),
                in_bundle: b("inBundle"),
                extraneous: b("extraneous"),
                os: str_list(o.get("os")),
                cpu: str_list(o.get("cpu")),
                libc: str_list(o.get("libc")),
                deps: dep_names(o),
                opt_deps: opt_dep_names(o),
            });
        }
        Ok(Lockfile { root: root.to_path_buf(), raw, entries })
    }
}

/// Number of `node_modules/` segments in a key: placement order (parents first).
pub fn depth(path: &str) -> usize {
    path.matches("node_modules/").count()
}

/// The package name implied by a key: the part after the last `node_modules/`.
pub fn name_from_path(path: &str) -> &str {
    match path.rfind("node_modules/") {
        Some(i) => &path[i + "node_modules/".len()..],
        None => path,
    }
}
