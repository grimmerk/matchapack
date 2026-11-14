//! `matchapack npm <args>`: what the shell wrapper calls instead of npm.
//!
//! Only `npm ci` (or one of its aliases) is a candidate. Everything else, and
//! every candidate that fails a check, runs the real npm with the original
//! arguments, so the worst case is plain npm.
//!
//! The inject engine (`run_inject`) takes `npm ci` with any arguments and
//! declines before npm starts only what needs no node (`decide_inject`,
//! `inject::gates`: npm major 10–12, umask 022, the exact flags that make it
//! global or install nothing, a start directory outside the trial scope).
//! The gates that depend on where npm installs (the trial scope, a symlinked
//! node_modules, a global install set in an npmrc) run on npm's own install
//! root when npm reports it (serve.rs `accept_root`; #36).

use crate::{trial, util, InstallError};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::process::Command;

const CI_ALIASES: [&str; 5] = ["ci", "clean-install", "ic", "install-clean", "isntall-clean"];

pub fn run(args: Vec<String>) -> i32 {
    if std::env::var("MATCHAPACK").as_deref() == Ok("0") {
        return exec_npm(&args);
    }
    if !args.first().is_some_and(|a| CI_ALIASES.contains(&a.as_str())) {
        return exec_npm(&args);
    }
    // Before anything of the install runs (`load_end`: trial::append).
    let load_start = trial::loadavg();
    let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let npm_version = util::npm_version().unwrap_or_default();
    let log = |outcome: &str, extra: Value| {
        let mut rec = json!({ "outcome": outcome, "dir": dir, "npm": npm_version, "args": args, "phases": util::phases(), "load_start": load_start });
        if let (Value::Object(m), Value::Object(x)) = (&mut rec, extra) {
            m.extend(x);
        }
        trial::append(rec);
    };
    // MATCHAPACK_ENGINE: unset, empty or `inject`; anything else (`classic`,
    // removed in PR C2, included) is plain npm with the reason. Logged as
    // the inject engine's run: `report` counts runs without an engine as
    // the classic engine's, from logs written before PR C2.
    if let Err(reason) = crate::inject::engine() {
        log("declined", json!({ "engine": "inject", "reason": reason }));
        return decline(&args, &reason);
    }
    run_inject(&args, &log)
}

/// `npm ci` through the inject engine: npm itself runs, with the user's
/// arguments unchanged; matchapack declines only what it must know before
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
    eprintln!("matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::inject::run(&opts)));
    let outcome = match outcome {
        Ok(o) => o,
        Err(_) => Err(InstallError::Broke(anyhow::anyhow!("internal error (panic)"))),
    };
    match outcome {
        Ok(s) => {
            let by_npm = s.inject.as_ref().and_then(|i| i["fallbacks"].as_object()).map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>()).unwrap_or(0);
            // Without the hook npm extracted every package it installed.
            let by_npm = match s.inject.as_ref() {
                Some(i) if i["hook"] == "not-installed" => i["installed"].as_u64().unwrap_or(by_npm),
                _ => by_npm,
            };
            eprintln!(
                "matchapack: added {} packages from the store in {:.1}s (inject engine; npm extracted {by_npm})",
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
        Err(InstallError::Broke(e)) => {
            log("broke", json!({ "engine": "inject", "reason": one_line(&e) }));
            eprintln!("matchapack: failed while npm was running ({}); running npm ci to rebuild node_modules", one_line(&e));
            exec_npm(args)
        }
    }
}

/// What matchapack declines before npm starts, for the inject engine: only
/// what needs no node, as a shortcut. The authority is npm's own install
/// root and settings, checked when npm reports them (serve.rs
/// `accept_root`): abbreviations (`--glob`), the environment and npmrc files
/// (`global=true`), the workspace walk and `--prefix` are npm's to read.
/// Whatever npm's flags make it install, the hook replaces only extractions
/// it can reproduce exactly.
fn decide_inject(args: &[String]) -> Result<crate::inject::InjectOpts, String> {
    let prefix_typed = early_flags(&args[1..])?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // Without --prefix npm installs into this directory or an ancestor of it,
    // and the trial scope is a string prefix: a directory outside it has no
    // ancestor inside it. With --prefix the root decides (serve.rs).
    if !prefix_typed {
        trial::in_scope(&cwd)?;
    }
    Ok(crate::inject::InjectOpts {
        dir: cwd,
        trial_scope: true,
        store: crate::default_store(None),
        clone_mode: crate::default_clone_mode(),
        npm_args: args.to_vec(),
    })
}

/// The exact spellings of npm's flags that decline before node starts
/// (npm's other spellings are caught on its own values, serve.rs), and
/// whether `--prefix` / `-C` was typed.
fn early_flags(rest: &[String]) -> Result<bool, String> {
    let mut prefix_typed = false;
    for (i, a) in rest.iter().enumerate() {
        let name = a.split('=').next().unwrap_or(a);
        let global = ["-g", "--global"].contains(&a.as_str())
            || a == "--location=global"
            || (a == "--location" && rest.get(i + 1).map(String::as_str) == Some("global"));
        if global {
            return Err(format!("option {a} makes npm install globally"));
        }
        // Not installs: nothing to clone, and a run that places nothing
        // would read in the log as a hook that no longer fires.
        if ["--help", "-h", "-?", "--usage", "--dry-run", "--package-lock-only"].contains(&a.as_str()) {
            return Err(format!("option {a} installs nothing"));
        }
        if ["--prefix", "-C"].contains(&name) {
            prefix_typed = true;
        }
    }
    Ok(prefix_typed)
}

/// matchapack will not handle this install and nothing has been changed. Run
/// plain npm, unless MATCHAPACK_NO_FALLBACK=1 (scripts/convert-existing.sh):
/// then report and exit 3, leaving the existing node_modules as it is.
fn decline(args: &[String], reason: &str) -> i32 {
    if std::env::var("MATCHAPACK_NO_FALLBACK").as_deref() == Ok("1") {
        eprintln!("matchapack: not handled, left unchanged ({reason})");
        return 3;
    }
    eprintln!("matchapack: running plain npm instead ({reason})");
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

/// Replace this process with the real npm. `MATCHAPACK=0` keeps any wrapper
/// from routing back here.
fn exec_npm(args: &[String]) -> i32 {
    util::remove_run_dir();
    let err = Command::new("npm").args(args).env("MATCHAPACK", "0").exec();
    eprintln!("matchapack: could not start npm: {err}");
    127
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

/// npm major versions the inject engine runs on. It has no code per npm
/// version: what keeps it right on a new release is checked at run time (the
/// hook installs itself only when pacote has the expected shape; the first
/// install under a new pacote/tar version compares every store entry it uses
/// with pacote's extraction; npm placing packages the hook never saw is
/// reported). A major is listed once the inject harness has run on it
/// (`scripts/verify-npm.sh`, `scripts/test-inject.sh`):
/// 10 (10.8.2 and 10.9.2, also a 3-workspace project; 10.5.2, 10.7.0 and
/// 10.9.10; NOTES), 11 (11.17–11.20 in use), 12 (12.2.2). Inside a major,
/// the pacote fingerprint gate (`npm_fingerprints`) decides.
pub const INJECT_NPM_MAJORS: &[u32] = &[10, 11, 12];

/// The inject engine's version gate: MATCHAPACK_NPM_VERSIONS when set (`any`,
/// or a comma list of versions: `11.19` matches 11.19.x, `11` matches 11.x),
/// otherwise the majors above.
pub fn inject_npm_version_gate() -> Result<(), String> {
    let version = util::npm_version()?;
    inject_version_allowed(&version, std::env::var("MATCHAPACK_NPM_VERSIONS").ok().as_deref())
}

fn inject_version_allowed(version: &str, allowed: Option<&str>) -> Result<(), String> {
    let ok = match allowed {
        Some("any") => true,
        Some(list) => list.split(',').any(|v| version_matches(version, v.trim())),
        None => version.split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| INJECT_NPM_MAJORS.contains(&m)),
    };
    if ok {
        Ok(())
    } else if let Some(list) = allowed {
        Err(format!("npm {version} is not in MATCHAPACK_NPM_VERSIONS ({list})"))
    } else {
        let majors: Vec<String> = INJECT_NPM_MAJORS.iter().map(u32::to_string).collect();
        Err(format!("npm {version}: the inject engine (the default) runs on npm {} (MATCHAPACK_NPM_VERSIONS=any to try another)", majors.join(", ")))
    }
}

/// "11.19" matches 11.19.x; "11" matches 11.x; an exact version matches itself.
fn version_matches(version: &str, pattern: &str) -> bool {
    version == pattern || version.starts_with(&format!("{pattern}."))
}

/// The pacote code the hook may be installed on: fingerprints (SHA-256 of
/// pacote's lib/fetcher.js, the file of its FetcherBase class, `node
/// src/inject.js --fingerprint <npm dir>`) of the npm releases the harness has run on, with the versions
/// and the date (src/npm-fingerprints.json; NOTES "G-lite: npm release
/// safety"). Inside a listed major, this is what decides whether an npm
/// release the binary has never seen gets the hook: it does when its pacote
/// code is byte for byte code that was tested.
const NPM_FINGERPRINTS: &str = include_str!("npm-fingerprints.json");

pub fn npm_fingerprints() -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(NPM_FINGERPRINTS).expect("src/npm-fingerprints.json is JSON");
    v["fingerprints"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

fn is_fingerprint(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What the hook is given as MATCHAPACK_INJECT_FINGERPRINTS: `any`, or the
/// fingerprints above plus the user's MATCHAPACK_NPM_FINGERPRINT (`any`, or
/// a comma list of fingerprints as matchapack prints them). The hook checks
/// its npm's pacote against it before npm starts, and declines (plain npm,
/// with the fingerprint in the message) when it is not on it.
pub fn inject_fingerprints() -> Result<String, String> {
    fingerprints_allowed(std::env::var("MATCHAPACK_NPM_FINGERPRINT").ok().as_deref())
}

fn fingerprints_allowed(user: Option<&str>) -> Result<String, String> {
    let mut list = npm_fingerprints();
    match user.map(str::trim) {
        None | Some("") => {}
        Some("any") => return Ok("any".into()),
        Some(extra) => {
            for f in extra.split(',').map(|f| f.trim().to_ascii_lowercase()).filter(|f| !f.is_empty()) {
                if !is_fingerprint(&f) {
                    return Err(format!("MATCHAPACK_NPM_FINGERPRINT: {f} is not a fingerprint (64 hexadecimal digits, as matchapack prints it) or `any`"));
                }
                list.push(f);
            }
        }
    }
    Ok(list.join(","))
}

#[cfg(test)]
mod tests {
    use super::{early_flags, fingerprints_allowed, inject_version_allowed, is_fingerprint, npm_fingerprints};

    #[test]
    fn flags_that_decline_before_node_starts() {
        let f = |a: &[&str]| early_flags(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        for a in [&["-g"][..], &["--global"], &["--location=global"], &["--no-audit", "--location", "global"]] {
            assert!(f(a).unwrap_err().contains("makes npm install globally"), "{a:?}");
        }
        for a in [&["--dry-run"][..], &["--help"], &["-h"], &["-?"], &["--usage"], &["--package-lock-only"]] {
            assert!(f(a).unwrap_err().contains("installs nothing"), "{a:?}");
        }
        // npm's to decide on its own values: a local location, abbreviations,
        // a value that only looks like a flag.
        for a in [&["--location=project"][..], &["--location", "project"], &["--glob"], &["--omit=dev"], &[]] {
            assert_eq!(f(a), Ok(false), "{a:?}");
        }
        // --prefix and -C are supported: the start directory's scope check is
        // then skipped (the root npm reports is checked).
        for a in [&["--prefix", "x"][..], &["--prefix=x"], &["-C", "x"], &["--no-audit", "-C=x"]] {
            assert_eq!(f(a), Ok(true), "{a:?}");
        }
    }

    #[test]
    fn npm_fingerprint_list() {
        let list = npm_fingerprints();
        // The eight texts of pacote's lib/fetcher.js in npm 10.0.0 to 12.2.2
        // (NOTES "The lazy hook and npm's install root"), each listed after
        // test-inject.sh and verify-npm.sh passed on a release with it.
        for p in ["07f98090ee5bc371", "8d7208cf1a9e98ee", "cef1bd59a34dfe60", "1a385341f75d6c77", "45eabb392a0c7c33", "6972084f1fa0d668", "97dbf9a402ad3329", "b7d5a64232232301"] {
            assert!(list.iter().any(|f| f.starts_with(p)), "{p}");
        }
        assert_eq!(list.len(), 8, "{list:?}");
        assert!(list.iter().all(|f| is_fingerprint(f) && *f == f.to_ascii_lowercase()), "{list:?}");
        let v: serde_json::Value = serde_json::from_str(super::NPM_FINGERPRINTS).unwrap();
        for (f, e) in v["fingerprints"].as_object().unwrap() {
            assert!(e["verified_on"].as_array().is_some_and(|a| !a.is_empty()) && e["added"].is_string(), "{f}: verified_on and added");
        }
        let joined = list.join(",");
        assert_eq!(fingerprints_allowed(None).unwrap(), joined);
        assert_eq!(fingerprints_allowed(Some(" ")).unwrap(), joined);
        assert_eq!(fingerprints_allowed(Some("any")).unwrap(), "any");
        // The user's fingerprints are added to the list, not put in its place.
        let mine = "AB".repeat(32);
        assert_eq!(fingerprints_allowed(Some(&format!(" {mine} ,"))).unwrap(), format!("{joined},{}", mine.to_ascii_lowercase()));
        let e = fingerprints_allowed(Some("e89576a3")).unwrap_err();
        assert!(e.contains("e89576a3 is not a fingerprint"), "{e}");
    }

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
        let e = inject_version_allowed("11.20.0", Some("13.1")).unwrap_err();
        assert!(e.contains("not in MATCHAPACK_NPM_VERSIONS (13.1)"), "an explicit list replaces the majors, and says so: {e}");
    }
}
