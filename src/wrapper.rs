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
/// plain npm until the harness has been re-run: scripts/verify-npm.sh runs it
/// on the npm on PATH and, when every case is identical, records the version
/// in `<config dir>/npm-verified`, which is read as well (override:
/// THINCLONE_NPM_VERSIONS = comma list, or `any`).
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
    let engine = match crate::inject::engine() {
        Ok(e) => e,
        Err(reason) => {
            log("declined", json!({ "reason": reason }));
            return decline(&args, &reason);
        }
    };
    if engine == crate::inject::Engine::Inject {
        return run_inject(&args, &log);
    }
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
        // Only the inject engine returns this.
        Err(InstallError::Npm { code, .. }) => code,
    }
}

/// `npm ci` through the inject engine: npm itself runs, with the user's
/// arguments unchanged; thinclone declines only what it must know before
/// npm starts.
fn run_inject(args: &[String], log: &dyn Fn(&str, Value)) -> i32 {
    let opts = match decide_inject(args) {
        Ok(o) => o,
        Err(reason) => {
            log("declined", json!({ "engine": "inject", "reason": reason }));
            return decline(args, &reason);
        }
    };
    util::set_quiet(true);
    eprintln!("thinclone: npm ci with clones from the store (THINCLONE=0 npm ci for plain npm)");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::inject::run(&opts)));
    let outcome = match outcome {
        Ok(o) => o,
        Err(_) => Err(InstallError::Broke(anyhow::anyhow!("internal error (panic)"))),
    };
    match outcome {
        Ok(s) => {
            let by_npm = s.inject.as_ref().and_then(|i| i["fallbacks"].as_object()).map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>()).unwrap_or(0);
            eprintln!(
                "thinclone: added {} packages from the store in {:.1}s (inject engine; npm extracted {by_npm})",
                s.placed, s.secs
            );
            log(
                "installed",
                json!({ "engine": "inject", "secs": s.secs, "packages": s.placed, "store_new": s.store_new,
                        "bytes_logical": s.bytes_logical, "bytes_store_new": s.bytes_store_new, "inject": s.inject }),
            );
            0
        }
        Err(InstallError::Declined(e)) => {
            log("declined", json!({ "engine": "inject", "reason": one_line(&e) }));
            decline(args, &one_line(&e))
        }
        // npm ran and failed, and has said why: its status is the answer.
        Err(InstallError::Npm { code, inject }) => {
            log("npm_failed", json!({ "engine": "inject", "code": code, "reason": format!("npm exited with status {code}"), "inject": inject }));
            code
        }
        Err(InstallError::Scripts(e)) | Err(InstallError::Broke(e)) => {
            log("broke", json!({ "engine": "inject", "reason": one_line(&e) }));
            eprintln!("thinclone: failed while npm was running ({}); running npm ci to rebuild node_modules", one_line(&e));
            exec_npm(args)
        }
    }
}

/// What thinclone must know before npm starts, for the inject engine. npm's
/// flags and settings are npm's to read: whatever they make it install, the
/// hook replaces only extractions it can reproduce exactly.
fn decide_inject(args: &[String]) -> Result<crate::inject::InjectOpts, String> {
    // The clone server accepts destinations inside this directory only; with
    // these npm installs somewhere else.
    for a in &args[1..] {
        let name = a.split('=').next().unwrap_or(a);
        if ["--prefix", "-C", "--global", "-g", "--location"].contains(&name) {
            return Err(format!("option {a} moves the install out of this directory"));
        }
        // Not installs: nothing to clone, and a run that places nothing
        // would read in the log as a hook that no longer fires.
        if ["--help", "-h", "-?", "--usage", "--dry-run"].contains(&name) {
            return Err(format!("option {a} installs nothing"));
        }
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // Without a package.json here npm looks for the project in a parent
    // directory. Without a lockfile here either `npm ci` refuses, or this is
    // a workspace of a monorepo and npm installs into the root's
    // node_modules, outside this directory: nothing could be cloned
    // (grimmerk/thinclone#36 for doing better).
    if !cwd.join("package.json").is_file() {
        return Err("no package.json in this directory".into());
    }
    if !cwd.join("package-lock.json").is_file() && !cwd.join("npm-shrinkwrap.json").is_file() {
        return Err("no package-lock.json or npm-shrinkwrap.json in this directory (npm would install elsewhere, or refuse)".into());
    }
    trial::in_scope(&cwd)?;
    Ok(crate::inject::InjectOpts {
        dir: cwd,
        store: crate::default_store(None),
        clone_mode: crate::default_clone_mode(),
        npm_args: args.to_vec(),
    })
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
    util::remove_run_dir();
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
    let process_umask = process_umask();
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
        npm_args: args[1..].to_vec(),
    })
}

/// This process's umask (read by setting and restoring it: there is no other
/// call).
pub fn process_umask() -> u32 {
    unsafe {
        let m = libc::umask(0o022);
        libc::umask(m);
        m as u32
    }
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
    // npm ci ignores `call` (an `npm exec` option). It was off this list while
    // the root scripts ran through `npm exec`, which refuses `call` together
    // with a command; src/post-install.js does not use `npm exec`.
    "call",
];

/// npm's settings as npm loads them, through src/npm-config.js run with npm's
/// own @npmcli/config: `changed` (setting -> source) and `effective` values.
fn npm_config(cwd: &std::path::Path) -> Result<Value, String> {
    let npm_dir = util::npm_dir().ok_or("cannot locate npm's installation")?;
    let helper = util::private_run_dir().map_err(|e| format!("npm config helper: {e}"))?.join("npm-config.js");
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
        None => {
            let local = std::fs::read_to_string(util::config_dir().join("npm-verified")).unwrap_or_default();
            VERIFIED_NPM.iter().any(|v| version_matches(&version, v))
                || verified_lines(&local).any(|v| version_matches(&version, v))
        }
    };
    if !ok {
        return Err(format!("npm {version} has not been verified against npm ci"));
    }
    Ok(())
}

/// npm major versions the inject engine runs on. It has no code per npm
/// version: what keeps it right on a new release is checked at run time (the
/// hook installs itself only when pacote has the expected shape; the first
/// install under a new pacote/tar version compares every store entry it uses
/// with pacote's extraction; npm placing packages the hook never saw is
/// reported). A major is listed once the inject harness has run on it
/// (`THINCLONE_ENGINE=inject scripts/verify-npm.sh`, `scripts/test-inject.sh`):
/// 10 (10.8.2 and 10.9.2, also a 3-workspace project; NOTES), 11 (11.17–11.20
/// in use), 12 (12.2.2).
pub const INJECT_NPM_MAJORS: &[u32] = &[10, 11, 12];

/// The inject engine's version gate: THINCLONE_NPM_VERSIONS when set (`any`, or
/// a list as for the classic engine), otherwise the majors above. Separate
/// from the classic engine's list of verified minors, which it does not need.
pub fn inject_npm_version_gate() -> Result<(), String> {
    let version = util::npm_version().ok_or("could not determine the npm version")?;
    inject_version_allowed(&version, std::env::var("THINCLONE_NPM_VERSIONS").ok().as_deref())
}

fn inject_version_allowed(version: &str, allowed: Option<&str>) -> Result<(), String> {
    let ok = match allowed {
        Some("any") => true,
        Some(list) => list.split(',').any(|v| version_matches(version, v.trim())),
        None => version.split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| INJECT_NPM_MAJORS.contains(&m)),
    };
    if ok {
        Ok(())
    } else {
        let majors: Vec<String> = INJECT_NPM_MAJORS.iter().map(u32::to_string).collect();
        Err(format!("npm {version}: the inject engine runs on npm {} (THINCLONE_NPM_VERSIONS=any to try another)", majors.join(" and ")))
    }
}

/// "11.19" matches 11.19.x; "11" matches 11.x; an exact version matches itself.
fn version_matches(version: &str, pattern: &str) -> bool {
    version == pattern || version.starts_with(&format!("{pattern}."))
}

/// The versions in an npm-verified file: the first word of each line; `#`
/// starts a comment.
fn verified_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter_map(|l| l.split('#').next()?.split_whitespace().next())
}

#[cfg(test)]
mod tests {
    use super::{inject_version_allowed, verified_lines, version_matches};

    #[test]
    fn inject_engine_npm_majors() {
        for v in ["10.8.2", "10.9.2", "11.0.0", "11.17.0", "11.20.0", "12.2.2"] {
            assert!(inject_version_allowed(v, None).is_ok(), "{v}");
        }
        for v in ["9.9.9", "13.0.0", "1.0.0", "110.0.0", "", "x"] {
            assert!(inject_version_allowed(v, None).is_err(), "{v}");
        }
        assert!(inject_version_allowed("13.0.0", Some("any")).is_ok());
        assert!(inject_version_allowed("13.1.0", Some("13.1")).is_ok());
        assert!(inject_version_allowed("11.20.0", Some("13.1")).is_err(), "an explicit list replaces the majors");
    }

    #[test]
    fn npm_verified_file_lines() {
        let text = "# recorded by scripts/verify-npm.sh\n11.20  # 2026-11-06, build abc\n\n  12.0.1\n";
        let v: Vec<&str> = verified_lines(text).collect();
        assert_eq!(v, ["11.20", "12.0.1"]);
        assert!(v.iter().any(|p| version_matches("11.20.3", p)));
        assert!(!v.iter().any(|p| version_matches("11.2.0", p)), "11.20 must not match 11.2.x");
        assert!(!v.iter().any(|p| version_matches("12.0.2", p)), "an exact version matches only itself");
    }
}
