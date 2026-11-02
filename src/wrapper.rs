//! `lockclone npm <args>`: what the shell wrapper calls instead of npm.
//!
//! Only a bare `npm ci` (or one of its aliases, with output-only flags) is a
//! candidate. Everything else, and every candidate that fails a check, runs the
//! real npm with the original arguments, so the worst case is plain npm.
//! Checks, all before anything in the project changes:
//!   - npm's version is one the harness was run against (VERIFIED_NPM);
//!   - no npm setting changes the shape of the tree (omit, install-strategy, …);
//!   - the project has package.json + package-lock.json and no shrinkwrap;
//!   - lockclone's own prepare step succeeds (lockfile support, sync, store,
//!     reflink probe).
//! After node_modules has been changed: a failing lifecycle script is reported
//! as npm would report it; any other lockclone failure hands over to npm, whose
//! `npm ci` wipes and rebuilds node_modules.

use crate::{clone, trial, util, InstallError, InstallOpts};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

const CI_ALIASES: [&str; 5] = ["ci", "clean-install", "ic", "install-clean", "isntall-clean"];

/// npm major.minor versions the harness was run against. Another version runs
/// plain npm until the harness has been re-run (override: LOCKCLONE_NPM_VERSIONS
/// = comma list, or `any`).
const VERIFIED_NPM: &[&str] = &["11.19", "11.17"];

/// Flags that change only npm's output or network policy, never the tree.
const PASSIVE_FLAGS: [&str; 11] = [
    "--no-audit",
    "--audit=false",
    "--no-fund",
    "--fund=false",
    "--prefer-offline",
    "--no-progress",
    "--progress=false",
    "--foreground-scripts",
    "--silent",
    "--quiet",
    "-s",
];

pub fn run(args: Vec<String>) -> i32 {
    if std::env::var("LOCKCLONE").as_deref() == Ok("0") {
        return exec_npm(&args);
    }
    if !args.first().is_some_and(|a| CI_ALIASES.contains(&a.as_str())) {
        return exec_npm(&args);
    }
    let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let npm_version = util::npm_version().unwrap_or_default();
    let log = |outcome: &str, extra: Value| {
        let mut rec = json!({ "outcome": outcome, "dir": dir, "npm": npm_version, "args": args });
        if let (Value::Object(m), Value::Object(x)) = (&mut rec, extra) {
            m.extend(x);
        }
        trial::append(rec);
    };
    let opts = match decide(&args) {
        Ok(o) => o,
        Err(reason) => {
            log("declined", json!({ "reason": reason }));
            return decline(&args, &reason);
        }
    };
    util::set_quiet(true);
    eprintln!("lockclone: npm ci from the store (LOCKCLONE=0 npm ci for plain npm)");
    match crate::run_install(&opts) {
        Ok(s) => {
            eprintln!("lockclone: added {} packages from the store in {:.1}s", s.placed, s.secs);
            log(
                "installed",
                json!({ "secs": s.secs, "packages": s.placed, "store_new": s.store_new,
                        "bytes_logical": s.bytes_logical, "bytes_store_new": s.bytes_store_new }),
            );
            0
        }
        Err(InstallError::Declined(e)) => {
            log("declined", json!({ "reason": one_line(&e) }));
            decline(&args, &one_line(&e))
        }
        Err(InstallError::Scripts(e)) => {
            log("script_failed", json!({ "reason": one_line(&e) }));
            eprintln!("lockclone: {e:#}");
            1
        }
        Err(InstallError::Broke(e)) => {
            log("broke", json!({ "reason": one_line(&e) }));
            eprintln!("lockclone: failed after changing node_modules ({}); running npm ci to rebuild it", one_line(&e));
            exec_npm(&args)
        }
    }
}

/// lockclone will not handle this install and nothing has been changed. Run
/// plain npm, unless LOCKCLONE_NO_FALLBACK=1 (scripts/convert-existing.sh):
/// then report and exit 3, leaving the existing node_modules as it is.
fn decline(args: &[String], reason: &str) -> i32 {
    if std::env::var("LOCKCLONE_NO_FALLBACK").as_deref() == Ok("1") {
        eprintln!("lockclone: not handled, left unchanged ({reason})");
        return 3;
    }
    eprintln!("lockclone: running plain npm instead ({reason})");
    exec_npm(args)
}

fn one_line(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    let first = s.lines().next().unwrap_or("").to_string();
    if s.lines().count() > 1 {
        format!("{first} …")
    } else {
        first
    }
}

/// Replace this process with the real npm. `LOCKCLONE=0` keeps any wrapper
/// from routing back here.
fn exec_npm(args: &[String]) -> i32 {
    let err = Command::new("npm").args(args).env("LOCKCLONE", "0").exec();
    eprintln!("lockclone: could not start npm: {err}");
    127
}

fn decide(args: &[String]) -> Result<InstallOpts, String> {
    let mut ignore_scripts = false;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        if a == "--ignore-scripts" {
            ignore_scripts = true;
        } else if a == "--loglevel" {
            it.next();
        } else if PASSIVE_FLAGS.contains(&a.as_str()) || a.starts_with("--loglevel=") {
        } else {
            return Err(format!("option {a} is not handled"));
        }
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    if !cwd.join("package.json").is_file() || !cwd.join("package-lock.json").is_file() {
        return Err("no package.json + package-lock.json in this directory".into());
    }
    if cwd.join("npm-shrinkwrap.json").exists() {
        return Err("npm-shrinkwrap.json is not handled".into());
    }
    trial::in_scope(&cwd)?;

    let version = util::npm_version().ok_or("could not determine the npm version")?;
    let allowed = std::env::var("LOCKCLONE_NPM_VERSIONS").ok();
    let ok = match allowed.as_deref() {
        Some("any") => true,
        Some(list) => list.split(',').any(|v| version_matches(&version, v.trim())),
        None => VERIFIED_NPM.iter().any(|v| version_matches(&version, v)),
    };
    if !ok {
        return Err(format!("npm {version} has not been verified against npm ci"));
    }

    let out = util::npm_cmd()
        .args(["config", "list", "--json"])
        .current_dir(&cwd)
        .output()
        .map_err(|e| format!("npm config list: {e}"))?;
    let cfg: Value = serde_json::from_slice(&out.stdout).map_err(|_| "could not read npm config".to_string())?;
    let get = |k: &str| cfg.get(k).cloned().unwrap_or(Value::Null);
    let nonempty = |k: &str| get(k).as_array().is_some_and(|a| !a.is_empty());
    let is_true = |k: &str| get(k).as_bool() == Some(true);
    if nonempty("omit") {
        return Err(format!("npm omits {} (NODE_ENV=production or --omit)", get("omit")));
    }
    if get("install-strategy").as_str().is_some_and(|s| s != "hoisted") {
        return Err(format!("install-strategy={}", get("install-strategy")));
    }
    // engine-strict: npm ci fails when a package's `engines` excludes this Node.
    for k in ["global-style", "legacy-bundling", "legacy-peer-deps", "strict-peer-deps", "force", "dry-run", "global", "install-links", "engine-strict"] {
        if is_true(k) {
            return Err(format!("npm config {k}=true"));
        }
    }
    for k in ["bin-links", "package-lock"] {
        if get(k).as_bool() == Some(false) {
            return Err(format!("npm config {k}=false"));
        }
    }
    if nonempty("workspace") || !get("workspaces").is_null() {
        return Err("workspace selection is not handled".into());
    }
    for k in ["os", "cpu", "libc"] {
        if !get(k).is_null() {
            return Err(format!("npm config {k} is set"));
        }
    }
    if is_true("ignore-scripts") {
        ignore_scripts = true;
    }
    let cache = get("cache").as_str().map(PathBuf::from).unwrap_or_else(|| crate::home().join(".npm"));

    Ok(InstallOpts {
        dir: cwd,
        store: crate::default_store(None),
        cacache: cache.join("_cacache"),
        clone_mode: crate::default_clone_mode(),
        mtime: clone::Mtime::Now,
        ignore_scripts,
        no_rebuild: false,
    })
}

/// "11.19" matches 11.19.x; "11" matches 11.x; an exact version matches itself.
fn version_matches(version: &str, pattern: &str) -> bool {
    version == pattern || version.starts_with(&format!("{pattern}."))
}
