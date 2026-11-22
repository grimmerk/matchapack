//! Trial-period bookkeeping, all outside any repository:
//!   ~/.cache/matchapack/log.jsonl     one line per `matchapack npm ci` decision
//!   ~/.cache/matchapack/verify.jsonl  one line per sampled verification
//!                                    (scripts/verify-sample.sh)
//!   ~/.config/matchapack/paths        optional trial scope: path prefixes
//! `matchapack report` summarises the two logs.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The trial logs' directory: $MATCHAPACK_LOG_DIR, else ~/.cache/matchapack.
pub fn cache_dir() -> PathBuf {
    std::env::var_os("MATCHAPACK_LOG_DIR").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".cache/matchapack"))
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

/// Options whose value is a secret, by a word in their name (npm's `--otp`,
/// `--_auth`, `--//host/:_authToken=…`, `--password`; Yarn's `--otp`), and
/// npm's client key (`--key=<PEM>`, also per registry; `--cert` is public).
fn secret_option(name: &str) -> bool {
    let n = name.trim_start_matches('-').to_ascii_lowercase();
    ["auth", "token", "password", "otp", "secret"].iter().any(|w| n.contains(w)) || n == "key" || n.ends_with(":key") || n.ends_with("_key")
}

/// `scheme://user:pass@host…` → `scheme://***@host…` (proxy and registry
/// URLs carry credentials that way), and the value of a query parameter
/// named like a secret (`?private_token=…`) → `***`. The user name of an
/// ssh URL without a password (`git+ssh://git@github.com/…`) is not a
/// secret and stays.
fn redact_userinfo(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let scheme: String = head[..i].chars().rev().take_while(|c| c.is_ascii_alphanumeric() || "+.-".contains(*c)).collect::<String>().chars().rev().collect();
        let end = tail.find(|c: char| c == '/' || c.is_whitespace()).unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) if scheme.to_ascii_lowercase().contains("ssh") && !tail[..at].contains(':') => out.push_str(&tail[..end]),
            Some(at) => {
                out.push_str("***");
                out.push_str(&tail[at..end]);
            }
            None => out.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    redact_query(&out)
}

/// The value of each `?name=value` / `&name=value` whose name is a secret's
/// (`secret_option`) → `***`.
fn redact_query(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(['?', '&']) {
        out.push_str(&rest[..=i]);
        rest = &rest[i + 1..];
        let end = rest.find(|c: char| c == '&' || c == '#' || c.is_whitespace()).unwrap_or(rest.len());
        match rest[..end].split_once('=') {
            Some((k, _)) if secret_option(k) => {
                out.push_str(k);
                out.push_str("=***");
            }
            _ => out.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A package manager's arguments as they may go into the log: the value of a
/// secret option (`--otp 123456`, `--_auth=…`) replaced by `***`, credentials
/// in a URL (`--proxy http://user:pass@host`) removed.
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for a in args {
        // A PEM private key in any word (`--key '-----BEGIN PRIVATE KEY-----…'`
        // starts with `-`, so the rule below would not hide it).
        if a.contains("PRIVATE KEY-----") || (a.contains("-----BEGIN") && a.contains("KEY-----")) {
            hide_next = false;
            // `--key=<PEM>` keeps its name; a key that is a word of its own
            // starts with `-----BEGIN` and may hold `=` (base64 padding):
            // then the whole word goes.
            let name = a.split('=').next().unwrap_or("");
            out.push(if a.starts_with("--") && a.contains('=') && !name.contains("-----BEGIN") { format!("{name}=***") } else { "***".to_string() });
            continue;
        }
        if std::mem::take(&mut hide_next) && !a.starts_with('-') {
            out.push("***".to_string());
            continue;
        }
        if a.starts_with('-') {
            let (name, value) = match a.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (a.as_str(), None),
            };
            if secret_option(name) {
                match value {
                    Some(_) => out.push(format!("{name}=***")),
                    None => {
                        out.push(a.clone());
                        hide_next = true;
                    }
                }
                continue;
            }
        }
        out.push(redact_userinfo(a));
    }
    out
}

/// What goes into the log carries no credentials: `args` arrays through
/// `redact_args`, every other string through `redact_userinfo` (a reason
/// can quote an argument).
fn redact(v: &mut Value) {
    redact_in(v, false)
}

/// `argv`: inside `repair` (src/inject.js `repairInfo`), where every list of
/// strings is command-line words (`args`, `missing`, any later one).
fn redact_in(v: &mut Value, argv: bool) {
    match v {
        Value::String(s) => *s = redact_userinfo(s),
        Value::Array(a) => a.iter_mut().for_each(|x| redact_in(x, argv)),
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                let words = argv || k == "args";
                match x {
                    Value::Array(a) if words && a.iter().all(|y| y.is_string()) => {
                        let strs: Vec<String> = a.iter().filter_map(|y| y.as_str().map(String::from)).collect();
                        *x = json!(redact_args(&strs));
                    }
                    _ => redact_in(x, argv || k == "repair"),
                }
            }
        }
        _ => {}
    }
}

/// The 1- and 5-minute load averages, to 0.01 (None where the system
/// gives none). Recorded at the start (`load_start`, by the caller) and
/// the end (`load_end`, by `append`) of an install, so a slow install can
/// be read against what else the machine was doing.
pub fn loadavg() -> Option<[f64; 2]> {
    let mut l = [0f64; 2];
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 2) };
    (n == 2).then(|| l.map(|x| (x * 100.0).round() / 100.0))
}

/// Append one record; never fails the install (MATCHAPACK_LOG=0 disables).
pub fn append(rec: Value) {
    if std::env::var("MATCHAPACK_LOG").as_deref() == Ok("0") {
        return;
    }
    append_to(&log_path(), rec);
}

fn append_to(p: &Path, mut rec: Value) {
    redact(&mut rec);
    if let Value::Object(m) = &mut rec {
        m.insert("ts".into(), json!(now()));
        m.insert("matchapack".into(), json!(env!("CARGO_PKG_VERSION")));
        // The git commit of this binary (build.rs): trial numbers from two
        // builds must not be mixed.
        m.insert("build".into(), json!(env!("MATCHAPACK_BUILD")));
        m.insert("load_end".into(), json!(loadavg()));
    }
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    append_line(p, format!("{rec}\n").as_bytes());
}

/// Add one whole line to a log that several processes append to (two
/// installs can end at the same moment). The line goes out in one
/// `write_all` (`writeln!` on a File writes each JSON token on its own,
/// and two records interleaved mid-line on 2026-11-11), under an exclusive
/// flock on the log: O_APPEND keeps one write(2) in one piece on Linux's
/// local filesystems in practice, but no filesystem promises it, APFS does
/// not document it, and a short write would be finished by a second call.
/// The lock is waited for at most about two seconds, then the line is
/// written anyway (as at once where flock is not supported): a log line
/// never holds an install back.
fn append_line(p: &Path, line: &[u8]) {
    use std::os::fd::AsRawFd;
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) else { return };
    for _ in 0..200 {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let e = std::io::Error::last_os_error().raw_os_error();
        if e != Some(libc::EWOULDBLOCK) && e != Some(libc::EINTR) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = f.write_all(line);
    // Closing the file releases the lock.
}

/// Trial scope: if ~/.config/matchapack/paths (or MATCHAPACK_PATHS, `:`-separated)
/// lists path prefixes, matchapack only handles projects under one of them.
pub fn in_scope(dir: &Path) -> std::result::Result<(), String> {
    let source = if std::env::var_os("MATCHAPACK_PATHS").is_some() { "MATCHAPACK_PATHS" } else { "~/.config/matchapack/paths" };
    let list: Vec<String> = match std::env::var("MATCHAPACK_PATHS") {
        Ok(v) => v.split(':').map(String::from).collect(),
        Err(_) => {
            let f = crate::util::config_dir().join("paths");
            match std::fs::read_to_string(&f) {
                Ok(t) => paths_file_entries(&t),
                Err(_) => return Ok(()),
            }
        }
    };
    in_scope_of(dir, &list, source, &crate::home())
}

/// The entries of a paths file: its lines, trimmed, without empty lines and
/// comments.
fn paths_file_entries(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
}

/// `in_scope` for the entries `list` (from `source`, for the message).
fn in_scope_of(dir: &Path, list: &[String], source: &str, home: &Path) -> std::result::Result<(), String> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Entries that can never match: not absolute (`$HOME/git` typed
    // literally, a relative path), or under a directory that does not exist.
    // A prefix that is no directory itself is a good entry (`~/git/app`
    // covers `~/git/app-<topic>`), and so is a directory removed since; those
    // decline in silence for the install family like any project outside
    // the scope. Said when nothing matched, since the scope then may not be
    // what the user meant.
    let mut never: Vec<(&str, &str)> = vec![];
    for e in list {
        let p = e.strip_prefix("~/").map(|r| home.join(r)).unwrap_or_else(|| PathBuf::from(e));
        // Not absolute: never matches, and is never resolved (against the
        // current directory, `.` would cover every directory).
        if !p.is_absolute() {
            never.push((e.as_str(), "not an absolute path"));
            continue;
        }
        // Symlinks resolved, of the entry or, when it does not exist, of its
        // parent: `/tmp/app` matches `/private/tmp/app-topic` on macOS.
        let resolved = std::fs::canonicalize(&p)
            .ok()
            .or_else(|| Some(std::fs::canonicalize(p.parent()?).ok()?.join(p.file_name()?)));
        // A plain string prefix on purpose: `~/git/app` covers the worktrees
        // `~/git/app-<topic>` too.
        let prefix = resolved.as_ref().unwrap_or(&p);
        if dir.to_string_lossy().starts_with(&*prefix.to_string_lossy()) {
            return Ok(());
        }
        if resolved.is_none() {
            never.push((e.as_str(), "its parent directory does not exist"));
        }
    }
    if never.len() == list.len() {
        return Err(format!("outside the trial scope: {source} lists no entry that can match"));
    }
    if let Some((e, why)) = never.first() {
        return Err(format!("outside the trial scope: {source} lists {e:?}, which can never match ({why})"));
    }
    Err(OUT_OF_SCOPE.into())
}

/// The reason `in_scope` gives (wrapper.rs `quiet_decline` compares it).
pub const OUT_OF_SCOPE: &str = "outside the trial scope (~/.config/matchapack/paths or MATCHAPACK_PATHS)";

/// The records from `since` on, and how many lines (of any date) were not
/// readable JSON: a line cut short or two interleaved lines are skipped,
/// never the whole report.
fn read_jsonl(p: &Path, since: f64) -> (Vec<Value>, usize) {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let mut bad = 0;
    let recs = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let v = serde_json::from_str::<Value>(l).ok();
            bad += v.is_none() as usize;
            v
        })
        .filter(|v| v.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0) >= since)
        .collect();
    (recs, bad)
}

fn skipped(bad: usize, p: &Path) {
    if bad > 0 {
        println!("{bad} unreadable line{} skipped ({})", if bad == 1 { "" } else { "s" }, p.display());
    }
}

/// `load 3.1→5.4`: the 1-minute load average when the install started and
/// when it ended (records from before both were logged: None).
fn load_note(r: &Value) -> Option<String> {
    let (a, b) = (r["load_start"][0].as_f64()?, r["load_end"][0].as_f64()?);
    Some(format!("load {a:.1}→{b:.1}"))
}

fn gib(b: f64) -> String {
    format!("{:.1} GiB", b / 1024f64.powi(3))
}

fn when(ts: f64) -> String {
    let out = std::process::Command::new("date").args(["-r", &format!("{}", ts as i64), "+%m-%d %H:%M"]).output();
    out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{ts:.0}"))
}

/// The `matchapack yarn` lines of the report (src/yarn1.rs): outcomes, why
/// installs went to plain Yarn, what was cloned and what Node copied (by
/// reason), and installs where Yarn linked but the hook saw no link step.
/// Clones of Yarn's cache are not the store's: their bytes are not added to
/// any "space" line.
fn report_yarn1(runs: &[Value]) {
    let count = |o: &str| runs.iter().filter(|r| r["outcome"] == o).count();
    println!(
        "yarn: {} runs of `matchapack yarn`: installed {}, plain yarn {}, yarn failed {}, broke {}{}",
        runs.len(),
        count("installed"),
        count("declined"),
        count("yarn_failed"),
        count("broke"),
        // Stopped by SIGTERM, SIGHUP or SIGINT (src/signals.rs, #64).
        if count("interrupted") > 0 { format!(", interrupted {}", count("interrupted")) } else { String::new() }
    );
    // Which commands (src/yarn1.rs `engine_command`; a record from before
    // the field existed is an install).
    let mut commands: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in runs {
        *commands.entry(r["command"].as_str().unwrap_or("install")).or_default() += 1;
    }
    if !commands.is_empty() {
        let list: Vec<String> = commands.iter().map(|(c, n)| format!("{c} {n}")).collect();
        println!("  by command: {}", list.join(", "));
    }
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    for r in runs.iter().filter(|r| r["outcome"] == "declined") {
        let key: String = r["reason"].as_str().unwrap_or("?").chars().take(90).collect();
        *reasons.entry(key).or_default() += 1;
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1));
    for (why, n) in reasons.iter().take(6) {
        println!("  plain yarn {n}×: {why}");
    }
    let done: Vec<&Value> = runs.iter().filter(|r| r["yarn1"].is_object()).collect();
    if done.is_empty() {
        return;
    }
    let sum = |k: &str| done.iter().filter_map(|r| r["yarn1"][k].as_u64()).sum::<u64>();
    let mut copied: std::collections::BTreeMap<String, u64> = Default::default();
    for r in &done {
        if let Some(m) = r["yarn1"]["copied"].as_object() {
            for (k, n) in m {
                *copied.entry(k.clone()).or_default() += n.as_u64().unwrap_or(0);
            }
        }
    }
    let mut copied: Vec<(String, u64)> = copied.into_iter().collect();
    copied.sort_by(|a, b| b.1.cmp(&a.1));
    let list: Vec<String> = copied.iter().take(6).map(|(k, n)| format!("{k} {n}")).collect();
    println!(
        "yarn: {} files cloned from Yarn's cache ({} of file contents, held by Yarn's cache, not the store), {} copied by Node{}",
        sum("cloned"),
        gib(sum("cloned_bytes") as f64),
        sum("copied_total"),
        if list.is_empty() { String::new() } else { format!(" ({})", list.join(", ")) }
    );
    let seen = done.iter().filter(|r| r["yarn1"]["link_steps"].as_u64().unwrap_or(0) > 0).count();
    println!("yarn: link step seen by the hook in {seen} of {} runs that ran Yarn", done.len());
    let unusable: Vec<&str> = done.iter().filter_map(|r| r["yarn1"]["cache"].as_str()).filter(|c| c.starts_with("unusable: ")).collect();
    if !unusable.is_empty() {
        println!("yarn: in {} runs nothing could be cloned from Yarn's cache (first: {})", unusable.len(), unusable[0].trim_start_matches("unusable: "));
    }
    // Yarn linked, but the hook never saw its link step: it no longer fits
    // this Yarn (or another Yarn ran). Installs stay correct; this is the
    // only sign.
    let blind: Vec<&&Value> = done.iter().filter(|r| r["yarn1"]["unseen"] == true).collect();
    if !blind.is_empty() {
        let mut versions: Vec<&str> = blind.iter().filter_map(|r| r["yarn"].as_str()).collect();
        versions.sort();
        versions.dedup();
        println!(
            "yarn: in {} runs Yarn linked node_modules but the hook never saw its link step (yarn {}): the hook may no longer fit this Yarn",
            blind.len(),
            versions.join(", ")
        );
    }
}

/// The times of the installed runs of `npm ci` (`ci`; also records without a
/// command, from before the install family was routed) or of the install
/// family (not `ci`), sorted.
fn install_secs<'a>(runs: impl Iterator<Item = &'a Value>, ci: bool) -> Vec<f64> {
    let mut secs: Vec<f64> = runs
        .filter(|r| r["outcome"] == "installed" && (r["command"].as_str().unwrap_or("ci") == "ci") == ci)
        .filter_map(|r| r["secs"].as_f64())
        .collect();
    secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    secs
}

/// `t` as `YYYY-MM-DD HH:MM UTC` (days to a civil date: H. Hinnant's
/// algorithm, proleptic Gregorian).
fn utc(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

pub fn report(days: Option<f64>, store: &crate::store::Store) -> Result<()> {
    let since = days.map(|d| now() - d * 86400.0).unwrap_or(0.0);
    let (all, bad) = read_jsonl(&log_path(), since);
    // `matchapack yarn` runs (src/yarn1.rs) are reported apart: they use no
    // store entry, so none of the npm lines below (store, space) count them.
    let (yruns, runs): (Vec<Value>, Vec<Value>) = all.iter().cloned().partition(|r| r["engine"] == "yarn1");
    let window = days.map(|d| format!("last {d} days")).unwrap_or_else(|| "all time".into());
    println!("matchapack report ({window}; {})", log_path().display());
    skipped(bad, &log_path());
    if !yruns.is_empty() {
        report_yarn1(&yruns);
    }
    let count = |o: &str| runs.iter().filter(|r| r["outcome"] == o).count();
    // A run of the install family that broke stops with the repair command
    // (`repair: stopped`); `npm ci` is rebuilt by plain npm ci.
    let stopped = runs.iter().filter(|r| r["outcome"] == "broke" && r["repair"] == "stopped").count();
    println!(
        "runs {}: installed {}, plain npm {}, failed then npm {}{}{}{}{}",
        runs.len(),
        count("installed"),
        count("declined"),
        count("broke") - stopped,
        if stopped > 0 { format!(", failed and stopped {stopped}") } else { String::new() },
        // npm itself ran and exited non-zero (the inject engine).
        if count("npm_failed") > 0 { format!(", npm failed {}", count("npm_failed")) } else { String::new() },
        // A root or dependency script failed in the classic engine's own
        // post-install step: older logs only (removed in PR C2).
        if count("script_failed") > 0 { format!(", script failed (classic engine) {}", count("script_failed")) } else { String::new() },
        // Stopped by SIGTERM, SIGHUP or SIGINT while npm ran (src/signals.rs, #64).
        if count("interrupted") > 0 { format!(", interrupted {}", count("interrupted")) } else { String::new() }
    );
    // Which engine each run went to: the inject engine logs `"engine":
    // "inject"`; runs without an engine are the classic engine's (the
    // default before PR C1, removed in PR C2), from older logs.
    if !runs.is_empty() {
        let n_inject = runs.iter().filter(|r| r["engine"] == "inject").count();
        println!("engines: inject {n_inject}, classic (removed in PR C2; older runs) {}", runs.len() - n_inject);
        // npm's command; records without one are npm ci's (before the
        // install family was routed).
        let mut by_cmd: std::collections::BTreeMap<&str, usize> = Default::default();
        for r in &runs {
            *by_cmd.entry(r["command"].as_str().unwrap_or("ci")).or_default() += 1;
        }
        if by_cmd.keys().any(|c| *c != "ci") {
            let list: Vec<String> = by_cmd.iter().map(|(c, n)| format!("{c} {n}")).collect();
            println!("commands: {}", list.join(", "));
        }
    }
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
    // call it out on its own line, with the versions seen. The inject
    // engine's gate (npm majors), or in older logs the classic engine's
    // (its list of verified minors).
    let mut gated: std::collections::BTreeMap<String, usize> = Default::default();
    for r in runs.iter().filter(|r| r["outcome"] == "declined") {
        if r["reason"]
            .as_str()
            .is_some_and(|w| w.contains("has not been verified against npm ci") || w.contains(": the inject engine (the default) runs on npm"))
        {
            *gated.entry(r["npm"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        }
    }
    if !gated.is_empty() {
        let list: Vec<String> = gated.iter().map(|(v, n)| format!("npm {v}: {n}")).collect();
        println!(
            "npm version gate: {} runs went to plain npm ({}); run scripts/verify-npm.sh with that npm on PATH (both engines' harness)",
            gated.values().sum::<usize>(),
            list.join(", ")
        );
    }
    // The fingerprint gate (src/inject.js): an npm whose pacote code this
    // build was not tested with ran as plain npm. Grouped by npm version and
    // fingerprint (its first 16 digits), from the reason the hook gave.
    let mut unknown: std::collections::BTreeMap<String, usize> = Default::default();
    for r in runs.iter().filter(|r| r["outcome"] == "declined") {
        let Some(why) = r["reason"].as_str().filter(|w| w.contains("has pacote code this matchapack was not tested with")) else { continue };
        let fp: String = why.rsplit("fingerprint ").next().unwrap_or("?").chars().take(16).collect();
        *unknown.entry(format!("npm {} {fp}…", r["npm"].as_str().unwrap_or("?"))).or_default() += 1;
    }
    if !unknown.is_empty() {
        let list: Vec<String> = unknown.iter().map(|(v, n)| format!("{v}: {n}")).collect();
        println!(
            "npm fingerprint gate: {} runs went to plain npm, pacote code not tested with this build ({}); update matchapack, or run scripts/test-inject.sh and verify-npm.sh with that npm, add its fingerprint to src/npm-fingerprints.json and rebuild matchapack (the list is compiled in)",
            unknown.values().sum::<usize>(),
            list.join(", ")
        );
    }
    // The trap on the hook's result (src/inject.js `resultTrap`): npm read a
    // key of it other than resolved / integrity / from (inject.rs
    // `trap_fired`). npm ci: plain npm ci rebuilt node_modules; the install
    // family: the run stopped and said how to rebuild.
    for (stop, what) in [(false, "npm ci rebuilt node_modules"), (true, "the run stopped with the repair command (install, update, uninstall)")] {
        let trapped: Vec<&str> = runs
            .iter()
            .filter(|r| r["outcome"] == "broke" && (r["repair"] == "stopped") == stop)
            .filter_map(|r| r["reason"].as_str())
            .filter(|w| w.contains("of what matchapack's hook returned"))
            .collect();
        if !trapped.is_empty() {
            println!(
                "inject engine: in {} runs npm read an unexpected key of the hook's result, and {what}; first: {}",
                trapped.len(),
                trapped[0].chars().take(120).collect::<String>()
            );
        }
    }
    // pacote's extraction held a path older than the install, or something
    // other than files and directories (serve.rs, inject.rs
    // `extraction_outcome`): broken runs, which npm ci rebuilt or the
    // install family stopped (`repair`; something had been cloned), and
    // installed or npm-failed runs that had cloned nothing (npm's own tree;
    // the run log has `inject.extraction_unexpected`). Either way the
    // extraction code was declined in that store, when known.
    let broke: Vec<&Value> = runs
        .iter()
        .filter(|r| r["outcome"] == "broke")
        .filter(|r| r["reason"].as_str().is_some_and(|w| w.contains("not what matchapack's clones reproduce")))
        .collect();
    let stopped = broke.iter().filter(|r| r["repair"] == "stopped").count();
    // A broken run's record carries `inject` too (npm's settings for the
    // repair advice): counted once, above.
    let kept: Vec<String> = runs
        .iter()
        .filter(|r| r["outcome"] != "broke")
        .filter_map(|r| r["inject"]["extraction_unexpected"].as_str())
        .map(String::from)
        .collect();
    if !broke.is_empty() || !kept.is_empty() {
        let first = broke.first().and_then(|r| r["reason"].as_str()).map(String::from).or(kept.first().cloned()).unwrap();
        println!(
            "inject engine: in {} runs pacote's extraction was not what the clones reproduce ({} rebuilt by npm ci, {} stopped with the repair advice, {} had cloned nothing and kept npm's own tree); that extraction code is left to npm in the store; first: {}",
            broke.len() + kept.len(),
            broke.len() - stopped,
            stopped,
            kept.len(),
            first.chars().take(160).collect::<String>()
        );
    }
    // One line per matchapack build, in order of first use, so numbers from
    // different binaries are never read as one series.
    let mut builds: Vec<(String, usize, Vec<f64>)> = vec![];
    for r in &runs {
        let b = r["build"].as_str().unwrap_or("(no build recorded)").to_string();
        let i = match builds.iter().position(|(k, _, _)| *k == b) {
            Some(i) => i,
            None => {
                builds.push((b, 0, vec![]));
                builds.len() - 1
            }
        };
        builds[i].1 += 1;
        // npm ci runs only: a no-op `npm install` would pull the median down.
        if let Some(s) = install_secs(std::iter::once(r), true).first() {
            builds[i].2.push(*s);
        }
    }
    if builds.len() > 1 || builds.first().is_some_and(|(k, _, _)| k.starts_with('(')) {
        println!("by build:");
        for (b, n, mut secs) in builds {
            secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = if secs.is_empty() { "-".to_string() } else { format!("{:.1} s", secs[secs.len() / 2]) };
            println!("  {b:<22} runs {n:>4}, npm ci installed {:>4}, median npm ci {med}", secs.len());
        }
    }
    // The inject engine: how much of what npm installed came from the store,
    // and why the rest did not. A hook that stops firing shows here as
    // installs with nothing cloned.
    let inj: Vec<&Value> = runs.iter().filter(|r| r["engine"] == "inject" && r["inject"].is_object()).collect();
    if !inj.is_empty() {
        let sum = |k: &str| inj.iter().filter_map(|r| r["inject"][k].as_u64()).sum::<u64>();
        let mut why: std::collections::BTreeMap<String, u64> = Default::default();
        for r in &inj {
            if let Some(m) = r["inject"]["fallbacks"].as_object() {
                for (k, n) in m {
                    *why.entry(k.clone()).or_default() += n.as_u64().unwrap_or(0);
                }
            }
        }
        let mut why: Vec<(String, u64)> = why.into_iter().collect();
        why.sort_by(|a, b| b.1.cmp(&a.1));
        let by_npm: u64 = why.iter().map(|(_, n)| n).sum();
        let list: Vec<String> = why.iter().take(6).map(|(k, n)| format!("{k} {n}")).collect();
        println!(
            "inject engine: {} runs, {} packages cloned, {} extracted by npm{}, {} new in the store",
            inj.len(),
            sum("cloned"),
            by_npm,
            if list.is_empty() { String::new() } else { format!(" ({})", list.join(", ")) },
            sum("filled"),
        );
        // The tar code each run loaded (its fingerprint is part of the store
        // key: a new one means every entry is compared once under it). No
        // list, no gate: said so that a new tar shows the day it is used.
        let mut tars: Vec<(String, String, usize)> = vec![];
        for r in &inj {
            let Some(fp) = r["inject"]["tar_fingerprint"].as_str() else { continue };
            let fp: String = fp.chars().take(16).collect();
            let ver = r["inject"]["tar"].as_str().unwrap_or("?").to_string();
            match tars.iter_mut().find(|(f, v, _)| *f == fp && *v == ver) {
                Some(t) => t.2 += 1,
                None => tars.push((fp, ver, 1)),
            }
        }
        if !tars.is_empty() {
            let list: Vec<String> = tars.iter().map(|(f, v, n)| format!("{f}… (tar {v}) {n} runs")).collect();
            println!("inject engine: tar code loaded, by fingerprint: {}", list.join(", "));
        }
        // Store entries compared with npm's own extraction before use (serve.rs),
        // and the ones that differed, by who had made them.
        let mut differs: std::collections::BTreeMap<String, (u64, String)> = Default::default();
        for r in &inj {
            if let Some(m) = r["inject"]["differs"].as_object() {
                for (k, v) in m {
                    let e = differs.entry(k.clone()).or_default();
                    e.0 += v["n"].as_u64().unwrap_or(0);
                    if e.1.is_empty() {
                        e.1 = v["first"][0].as_str().unwrap_or("").to_string();
                    }
                }
            }
        }
        let listed: Vec<String> = differs.iter().map(|(k, (n, _))| format!("{k} {n}")).collect();
        println!(
            "inject engine: {} store entries compared with npm's extraction, {}",
            sum("checked"),
            if listed.is_empty() { "none differed".to_string() } else { format!("differed and replaced: {}", listed.join(", ")) }
        );
        for (kind, what) in [
            ("extractor", "made by the classic engine's extractor (expected for odd tarballs)"),
            ("changed", "changed after npm's pacote had made them: look at what writes into the store"),
            ("pacote", "made under other extraction code (pacote, tar, libuv), which extracted differently"),
        ] {
            if let Some((n, first)) = differs.get(kind) {
                println!("inject engine: {n} differing entries {what}; first: {first}");
            }
        }
        // Reads of resolved / integrity / from of the hook's result: harmless
        // (the hook gives what pacote does), but npm does not read them
        // today, so a new one is worth a look.
        let mut reads: std::collections::BTreeMap<String, u64> = Default::default();
        for r in &inj {
            if let Some(m) = r["inject"]["result_reads"].as_object() {
                for (k, n) in m {
                    *reads.entry(k.clone()).or_default() += n.as_u64().unwrap_or(0);
                }
            }
        }
        if !reads.is_empty() {
            let list: Vec<String> = reads.iter().map(|(k, n)| format!("{k} {n}")).collect();
            println!("inject engine: npm read the hook's result ({}): expected keys, but not read by npm so far", list.join(", "));
        }
        // npm placed packages the hook never saw: the patched method is no
        // longer (the only) way this npm extracts. Installs stay correct;
        // this line is the only sign.
        let blind: Vec<&&Value> = inj.iter().filter(|r| r["inject"]["unseen"].as_u64().unwrap_or(0) > 0).collect();
        if !blind.is_empty() {
            let mut versions: Vec<&str> = blind.iter().filter_map(|r| r["npm"].as_str()).collect();
            versions.sort();
            versions.dedup();
            println!(
                "inject engine: in {} runs npm placed {} packages the hook never saw (npm {}): the hook may no longer fit this npm",
                blind.len(),
                blind.iter().filter_map(|r| r["inject"]["unseen"].as_u64()).sum::<u64>(),
                versions.join(", ")
            );
        }
    }
    let inst: Vec<&Value> = runs.iter().filter(|r| r["outcome"] == "installed").collect();
    if !inst.is_empty() {
        let logical: f64 = inst.iter().filter_map(|r| r["bytes_logical"].as_f64()).sum();
        let new: f64 = inst.iter().filter_map(|r| r["bytes_store_new"].as_f64()).sum();
        // Apart: an `npm install` with nothing to do takes a fraction of an
        // `npm ci`, so one median of both would describe neither.
        for (ci, what) in [(true, "npm ci installs"), (false, "npm install / update / uninstall")] {
            let secs = install_secs(inst.iter().copied(), ci);
            if !secs.is_empty() {
                println!(
                    "{what}: {} runs, median {:.1} s, max {:.1} s, total {:.0} s",
                    secs.len(),
                    secs[secs.len() / 2],
                    secs.last().copied().unwrap_or(0.0),
                    secs.iter().sum::<f64>()
                );
            }
        }
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
    // Extraction code this store leaves to npm (serve.rs, store.rs
    // `decline_key`): each with its file, and whether this build still
    // honours it (a decline another build recorded is retried).
    let declines = store.declines();
    if !declines.is_empty() {
        println!("store: {} declined extraction code (installs under it are npm's own, no clones):", declines.len());
        for d in &declines {
            println!(
                "  {}: declined {} by {}{}; {}; file {}",
                d.key,
                d.modified.map(utc).unwrap_or_else(|| "?".into()),
                d.build.as_deref().unwrap_or("a build that recorded none"),
                if d.void() { " (void for this build: the next install under it tries again)" } else { "" },
                d.why,
                d.path.display()
            );
        }
        println!("  to clear one: delete its file; a new matchapack build tries a declined code again by itself");
    }
    if let Ok(o) = std::process::Command::new("df").args(["-k", &crate::home().to_string_lossy()]).output() {
        if let Some(free) = String::from_utf8_lossy(&o.stdout).lines().nth(1).and_then(|l| l.split_whitespace().nth(3)).and_then(|f| f.parse::<f64>().ok()) {
            println!("free on the home volume now: {}", gib(free * 1024.0));
        }
    }
    println!("recent:");
    for r in all.iter().rev().take(6) {
        let extra = match r["outcome"].as_str() {
            Some("installed") => {
                let mut s = format!("{:.1} s, {} packages", r["secs"].as_f64().unwrap_or(0.0), r["packages"]);
                if let Some(p) = r["phases"].as_object() {
                    let mut v: Vec<(&String, f64)> = p.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k, f))).collect();
                    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                    let top: Vec<String> = v.iter().take(3).map(|(k, f)| format!("{k} {f:.1}")).collect();
                    s += &format!("; slowest: {}", top.join(", "));
                }
                if r["engine"] == "inject" {
                    let by_npm: u64 = r["inject"]["fallbacks"].as_object().map(|m| m.values().filter_map(|n| n.as_u64()).sum()).unwrap_or(0);
                    s = format!("inject, {:.1} s, {} cloned, {by_npm} extracted by npm", r["secs"].as_f64().unwrap_or(0.0), r["inject"]["cloned"]);
                }
                if r["engine"] == "yarn1" {
                    s = format!(
                        "yarn, {:.1} s, {} files cloned, {} copied by Node",
                        r["secs"].as_f64().unwrap_or(0.0),
                        r["yarn1"]["cloned"],
                        r["yarn1"]["copied_total"]
                    );
                }
                s
            }
            _ => r["reason"].as_str().unwrap_or("").chars().take(70).collect(),
        };
        let extra = match load_note(r) {
            Some(l) => format!("{extra}; {l}"),
            None => extra,
        };
        println!("  {}  {:<13} {}  ({extra})", when(r["ts"].as_f64().unwrap_or(0.0)), r["outcome"].as_str().unwrap_or("?"), r["dir"].as_str().unwrap_or("?"));
    }
    let (ver, bad) = read_jsonl(&verify_log_path(), since);
    skipped(bad, &verify_log_path());
    for l in verify_lines(&ver, !yruns.is_empty(), &when) {
        println!("{l}");
    }
    Ok(())
}

/// The report's lines on the sampled verifications (scripts/verify-sample.sh):
/// npm's and Yarn's side by side, each with its counts by result and its
/// five most recent samples (`at` formats a time), each naming the command
/// replayed and its kind: a full install (`npm ci`, `yarn install`) or a
/// command that started with no node_modules ("started empty", #77 part
/// 1b). Yarn's part shows when it has samples, or when the log has
/// `matchapack yarn` runs to sample.
fn verify_lines(ver: &[Value], yarn_runs: bool, at: &dyn Fn(f64) -> String) -> Vec<String> {
    let (yarn, npm): (Vec<&Value>, Vec<&Value>) = ver.iter().partition(|v| v["engine"] == "yarn1");
    let mut out = vec![];
    for (name, list, how) in [("npm", &npm, "scripts/verify-sample.sh"), ("yarn", &yarn, "scripts/verify-sample.sh --yarn")] {
        if list.is_empty() {
            if name == "npm" || yarn_runs {
                out.push(format!("verification samples ({name}): none yet ({how})"));
            }
            continue;
        }
        let count = |r: &str| list.iter().filter(|v| v["result"] == r).count();
        let known = ["IDENTICAL", "DIFFERENT", "BUILD-OUTPUT-ONLY", "RACE-SHAPED", "ERROR", "SKIPPED"];
        let mut parts = vec![format!("identical {}", count("IDENTICAL")), format!("different {}", count("DIFFERENT"))];
        for (r, label) in [("BUILD-OUTPUT-ONLY", "build output only"), ("RACE-SHAPED", "race-shaped (sample again)")] {
            if count(r) > 0 || (r == "RACE-SHAPED" && name == "yarn") {
                parts.push(format!("{label} {}", count(r)));
            }
        }
        parts.push(format!("error {}", count("ERROR")));
        parts.push(format!("skipped {}", count("SKIPPED")));
        let other = list.iter().filter(|v| !known.iter().any(|k| v["result"] == *k)).count();
        if other > 0 {
            parts.push(format!("other {other}"));
        }
        // Samples, not distinct runs: a logged run is sampled again on every
        // run of the sampler until a newer one replaces it.
        out.push(format!("verification samples ({name}): {} samples: {}", list.len(), parts.join(", ")));
        for v in list.iter().rev().take(5) {
            let result = v["result"].as_str().unwrap_or("?");
            // npm records from before samples named their command are npm ci's.
            let command = v["command"].as_str().unwrap_or(if name == "npm" { "ci" } else { "install" });
            let mut what = match v["kind"].as_str() {
                Some("started-empty") => format!("{name} {command}, started empty"),
                _ => format!("{name} {command}"),
            };
            if v["replayed_fresh"] == true {
                what += ", replayed fresh";
            }
            if let Some(first) = v["first_result"].as_str() {
                what += &format!(", {first} at first, then with one pool thread");
            }
            let detail = if result == "SKIPPED" {
                v["reason"].as_str().unwrap_or("?").chars().take(110).collect::<String>()
            } else {
                let mut d = if name == "yarn" {
                    format!(
                        "yarn {}, node {}, yarn {} s, matchapack {} s, {} cloned, build {}",
                        v["yarn"].as_str().unwrap_or("?"),
                        v["node"].as_str().unwrap_or("?"),
                        v["yarn_secs"],
                        v["matchapack_secs"],
                        v["cloned"],
                        v["build"].as_str().unwrap_or("not recorded")
                    )
                } else {
                    format!("npm {} s, matchapack {} s, build {}", v["npm_ci_secs"], v["matchapack_secs"], v["build"].as_str().unwrap_or("not recorded"))
                };
                if let Some(first) = v["differing"].as_array().and_then(|a| a.first()).and_then(|x| x.as_str()) {
                    d += &format!("; {} differ, first: {first}", v["differing_count"].as_u64().unwrap_or(v["differing"].as_array().map_or(0, |a| a.len() as u64)));
                } else if result != "IDENTICAL" {
                    if let Some(why) = v["reason"].as_str() {
                        d += &format!("; {}", why.chars().take(110).collect::<String>());
                    }
                }
                d
            };
            out.push(format!("  {}  {:<17} {}  ({what}; {detail})", at(v["ts"].as_f64().unwrap_or(0.0)), result, v["dir"].as_str().unwrap_or("?")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn verification_samples_of_both_engines() {
        let at = |t: f64| format!("t{t}");
        assert_eq!(verify_lines(&[], false, &at), ["verification samples (npm): none yet (scripts/verify-sample.sh)"]);
        assert_eq!(verify_lines(&[], true, &at)[1], "verification samples (yarn): none yet (scripts/verify-sample.sh --yarn)");
        let ver = vec![
            // An npm sample from before samples named their engine and command.
            json!({ "ts": 1.0, "result": "IDENTICAL", "dir": "/p/npm", "npm_ci_secs": 2.5, "matchapack_secs": 1.5, "build": "b1" }),
            json!({ "ts": 1.5, "engine": "npm", "kind": "started-empty", "command": "install", "result": "DIFFERENT", "dir": "/p/n2", "npm_ci_secs": 3.0, "matchapack_secs": 2.0, "build": "b2", "differing": ["bytes package-lock.json"] }),
            json!({ "ts": 2.0, "engine": "yarn1", "kind": "full", "command": "install", "result": "IDENTICAL", "dir": "/p/y", "yarn": "1.22.22", "node": "v24.22.1", "yarn_secs": 3.1, "matchapack_secs": 3.0, "cloned": 190, "build": "b2", "replayed_fresh": true, "first_result": "RACE-SHAPED" }),
            json!({ "ts": 3.0, "engine": "yarn1", "kind": "full", "command": "install", "result": "SKIPPED", "dir": "/p/gone", "reason": "the project directory is gone" }),
            json!({ "ts": 4.0, "engine": "yarn1", "kind": "started-empty", "command": "add", "result": "DIFFERENT", "dir": "/p/y", "yarn": "1.22.22", "node": "v24.22.1", "yarn_secs": 3.1, "matchapack_secs": 3.0, "cloned": 190, "build": "b2", "differing": ["content node_modules/y1-a/index.js"], "differing_count": 1 }),
            json!({ "ts": 5.0, "engine": "yarn1", "result": "ERROR", "dir": "/p/e", "yarn": "1.22.22", "node": "v24.22.1", "yarn_secs": null, "matchapack_secs": null, "cloned": null, "build": "b2", "reason": "plain Yarn failed in its copy" }),
        ];
        let l = verify_lines(&ver, false, &at);
        assert_eq!(l[0], "verification samples (npm): 2 samples: identical 1, different 1, error 0, skipped 0");
        assert_eq!(l[1], "  t1.5  DIFFERENT         /p/n2  (npm install, started empty; npm 3.0 s, matchapack 2.0 s, build b2; 1 differ, first: bytes package-lock.json)");
        assert_eq!(l[2], "  t1  IDENTICAL         /p/npm  (npm ci; npm 2.5 s, matchapack 1.5 s, build b1)");
        assert_eq!(l[3], "verification samples (yarn): 4 samples: identical 1, different 1, race-shaped (sample again) 0, error 1, skipped 1");
        assert_eq!(l[4], "  t5  ERROR             /p/e  (yarn install; yarn 1.22.22, node v24.22.1, yarn null s, matchapack null s, null cloned, build b2; plain Yarn failed in its copy)");
        assert_eq!(l[5], "  t4  DIFFERENT         /p/y  (yarn add, started empty; yarn 1.22.22, node v24.22.1, yarn 3.1 s, matchapack 3.0 s, 190 cloned, build b2; 1 differ, first: content node_modules/y1-a/index.js)");
        assert_eq!(l[6], "  t3  SKIPPED           /p/gone  (yarn install; the project directory is gone)");
        assert_eq!(l[7], "  t2  IDENTICAL         /p/y  (yarn install, replayed fresh, RACE-SHAPED at first, then with one pool thread; yarn 1.22.22, node v24.22.1, yarn 3.1 s, matchapack 3.0 s, 190 cloned, build b2)");
        assert_eq!(l.len(), 8);
    }

    #[test]
    fn utc_dates() {
        let t = |secs: u64| super::utc(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
        assert_eq!(t(0), "1970-01-01 00:00 UTC");
        // npm's tarball time, 1985-10-26 08:15 UTC.
        assert_eq!(t(499_162_500), "1985-10-26 08:15 UTC");
        assert_eq!(t(1_709_208_000), "2024-02-29 12:00 UTC");
    }

    use super::*;
    use crate::store::tests::scratch;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    /// A record about the size of a real one (500–2000 bytes, many keys:
    /// serde_json writes each token on its own).
    fn sized_record(writer: usize, seq: usize) -> Value {
        let phases: serde_json::Map<String, Value> = (0..12 + seq % 20).map(|i| (format!("phase-{i}"), json!(i as f64 * 0.37))).collect();
        json!({ "outcome": "installed", "writer": writer, "seq": seq, "dir": format!("/Users/someone/git/project-{writer}"),
                "args": ["ci", "--no-audit"], "phases": phases, "reason": "x".repeat(seq * 37 % 900) })
    }

    /// Run as a child process by `concurrent_appends_stay_whole_lines`
    /// (nothing to do otherwise): wait for the start file, then append.
    #[test]
    fn append_child() {
        let Ok(log) = std::env::var("MATCHAPACK_TEST_APPEND_TO") else { return };
        let writer: usize = std::env::var("MATCHAPACK_TEST_WRITER").unwrap().parse().unwrap();
        let go = Path::new(&log).with_extension("go");
        while !go.exists() {
            std::thread::yield_now();
        }
        for seq in 0..150 {
            append_to(Path::new(&log), sized_record(writer, seq));
        }
    }

    /// Installs that end together (separate processes) append whole lines,
    /// none lost. Before, `writeln!` interleaved two records mid-line.
    #[test]
    fn concurrent_appends_stay_whole_lines() {
        let log = scratch("trial-concurrent").join("log.jsonl");
        let exe = std::env::current_exe().unwrap();
        let writers = 12;
        let children: Vec<_> = (0..writers)
            .map(|w| {
                std::process::Command::new(&exe)
                    .args(["--exact", "trial::tests::append_child", "--test-threads=1", "-q"])
                    .env("MATCHAPACK_TEST_APPEND_TO", &log)
                    .env("MATCHAPACK_TEST_WRITER", w.to_string())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        std::fs::write(log.with_extension("go"), "").unwrap();
        for mut c in children {
            assert!(c.wait().unwrap().success());
        }
        let text = std::fs::read_to_string(&log).unwrap();
        let mut seen = std::collections::HashSet::new();
        for (i, l) in text.lines().enumerate() {
            let r: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("line {} is not one record ({e}): {:.200}", i + 1, l));
            assert!(seen.insert((r["writer"].as_u64().unwrap(), r["seq"].as_u64().unwrap())), "line {} written twice", i + 1);
        }
        assert_eq!(seen.len(), writers * 150, "every record once");
        let (recs, bad) = read_jsonl(&log, 0.0);
        assert_eq!((recs.len(), bad), (writers * 150, 0));
    }

    #[test]
    fn unreadable_lines_are_counted_and_skipped() {
        let log = scratch("trial-unreadable").join("log.jsonl");
        std::fs::write(&log, "{\"ts\":5,\"outcome\":\"installed\"}\n{\"outcome\":\"installed\"{,\"\"outcome\"\n\n{\"ts\":9}\n").unwrap();
        let (recs, bad) = read_jsonl(&log, 0.0);
        assert_eq!((recs.len(), bad), (2, 1));
        assert_eq!(read_jsonl(&log, 6.0).0.len(), 1, "the window still applies");
    }

    #[test]
    fn install_medians_keep_npm_ci_apart_from_the_install_family() {
        let runs = [
            json!({ "outcome": "installed", "secs": 30.0 }),
            json!({ "outcome": "installed", "command": "ci", "secs": 20.0 }),
            json!({ "outcome": "installed", "command": "install", "secs": 0.5 }),
            json!({ "outcome": "installed", "command": "uninstall", "secs": 0.4 }),
            json!({ "outcome": "declined", "command": "ci", "secs": 1.0 }),
        ];
        assert_eq!(super::install_secs(runs.iter(), true), [20.0, 30.0], "npm ci, and records from before the family was routed");
        assert_eq!(super::install_secs(runs.iter(), false), [0.4, 0.5]);
    }

    #[test]
    fn records_carry_the_load_average() {
        let l = loadavg().expect("getloadavg answers on macOS and Linux");
        assert!(l.iter().all(|x| *x >= 0.0 && ((x * 100.0).round() - x * 100.0).abs() < 1e-6), "two decimals: {l:?}");
        let log = scratch("trial-load").join("log.jsonl");
        append_to(&log, json!({ "outcome": "installed", "load_start": [3.14, 2.0] }));
        let (recs, _) = read_jsonl(&log, 0.0);
        let r = &recs[0];
        assert!(r["load_end"].as_array().is_some_and(|a| a.len() == 2 && a.iter().all(|x| x.is_f64())), "{r}");
        let end = r["load_end"][0].as_f64().unwrap();
        assert_eq!(load_note(r), Some(format!("load 3.1→{end:.1}")));
        assert_eq!(load_note(&json!({ "outcome": "installed" })), None, "an older record: no load note");
    }

    #[test]
    fn a_relative_scope_entry_never_matches() {
        let d = crate::store::tests::scratch("trial-scope");
        let app = d.join("app/x");
        std::fs::create_dir_all(&app).unwrap();
        let home = d.join("home");
        let v = |e: &[&str]| e.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // From the environment and from a paths file alike: `.` and a
        // relative path are rejected, also where the current directory or a
        // directory of that name would cover `dir`.
        let file = paths_file_entries("# scope\n.\n  app/x  \n");
        assert_eq!(file, v(&[".", "app/x"]));
        let cwd = std::env::current_dir().unwrap();
        for (list, source) in [(v(&["."]), "MATCHAPACK_PATHS"), (v(&["app/x"]), "MATCHAPACK_PATHS"), (file.clone(), "~/.config/matchapack/paths")] {
            for dir in [&app, &cwd] {
                assert_eq!(in_scope_of(dir, &list, source, &home), Err(format!("outside the trial scope: {source} lists no entry that can match")), "{list:?} {}", dir.display());
            }
        }
        // Next to a good entry: said by name, never matched.
        let good = d.join("elsewhere").display().to_string();
        let e = in_scope_of(&app, &v(&[&good, "."]), "MATCHAPACK_PATHS", &home).unwrap_err();
        assert_eq!(e, "outside the trial scope: MATCHAPACK_PATHS lists \".\", which can never match (not an absolute path)");
        // An absolute entry still matches, and `~/` is the home directory.
        assert_eq!(in_scope_of(&app, &v(&[&d.join("app").display().to_string()]), "MATCHAPACK_PATHS", &home), Ok(()));
        std::fs::create_dir_all(home.join("w")).unwrap();
        assert_eq!(in_scope_of(&home.join("w"), &v(&["~/w"]), "MATCHAPACK_PATHS", &home), Ok(()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn credentials_never_reach_the_log() {
        assert_eq!(
            redact_args(&v(&["install", "--proxy", "http://u:p@proxy:8080", "--https-proxy=https://u%40x:p@h/", "--otp", "123456", "--cache-folder", "/c"])),
            v(&["install", "--proxy", "http://***@proxy:8080", "--https-proxy=https://***@h/", "--otp", "***", "--cache-folder", "/c"])
        );
        assert_eq!(redact_args(&v(&["ci", "--_auth=dXNlcjpwYXNz", "--//r.example/:_authToken=abc", "--no-audit"])), v(&["ci", "--_auth=***", "--//r.example/:_authToken=***", "--no-audit"]));
        // A secret option followed by another option: nothing hidden.
        assert_eq!(redact_args(&v(&["--otp", "--frozen-lockfile"])), v(&["--otp", "--frozen-lockfile"]));
        assert_eq!(redact_userinfo("option --registry=https://me:pw@r.example/x is not handled"), "option --registry=https://***@r.example/x is not handled");
        assert_eq!(redact_userinfo("http://host:80/a@b"), "http://host:80/a@b", "an @ after the host is not userinfo");
        // npm's client key; a secret in a query; an ssh user name is not one.
        assert_eq!(
            redact_args(&v(&["install", "--key=-----BEGIN PRIVATE KEY-----x", "--cert=-----BEGIN CERTIFICATE-----y", "--//r.example/:key=z", "--keyfile=/k"])),
            v(&["install", "--key=***", "--cert=-----BEGIN CERTIFICATE-----y", "--//r.example/:key=***", "--keyfile=/k"])
        );
        assert_eq!(
            redact_args(&v(&["install", "https://gitlab.example/api/x.tgz?private_token=abc&ref=main", "--registry=https://r.example/?access_token=t#f"])),
            v(&["install", "https://gitlab.example/api/x.tgz?private_token=***&ref=main", "--registry=https://r.example/?access_token=***#f"])
        );
        assert_eq!(redact_args(&v(&["install", "git+ssh://git@github.com/o/r.git", "ssh://u:pw@h/r"])), v(&["install", "git+ssh://git@github.com/o/r.git", "ssh://***@h/r"]));
        // A PEM key as a word of its own (it starts with `-`).
        assert_eq!(
            redact_args(&v(&["install", "--key", "-----BEGIN PRIVATE KEY-----\nMIIx\n-----END PRIVATE KEY-----", "--cert", "-----BEGIN CERTIFICATE-----y", "x", "--x=-----BEGIN EC KEY-----z"])),
            v(&["install", "--key", "***", "--cert", "-----BEGIN CERTIFICATE-----y", "x", "--x=***"])
        );
        // With base64 padding (`=`), on several lines and on one, as a word
        // of its own and after `--key=`; a certificate stays.
        assert_eq!(
            redact_args(&v(&[
                "install",
                "--key",
                "-----BEGIN PRIVATE KEY-----\nFAKEBODYFAKEBODYFA==\n-----END PRIVATE KEY-----",
                "--key",
                "-----BEGIN RSA PRIVATE KEY-----FAKE=-----END RSA PRIVATE KEY-----",
                "--key=-----BEGIN PRIVATE KEY-----\nFAKE==\n-----END PRIVATE KEY-----",
                "--cert=-----BEGIN CERTIFICATE-----\nFAKE==\n-----END CERTIFICATE-----"
            ])),
            v(&["install", "--key", "***", "--key", "***", "--key=***", "--cert=-----BEGIN CERTIFICATE-----\nFAKE==\n-----END CERTIFICATE-----"])
        );
        // Every field with command-line words: the record's `args`, and
        // every list under `inject.repair` (`args`, `missing`, a later one).
        let secrets = ["--otp=123456", "--//r.example/:_authToken=FAKETOKEN123", "--_password=hunter2", "--audit=false"];
        let hidden = json!(["--otp=***", "--//r.example/:_authToken=***", "--_password=***", "--audit=false"]);
        let mut rec = json!({ "args": secrets, "inject": { "repair": { "args": secrets, "missing": secrets, "later": secrets, "npmrc": ["project"], "checked": false } } });
        redact(&mut rec);
        assert_eq!(rec["args"], hidden);
        for k in ["args", "missing", "later"] {
            assert_eq!(rec["inject"]["repair"][k], hidden, "{k}");
        }
        assert_eq!(rec["inject"]["repair"]["npmrc"], json!(["project"]));
        assert!(!rec.to_string().contains("123456") && !rec.to_string().contains("FAKETOKEN") && !rec.to_string().contains("hunter2"), "{rec}");
        let mut rec = json!({ "args": ["--proxy", "http://u:p@h"], "reason": "x http://a:b@h y", "yarn1": { "errors": ["https://c:d@e"] } });
        redact(&mut rec);
        assert_eq!(rec, json!({ "args": ["--proxy", "http://***@h"], "reason": "x http://***@h y", "yarn1": { "errors": ["https://***@e"] } }));
        // The options the repair advice keeps (src/inject.js repairInfo, one
        // token each), in the log's inject record: redacted as args are.
        let mut rec = json!({ "inject": { "repair": { "args": ["--_password=-abcSECRET", "--//r.example/:_authToken=t0k", "--omit=dev"], "npmrc": [] } } });
        redact(&mut rec);
        assert_eq!(rec["inject"]["repair"]["args"], json!(["--_password=***", "--//r.example/:_authToken=***", "--omit=dev"]));
        // A Yarn install's replay arguments (src/yarn1.rs `replay`), nested
        // under `replay.args`: redacted as args are.
        let mut rec = json!({ "engine": "yarn1", "replay": { "args": ["--otp", "123456", "--proxy", "http://u:p@h", "--frozen-lockfile"] } });
        redact(&mut rec);
        assert_eq!(rec["replay"]["args"], json!(["--otp", "***", "--proxy", "http://***@h", "--frozen-lockfile"]));
    }
}
