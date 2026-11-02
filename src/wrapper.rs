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

use crate::{clone, util, InstallError, InstallOpts};
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CI_ALIASES: [&str; 5] = ["ci", "clean-install", "ic", "install-clean", "isntall-clean"];

/// npm major.minor versions the harness was run against. Another version runs
/// plain npm until the harness has been re-run (override: LOCKCLONE_NPM_VERSIONS
/// = comma list, or `any`).
const VERIFIED_NPM: &[&str] = &["11.19"];

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
    let opts = match decide(&args) {
        Ok(o) => o,
        Err(reason) => return decline(&args, &reason),
    };
    util::set_quiet(true);
    eprintln!("lockclone: npm ci from the store (LOCKCLONE=0 npm ci for plain npm)");
    match crate::run_install(&opts) {
        Ok(s) => {
            eprintln!("lockclone: added {} packages from the store in {:.1}s", s.placed, s.secs);
            0
        }
        Err(InstallError::Declined(e)) => decline(&args, &one_line(&e)),
        Err(InstallError::Scripts(e)) => {
            eprintln!("lockclone: {e:#}");
            1
        }
        Err(InstallError::Broke(e)) => {
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

    let version = npm_version().ok_or("could not determine the npm version")?;
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
    for k in ["global-style", "legacy-bundling", "legacy-peer-deps", "strict-peer-deps", "force", "dry-run", "global", "install-links"] {
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

/// The version of the npm that `npm` on PATH resolves to, read from its own
/// package.json (bin/npm → ../lib/node_modules/npm/bin/npm-cli.js).
fn npm_version() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let bin = std::env::split_paths(&path).map(|d| d.join("npm")).find(|p| is_executable(p))?;
    let cli = std::fs::canonicalize(&bin).ok()?;
    let pkg = cli.parent()?.parent()?.join("package.json");
    let v: Value = serde_json::from_slice(&std::fs::read(pkg).ok()?).ok()?;
    if v.get("name").and_then(|n| n.as_str()) != Some("npm") {
        return None;
    }
    v.get("version").and_then(|x| x.as_str()).map(String::from)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}
