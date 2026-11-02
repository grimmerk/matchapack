//! Turn `Source::Registry { name, version }` plan entries into tarball URL +
//! integrity, the way `npm ci` does for lockfile entries without `resolved`:
//! from the package's registry document (packument). Packuments are read from
//! npm's own cache; misses are filled with one batched `npm cache add`.

use crate::plan::{Kind, Plan, Source};
use crate::store::cacache_read;
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

struct Registries {
    default: String,
    scoped: HashMap<String, String>,
}

impl Registries {
    fn load(root: &Path) -> Result<Registries> {
        let out = crate::util::npm_cmd().args(["config", "list", "--json"]).current_dir(root).output().context("npm config list")?;
        let v: Value = serde_json::from_slice(&out.stdout).context("parsing npm config list --json")?;
        let mut scoped = HashMap::new();
        if let Value::Object(m) = &v {
            for (k, val) in m {
                if let (Some(scope), Some(url)) = (k.strip_suffix(":registry"), val.as_str()) {
                    if scope.starts_with('@') {
                        scoped.insert(scope.to_string(), url.to_string());
                    }
                }
            }
        }
        let default = v.get("registry").and_then(|x| x.as_str()).unwrap_or("https://registry.npmjs.org/").to_string();
        Ok(Registries { default, scoped })
    }

    /// The packument URL npm-registry-fetch requests: registry + escaped name.
    fn packument_url(&self, name: &str) -> String {
        let reg = name
            .split_once('/')
            .filter(|_| name.starts_with('@'))
            .and_then(|(scope, _)| self.scoped.get(scope))
            .unwrap_or(&self.default);
        let reg = if reg.ends_with('/') { reg.clone() } else { format!("{reg}/") };
        format!("{reg}{}", name.replace('/', "%2f"))
    }
}

/// dist.tarball and an SRI integrity (dist.integrity, else sha1 from dist.shasum).
fn dist_from(packument: &[u8], version: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(packument).ok()?;
    let dist = v.get("versions")?.get(version)?.get("dist")?;
    let url = dist.get("tarball")?.as_str()?.to_string();
    let integrity = match dist.get("integrity").and_then(|x| x.as_str()) {
        Some(i) => i.to_string(),
        None => {
            let sha = hex::decode(dist.get("shasum")?.as_str()?).ok()?;
            format!("sha1-{}", base64::engine::general_purpose::STANDARD.encode(sha))
        }
    };
    Some((url, integrity))
}

pub fn resolve(plan: &mut Plan, root: &Path, cacache: &Path) -> Result<usize> {
    let wanted: Vec<(String, String)> = plan
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Package { source: Source::Registry { name, version }, .. } => Some((name.clone(), version.clone())),
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return Ok(0);
    }
    let regs = Registries::load(root)?;
    let lookup = |name: &str, version: &str| {
        let key = format!("make-fetch-happen:request-cache:{}", regs.packument_url(name));
        cacache_read(cacache, &key).and_then(|p| dist_from(&p, version))
    };
    let mut found: HashMap<(String, String), (String, String)> = HashMap::new();
    let mut missing = vec![];
    for (n, v) in &wanted {
        if found.contains_key(&(n.clone(), v.clone())) {
            continue;
        }
        match lookup(n, v) {
            Some(d) => {
                found.insert((n.clone(), v.clone()), d);
            }
            None => missing.push(format!("{n}@{v}")),
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        crate::info!("fetching {} packuments with npm cache add", missing.len());
        for chunk in missing.chunks(200) {
            let st = crate::util::npm_cmd().args(["cache", "add"]).args(chunk).current_dir(root).status()?;
            if !st.success() {
                bail!("npm cache add failed for registry specs ({st})");
            }
        }
        for spec in &missing {
            let (n, v) = spec.rsplit_once('@').unwrap();
            let d = lookup(n, v).with_context(|| format!("no packument entry for {spec} even after npm cache add"))?;
            found.insert((n.to_string(), v.to_string()), d);
        }
    }
    for e in plan.entries.iter_mut() {
        if let Kind::Package { integrity, source } = &mut e.kind {
            if let Source::Registry { name, version } = source {
                let (url, integ) = found[&(name.clone(), version.clone())].clone();
                // Keep a lockfile-supplied integrity; it is what npm ci checks against.
                if integrity.is_none() {
                    *integrity = Some(integ);
                }
                *source = Source::Tarball { url };
            }
        }
    }
    Ok(wanted.len())
}
