//! Verification harness: compare two project trees (an `npm ci` result and a
//! lockclone result). "Identical" means: same set of paths, and per path the
//! same type, permission bits, file content (blake3) and symlink target.
//! `node_modules/.package-lock.json` is compared as parsed JSON (and also
//! byte-for-byte, reported separately).

use anyhow::Result;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, Clone)]
enum Node {
    Dir { mode: u32 },
    File { mode: u32, hash: String, size: u64 },
    Link { target: PathBuf },
    Other,
}

const HIDDEN: &str = "node_modules/.package-lock.json";

fn scan(root: &Path) -> Result<BTreeMap<String, Node>> {
    let mut paths = vec![];
    for ent in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let ent = ent?;
        let rel = ent.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
        paths.push((rel, ent.path().to_path_buf(), ent.file_type()));
    }
    let nodes: Vec<(String, Node)> = paths
        .into_par_iter()
        .map(|(rel, p, ft)| {
            let n = if ft.is_symlink() {
                Node::Link { target: std::fs::read_link(&p).unwrap_or_default() }
            } else if ft.is_dir() {
                Node::Dir { mode: std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0) }
            } else if ft.is_file() {
                let md = std::fs::metadata(&p).unwrap();
                let data = std::fs::read(&p).unwrap_or_default();
                Node::File {
                    mode: md.permissions().mode() & 0o7777,
                    hash: blake3::hash(&data).to_hex().to_string(),
                    size: md.len(),
                }
            } else {
                Node::Other
            };
            (rel, n)
        })
        .collect();
    Ok(nodes.into_iter().collect())
}

pub struct Report {
    pub only_a: Vec<String>,
    pub only_b: Vec<String>,
    pub differ: Vec<(String, String)>,
    pub hidden_bytes_equal: Option<bool>,
    pub files: usize,
}

pub fn diff(a: &Path, b: &Path, ignore: &[String]) -> Result<Report> {
    let (ma, mb) = rayon::join(|| scan(a), || scan(b));
    let (ma, mb) = (ma?, mb?);
    let ignored = |p: &str| ignore.iter().any(|i| p == i || p.starts_with(&format!("{i}/")));
    let mut r = Report { only_a: vec![], only_b: vec![], differ: vec![], hidden_bytes_equal: None, files: ma.len() };
    for (k, va) in &ma {
        if ignored(k) {
            continue;
        }
        match mb.get(k) {
            None => r.only_a.push(k.clone()),
            Some(vb) if k == HIDDEN => {
                let ja: serde_json::Value = serde_json::from_slice(&std::fs::read(a.join(k))?)?;
                let jb: serde_json::Value = serde_json::from_slice(&std::fs::read(b.join(k))?)?;
                r.hidden_bytes_equal = Some(va == vb);
                if ja != jb {
                    r.differ.push((k.clone(), "hidden lockfile JSON differs".into()));
                }
            }
            Some(vb) if va != vb => r.differ.push((k.clone(), format!("a={va:?} b={vb:?}"))),
            _ => {}
        }
    }
    for k in mb.keys() {
        if !ignored(k) && !ma.contains_key(k) {
            r.only_b.push(k.clone());
        }
    }
    Ok(r)
}

impl Report {
    pub fn ok(&self) -> bool {
        self.only_a.is_empty() && self.only_b.is_empty() && self.differ.is_empty()
    }

    pub fn print_paths(&self) {
        for p in self.only_a.iter().chain(&self.only_b).chain(self.differ.iter().map(|(p, _)| p)) {
            println!("{p}");
        }
    }

    pub fn print(&self, limit: usize) {
        println!("scanned {} paths in A", self.files);
        let show = |title: &str, v: &Vec<String>| {
            println!("{title}: {}", v.len());
            for x in v.iter().take(limit) {
                println!("  {x}");
            }
        };
        show("only in A", &self.only_a);
        show("only in B", &self.only_b);
        println!("differ: {}", self.differ.len());
        for (k, why) in self.differ.iter().take(limit) {
            println!("  {k}: {why}");
        }
        if let Some(eq) = self.hidden_bytes_equal {
            println!("hidden lockfile byte-identical: {eq}");
        }
        println!("RESULT: {}", if self.ok() { "IDENTICAL" } else { "DIFFERENT" });
    }
}
