//! Trial-period bookkeeping, all outside any repository:
//!   ~/.cache/thinclone/log.jsonl     one line per `thinclone npm ci` decision
//!   ~/.cache/thinclone/verify.jsonl  one line per sampled verification
//!                                    (scripts/verify-sample.sh)
//!   ~/.config/thinclone/paths        optional trial scope: path prefixes
//! `thinclone report` summarises the two logs.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

fn cache_dir() -> PathBuf {
    std::env::var_os("THINCLONE_LOG_DIR").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".cache/thinclone"))
}

pub fn log_path() -> PathBuf {
    cache_dir().join("log.jsonl")
}

fn verify_log_path() -> PathBuf {
    cache_dir().join("verify.jsonl")
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Append one record; never fails the install (THINCLONE_LOG=0 disables).
pub fn append(mut rec: Value) {
    if std::env::var("THINCLONE_LOG").as_deref() == Ok("0") {
        return;
    }
    if let Value::Object(m) = &mut rec {
        m.insert("ts".into(), json!(now()));
        m.insert("thinclone".into(), json!(env!("CARGO_PKG_VERSION")));
    }
    let p = log_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{rec}");
    }
}

/// Trial scope: if ~/.config/thinclone/paths (or THINCLONE_PATHS, `:`-separated)
/// lists path prefixes, thinclone only handles projects under one of them.
pub fn in_scope(dir: &Path) -> std::result::Result<(), String> {
    let list: Vec<String> = match std::env::var("THINCLONE_PATHS") {
        Ok(v) => v.split(':').map(String::from).collect(),
        Err(_) => {
            let f = crate::home().join(".config/thinclone/paths");
            match std::fs::read_to_string(&f) {
                Ok(t) => t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect(),
                Err(_) => return Ok(()),
            }
        }
    };
    let home = crate::home();
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for p in &list {
        let p = p.strip_prefix("~/").map(|r| home.join(r)).unwrap_or_else(|| PathBuf::from(p));
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        // A plain string prefix on purpose: `~/git/app` covers the worktrees
        // `~/git/app-<topic>` too.
        if dir.to_string_lossy().starts_with(&*p.to_string_lossy()) {
            return Ok(());
        }
    }
    Err("outside the trial scope (~/.config/thinclone/paths or THINCLONE_PATHS)".into())
}

fn read_jsonl(p: &Path, since: f64) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0) >= since)
        .collect()
}

fn gib(b: f64) -> String {
    format!("{:.1} GiB", b / 1024f64.powi(3))
}

fn when(ts: f64) -> String {
    let out = std::process::Command::new("date").args(["-r", &format!("{}", ts as i64), "+%m-%d %H:%M"]).output();
    out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{ts:.0}"))
}

pub fn report(days: Option<f64>, store: &crate::store::Store) -> Result<()> {
    let since = days.map(|d| now() - d * 86400.0).unwrap_or(0.0);
    let runs = read_jsonl(&log_path(), since);
    let window = days.map(|d| format!("last {d} days")).unwrap_or_else(|| "all time".into());
    println!("thinclone report ({window}; {})", log_path().display());
    let count = |o: &str| runs.iter().filter(|r| r["outcome"] == o).count();
    println!(
        "runs {}: installed {}, plain npm {}, script failed {}, failed then npm {}",
        runs.len(),
        count("installed"),
        count("declined"),
        count("script_failed"),
        count("broke")
    );
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    for r in runs.iter().filter(|r| r["outcome"] != "installed") {
        let why = r["reason"].as_str().unwrap_or("?");
        let key: String = why.chars().take(90).collect();
        *reasons.entry(key).or_default() += 1;
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1));
    for (why, n) in reasons.iter().take(8) {
        println!("  {n}× {why}");
    }
    // An npm upgrade turns every install into plain npm without an error:
    // call it out on its own line, with the versions seen.
    let mut gated: std::collections::BTreeMap<String, usize> = Default::default();
    for r in runs.iter().filter(|r| r["outcome"] == "declined") {
        if r["reason"].as_str().is_some_and(|w| w.ends_with("has not been verified against npm ci")) {
            *gated.entry(r["npm"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        }
    }
    if !gated.is_empty() {
        let list: Vec<String> = gated.iter().map(|(v, n)| format!("npm {v}: {n}")).collect();
        println!(
            "npm version gate: {} runs went to plain npm ({}); run the harness on that npm, then add it to VERIFIED_NPM",
            gated.values().sum::<usize>(),
            list.join(", ")
        );
    }
    let inst: Vec<&Value> = runs.iter().filter(|r| r["outcome"] == "installed").collect();
    if !inst.is_empty() {
        let mut secs: Vec<f64> = inst.iter().filter_map(|r| r["secs"].as_f64()).collect();
        secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let logical: f64 = inst.iter().filter_map(|r| r["bytes_logical"].as_f64()).sum();
        let new: f64 = inst.iter().filter_map(|r| r["bytes_store_new"].as_f64()).sum();
        println!(
            "installs: median {:.1} s, max {:.1} s, total {:.0} s",
            secs[secs.len() / 2],
            secs.last().copied().unwrap_or(0.0),
            secs.iter().sum::<f64>()
        );
        println!(
            "space: {} placed as clones, {} of it new to the store → about {} not written \
             (file contents only; per-file metadata, roughly 0.1–0.3 GiB per large install, is not counted)",
            gib(logical),
            gib(new),
            gib(logical - new)
        );
    }
    let (n, bytes) = store.totals();
    println!("store: {} in {n} packages ({})", gib(bytes as f64), store.root.display());
    if let Ok(o) = std::process::Command::new("df").args(["-k", &crate::home().to_string_lossy()]).output() {
        if let Some(free) = String::from_utf8_lossy(&o.stdout).lines().nth(1).and_then(|l| l.split_whitespace().nth(3)).and_then(|f| f.parse::<f64>().ok()) {
            println!("free on the home volume now: {}", gib(free * 1024.0));
        }
    }
    println!("recent:");
    for r in runs.iter().rev().take(6) {
        let extra = match r["outcome"].as_str() {
            Some("installed") => {
                let mut s = format!("{:.1} s, {} packages", r["secs"].as_f64().unwrap_or(0.0), r["packages"]);
                if let Some(p) = r["phases"].as_object() {
                    let mut v: Vec<(&String, f64)> = p.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k, f))).collect();
                    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                    let top: Vec<String> = v.iter().take(3).map(|(k, f)| format!("{k} {f:.1}")).collect();
                    s += &format!("; slowest: {}", top.join(", "));
                }
                s
            }
            _ => r["reason"].as_str().unwrap_or("").chars().take(70).collect(),
        };
        println!("  {}  {:<13} {}  ({extra})", when(r["ts"].as_f64().unwrap_or(0.0)), r["outcome"].as_str().unwrap_or("?"), r["dir"].as_str().unwrap_or("?"));
    }
    let ver = read_jsonl(&verify_log_path(), since);
    if ver.is_empty() {
        println!("verification samples: none yet (scripts/verify-sample.sh)");
    } else {
        let ok = ver.iter().filter(|v| v["result"] == "IDENTICAL").count();
        println!("verification samples: {} run, {ok} identical", ver.len());
        for v in ver.iter().rev().take(5) {
            println!(
                "  {}  {:<10} {}  (npm ci {} s, thinclone {} s)",
                when(v["ts"].as_f64().unwrap_or(0.0)),
                v["result"].as_str().unwrap_or("?"),
                v["dir"].as_str().unwrap_or("?"),
                v["npm_ci_secs"],
                v["thinclone_secs"]
            );
        }
    }
    Ok(())
}
