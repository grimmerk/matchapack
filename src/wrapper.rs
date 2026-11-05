//! `thinclone npm <args>`: what the shell wrapper calls instead of npm.
//!
//! Only a bare `npm ci` (or one of its aliases, with output-only flags) is a
//! candidate. Everything else, and every candidate that fails a check, runs the
//! real npm with the original arguments, so the worst case is plain npm.
//! Checks, all before anything in the project changes:
//!   - npm's version is one the harness was run against (VERIFIED_NPM);
//!   - no npm setting changes the shape of the tree (omit, install-strategy, …);
//!   - the project has package.json + package-lock.json and no shrinkwrap;
//!   - thinclone's own prepare step succeeds (lockfile support, sync, store,
//!     reflink probe).
//! After node_modules has been changed: a failing lifecycle script is reported
//! as npm would report it; any other thinclone failure hands over to npm, whose
//! `npm ci` wipes and rebuilds node_modules.

use crate::{clone, trial, util, InstallError, InstallOpts};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

const CI_ALIASES: [&str; 5] = ["ci", "clean-install", "ic", "install-clean", "isntall-clean"];

/// npm major.minor versions the harness was run against. Another version runs
/// plain npm until the harness has been re-run (override: THINCLONE_NPM_VERSIONS
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
    if std::env::var("THINCLONE").as_deref() == Ok("0") {
        return exec_npm(&args);
    }
    if !args.first().is_some_and(|a| CI_ALIASES.contains(&a.as_str())) {
        return exec_npm(&args);
    }
    let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let npm_version = util::npm_version().unwrap_or_default();
    let log = |outcome: &str, extra: Value| {
        let mut rec = json!({ "outcome": outcome, "dir": dir, "npm": npm_version, "args": args, "phases": util::phases() });
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
    eprintln!("thinclone: npm ci from the store (THINCLONE=0 npm ci for plain npm)");
    // A panic must not skip the fallback: npm ci rebuilds node_modules from
    // whatever state it is in.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::run_install(&opts)));
    let outcome = match outcome {
        Ok(o) => o,
        Err(_) => Err(InstallError::Broke(anyhow::anyhow!("internal error (panic)"))),
    };
    match outcome {
        Ok(s) => {
            eprintln!("thinclone: added {} packages from the store in {:.1}s", s.placed, s.secs);
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
            eprintln!("thinclone: {e:#}");
            1
        }
        Err(InstallError::Broke(e)) => {
            log("broke", json!({ "reason": one_line(&e) }));
            eprintln!("thinclone: failed after changing node_modules ({}); running npm ci to rebuild it", one_line(&e));
            exec_npm(&args)
        }
    }
}

/// thinclone will not handle this install and nothing has been changed. Run
/// plain npm, unless THINCLONE_NO_FALLBACK=1 (scripts/convert-existing.sh):
/// then report and exit 3, leaving the existing node_modules as it is.
fn decline(args: &[String], reason: &str) -> i32 {
    if std::env::var("THINCLONE_NO_FALLBACK").as_deref() == Ok("1") {
        eprintln!("thinclone: not handled, left unchanged ({reason})");
        return 3;
    }
    eprintln!("thinclone: running plain npm instead ({reason})");
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

/// Replace this process with the real npm. `THINCLONE=0` keeps any wrapper
/// from routing back here.
fn exec_npm(args: &[String]) -> i32 {
    let err = Command::new("npm").args(args).env("THINCLONE", "0").exec();
    eprintln!("thinclone: could not start npm: {err}");
    127
}

fn decide(args: &[String]) -> Result<InstallOpts, String> {
    let mut ignore_scripts = false;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        if a == "--ignore-scripts" {
            ignore_scripts = true;
        } else if a == "--legacy-peer-deps" {
            // Changes npm's ideal tree (peer dependencies are not installed);
            // every npm thinclone starts gets it, so the dry-run follows it.
            util::set_npm_config("legacy-peer-deps", "true");
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

    npm_version_gate()?;

    let cfg = npm_config(&cwd)?;
    // First gate (an allow-list): every npm setting changed from npm's
    // default, in any npmrc or the environment, must be one known not to
    // change what `npm ci` installs or how. Settings npm does not define are
    // not listed by the helper (npm ignores them).
    let mut unknown: Vec<String> = cfg
        .get("changed")
        .and_then(|c| c.as_object())
        .ok_or("could not read npm config")?
        .iter()
        .filter(|(k, _)| !SAFE_CONFIG.contains(&k.as_str()))
        .map(|(k, w)| format!("{k} (from {})", w.as_str().unwrap_or("?")))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!("npm config not on thinclone's list of settings known to be safe: {}", unknown.join(", ")));
    }
    // Second gate: the effective values, defaults included (NODE_ENV=production
    // makes `omit` default to dev without any setting being changed).
    let eff = cfg.get("effective").cloned().unwrap_or(Value::Null);
    let get = |k: &str| eff.get(k).cloned().unwrap_or(Value::Null);
    let nonempty = |k: &str| get(k).as_array().is_some_and(|a| !a.is_empty());
    let is_true = |k: &str| get(k).as_bool() == Some(true);
    if nonempty("omit") {
        return Err(format!("npm omits {} (NODE_ENV=production or --omit)", get("omit")));
    }
    if get("install-strategy").as_str().is_some_and(|s| s != "hoisted") {
        return Err(format!("install-strategy={}", get("install-strategy")));
    }
    // engine-strict is NOT refused here: the `npm ci --dry-run` every install
    // runs applies it (measured: an engines mismatch fails it with EBADENGINE,
    // which hands over to npm; a project whose engines match is installed).
    // legacy-peer-deps is not refused: npm's own dry-run builds the ideal tree
    // with it (from .npmrc, the environment or the flag above), and thinclone
    // installs that tree.
    for k in ["global-style", "legacy-bundling", "strict-peer-deps", "force", "dry-run", "global", "install-links"] {
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
    // strict-allow-scripts: npm ci refuses BEFORE wiping when a dependency has
    // unreviewed install scripts; npm rebuild would only fail after the wipe.
    if is_true("strict-allow-scripts") && !ignore_scripts && !is_true("dangerously-allow-all-scripts") {
        return Err("npm config strict-allow-scripts=true".into());
    }
    // Source policies: npm refuses (or leaves out) packages from these sources
    // when restricted; thinclone does not apply them.
    for k in ["allow-remote", "allow-directory", "allow-file", "allow-git"] {
        if get(k).as_str().is_some_and(|v| v != "all") {
            return Err(format!("npm config {k}={}", get(k)));
        }
    }
    if !get("before").is_null() {
        return Err("npm config before is set".into());
    }
    // File modes: npm (pacote) applies the process umask and npm's `umask`
    // config; the store is extracted with 022.
    let process_umask = unsafe {
        let m = libc::umask(0o022);
        libc::umask(m);
        m as u32
    };
    if process_umask != 0o022 {
        return Err(format!("umask is {process_umask:03o}, the store assumes 022"));
    }
    if get("umask").as_u64().is_some_and(|u| u != 0) || get("umask").as_str().is_some_and(|u| u != "0") {
        return Err(format!("npm config umask={}", get("umask")));
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

/// npm settings that, changed from their default, leave `npm ci`'s result as
/// thinclone reproduces it. Anything else npm defines hands over to npm.
const SAFE_CONFIG: &[&str] = &[
    // registry, network, credentials, cache
    "registry", "scope", "_auth", "auth-type", "otp", "cache", "cache-max", "cache-min", "prefer-offline",
    "prefer-online", "fetch-retries", "fetch-retry-factor", "fetch-retry-maxtimeout", "fetch-retry-mintimeout",
    "fetch-timeout", "proxy", "https-proxy", "noproxy", "local-address", "strict-ssl", "ca", "cafile", "cert",
    "key", "maxsockets", "user-agent",
    // output only
    "loglevel", "logs-dir", "logs-max", "progress", "color", "unicode", "timing", "heading", "audit",
    "audit-level", "fund", "update-notifier", "foreground-scripts", "usage", "yes",
    // handled: thinclone follows npm (its own dry-run, npm rebuild, npm's
    // script runner): scripts on or off, peer handling, engines, release age
    "ignore-scripts", "legacy-peer-deps", "engine-strict", "min-release-age", "min-release-age-exclude",
    "script-shell", "node-gyp", "shell",
    // config file locations (their contents are loaded and checked too)
    "userconfig", "globalconfig",
    // used only by publish, init, version, save and search, never by npm ci
    "access", "provenance", "provenance-file", "tag", "tag-version-prefix", "preid", "message",
    "git-tag-version", "commit-hooks", "sign-git-commit", "sign-git-tag", "git", "save", "save-bundle",
    "save-dev", "save-exact", "save-optional", "save-peer", "save-prefix", "save-prod", "init-author-email",
    "init-author-name", "init-author-url", "init-license", "init-module", "init-type", "init-version",
    "init-private", "init.author.email", "init.author.name", "init.author.url", "init.license", "init.module",
    "init.version", "searchexclude", "searchlimit", "searchopts", "searchstaleness", "editor", "browser",
    "viewer",
    // Not `call`: npm ci ignores it, but `npm exec` (which runs the root
    // scripts helper) refuses a non-empty `call` together with a command.
];

/// npm's settings as npm loads them, through src/npm-config.js run with npm's
/// own @npmcli/config: `changed` (setting -> source) and `effective` values.
fn npm_config(cwd: &std::path::Path) -> Result<Value, String> {
    let npm_dir = util::npm_dir().ok_or("cannot locate npm's installation")?;
    let tmp = crate::default_store(None).join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    let helper = tmp.join(format!("{}-npm-config.js", std::process::id()));
    let _ = std::fs::remove_file(&helper);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&helper)
            .map_err(|e| format!("npm config helper: {e}"))?;
        f.write_all(include_str!("npm-config.js").as_bytes()).map_err(|e| format!("npm config helper: {e}"))?;
    }
    let out = util::node_cmd().arg(&helper).arg(&npm_dir).current_dir(cwd).output();
    let _ = std::fs::remove_file(&helper);
    let out = out.map_err(|e| format!("npm config helper: {e}"))?;
    if !out.status.success() {
        return Err(format!("could not read npm config: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|_| "could not read npm config".to_string())
}

/// The npm on PATH must be a version the harness was run against. Checked
/// here first (before reading npm's config) and again in `prepare`, so a
/// direct `thinclone install` is held to the same rule.
pub fn npm_version_gate() -> Result<(), String> {
    let version = util::npm_version().ok_or("could not determine the npm version")?;
    let allowed = std::env::var("THINCLONE_NPM_VERSIONS").ok();
    let ok = match allowed.as_deref() {
        Some("any") => true,
        Some(list) => list.split(',').any(|v| version_matches(&version, v.trim())),
        None => VERIFIED_NPM.iter().any(|v| version_matches(&version, v)),
    };
    if !ok {
        return Err(format!("npm {version} has not been verified against npm ci"));
    }
    Ok(())
}

/// "11.19" matches 11.19.x; "11" matches 11.x; an exact version matches itself.
fn version_matches(version: &str, pattern: &str) -> bool {
    version == pattern || version.starts_with(&format!("{pattern}."))
}
