//! `matchapack npm <args>`: what the shell wrapper calls instead of npm.
//!
//! Candidates are `npm ci`, `npm install`, `npm update` and `npm uninstall`
//! under the first words of `ROUTED` (their aliases). Everything else, and
//! every candidate that fails a check, runs the real npm with the original
//! arguments, so the worst case is plain npm.
//!
//! The inject engine (`run_inject`) takes them with any arguments and
//! declines before npm starts only what needs no node (`decide_inject`,
//! `inject::gates`: npm major 10–12, umask 022, the exact flags that make it
//! global or install nothing, a start directory outside the trial scope).
//! The gates that depend on where npm installs (the trial scope, a symlinked
//! node_modules, a global install set in an npmrc) run on npm's own install
//! root when npm reports it (serve.rs `accept_root`).

use crate::{trial, util, InstallError};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The first words the shell function sends to `matchapack npm` (shell/
/// matchapack.sh and .fish list the same words; a test compares them), with
/// the npm command each names: npm's own aliases and typos
/// (`lib/utils/cmd-list.js`, the same in npm 10.0.0 to 12.2.1, except `u`,
/// an alias of `update` from npm 11; on npm 10 `npm u` is no command, and
/// the root check declines it before it runs). Abbreviations npm also
/// accepts (`npm upd`) are not routed: plain npm. Not routed although the
/// engine would handle them: install-test, install-ci-test, dedupe, prune,
/// `audit fix`, link, exec (the owner's decision, 2026-11-15).
pub const ROUTED: &[(&str, &str)] = &[
    ("ci", "ci"),
    ("clean-install", "ci"),
    ("ic", "ci"),
    ("install-clean", "ci"),
    ("isntall-clean", "ci"),
    ("install", "install"),
    ("i", "install"),
    ("in", "install"),
    ("ins", "install"),
    ("inst", "install"),
    ("insta", "install"),
    ("instal", "install"),
    ("isnt", "install"),
    ("isnta", "install"),
    ("isntal", "install"),
    ("isntall", "install"),
    ("add", "install"),
    ("update", "update"),
    ("up", "update"),
    ("upgrade", "update"),
    ("udpate", "update"),
    ("u", "update"),
    ("uninstall", "uninstall"),
    ("un", "uninstall"),
    ("unlink", "uninstall"),
    ("remove", "uninstall"),
    ("rm", "uninstall"),
    ("r", "uninstall"),
];

/// The npm command a routed first word names, or None (not routed).
pub fn routed(word: &str) -> Option<&'static str> {
    ROUTED.iter().find(|(w, _)| *w == word).map(|(_, c)| *c)
}

pub fn run(args: Vec<String>) -> i32 {
    if std::env::var("MATCHAPACK").as_deref() == Ok("0") {
        return exec_npm(&args);
    }
    let Some(command) = args.first().and_then(|a| routed(a)) else {
        return exec_npm(&args);
    };
    // Before anything of the install runs (`load_end`: trial::append).
    let load_start = trial::loadavg();
    let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let npm_version = util::npm_version().unwrap_or_default();
    // `command`: npm's name for the routed word (npm's own once npm reported
    // it, `inject.command`); `root`: npm's install root, once npm reported it.
    let log = |outcome: &str, extra: Value| {
        // The npm settings from the environment, hashed (src/replay.rs): what
        // a later sample checks the replay's environment against.
        let mut rec = json!({ "outcome": outcome, "dir": dir, "npm": npm_version, "args": args, "command": command, "phases": util::phases(), "load_start": load_start, "env_settings": crate::replay::env_settings("npm") });
        if let (Value::Object(m), Value::Object(x)) = (&mut rec, extra) {
            m.extend(x);
        }
        trial::append(rec);
    };
    // MATCHAPACK_ENGINE: unset, empty or `inject`; anything else (`classic`,
    // removed before 0.1.0, included) is plain npm with the reason. Logged as
    // the inject engine's run: `report` counts runs without an engine as
    // the classic engine's, from logs written before it was removed.
    if let Err(reason) = crate::inject::engine() {
        log("declined", json!({ "engine": "inject", "reason": reason }));
        return decline(&args, command, &reason);
    }
    run_inject(&args, command, &log)
}

/// npm's command and install root as the run's `inject` record has them
/// (npm reported them), for the log's top level.
fn command_and_root(inject: Option<&Value>) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    for k in ["command", "root"] {
        if let Some(v) = inject.and_then(|i| i[k].as_str()) {
            m.insert(k.into(), v.into());
        }
    }
    m
}

/// `extra` with npm's command and root added (`command_and_root`), and
/// what a later sample needs to replay the run (src/replay.rs): the root's
/// node_modules before the run and the SHA-256 of its lockfile and manifests
/// before (taken when npm reported the root) and after it (now).
fn with_root(mut extra: Value, inject: Option<&Value>) -> Value {
    if let Value::Object(m) = &mut extra {
        m.extend(command_and_root(inject));
        m.extend(replay_state(inject));
        // Once, at the top level: not again inside the inject record.
        if let Some(Value::Object(i)) = m.get_mut("inject") {
            i.remove("before");
        }
    }
    extra
}

/// `node_modules_before`, `files_before`, `files_after` and `files_complete`
/// for the log's top level, when npm reported its root.
fn replay_state(inject: Option<&Value>) -> serde_json::Map<String, Value> {
    let (Some(root), Some(before)) = (inject.and_then(|i| i["root"].as_str()), inject.map(|i| &i["before"]).filter(|b| b.is_object())) else {
        return serde_json::Map::new();
    };
    crate::replay::record_fields(std::path::Path::new(root), crate::replay::NPM_LOCKFILES, before)
}

/// `a` as one shell word: bare only when every byte is in a set no shell
/// (zsh's EXTENDED_GLOB included: `^`, `#`, `~`; fish's `%self`) gives a
/// meaning, and it neither starts with `=` (zsh's `=cmd`) nor holds `==`
/// (zsh's MAGIC_EQUAL_SUBST expands `--opt==cmd`); single-quoted otherwise.
pub(crate) fn shell_quote(a: &str) -> String {
    if !a.is_empty() && !a.starts_with('=') && !a.contains("==") && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_=.,/:@+".contains(&b)) {
        a.to_string()
    } else {
        // A backslash and a quote go outside the quotes, escaped (`'\\'`,
        // `'\''`): inside single quotes fish reads `\\` and `\'` as
        // escapes, bash and zsh do not.
        let mut q = String::from("'");
        for c in a.chars() {
            match c {
                '\\' => q.push_str("'\\\\'"),
                '\'' => q.push_str("'\\''"),
                c => q.push(c),
            }
        }
        q.push('\'');
        q
    }
}

/// `head` and then `args`, shell-quoted.
fn shell_line(head: &str, args: &[String]) -> String {
    std::iter::once(head.to_string()).chain(args.iter().map(|a| shell_quote(a))).collect::<Vec<_>>().join(" ")
}

/// npm's settings at its root report (the run's `inject` record) that decide
/// how a broken run can be rebuilt; None: not known.
#[derive(Debug, Default, Clone)]
struct RepairSettings {
    /// `package-lock`: false, npm wrote no package-lock.json in this run.
    package_lock: Option<bool>,
    /// `save`: false (`--no-save`), npm saved neither package.json nor
    /// package-lock.json.
    save: Option<bool>,
    /// The command was filtered to some workspaces (`-w`, `--workspaces`, or
    /// started inside a member).
    workspace_filter: Option<bool>,
    /// The command's options as npm parsed them, serialised one token each
    /// (`--name=value`, `--flag`), without the workspace filter, the root
    /// (`--prefix`), the command word and package names (src/inject.js
    /// `repairInfo`; `inject.repair.args` in the log, where the log's
    /// redaction of secrets applies): what a whole-project rebuild keeps.
    /// Only when npm's own parser read them back exactly as it read the
    /// typed command line (`checked` true); None otherwise.
    flags: Option<Vec<String>>,
    /// The npmrc layers (project, user, global, builtin) that set a
    /// workspace filter: a command line cannot unset them.
    npmrc: Vec<String>,
    /// The options could not be repeated safely: npm's parser did not read
    /// them back the same, could not be asked, or npm reported none.
    unsure: bool,
    /// When unsure: the options npm parsed from the typed line that change
    /// what an install does or runs (`--ignore-scripts`, `--omit=dev`, …),
    /// for the advice to name.
    missing: Vec<String>,
}

impl RepairSettings {
    fn of(inject: &Value) -> RepairSettings {
        let b = |k: &str| inject[k].as_bool();
        let strs = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>());
        // Anything but a passed check is unsure: a failed one, one that
        // could not be asked (`null`), no report of the options at all.
        let checked = inject["repair"]["checked"] == true && inject["repair"]["args"].is_array();
        RepairSettings {
            package_lock: b("package_lock"),
            save: b("save"),
            workspace_filter: b("workspace_filter"),
            flags: if checked { strs(&inject["repair"]["args"]) } else { None },
            npmrc: strs(&inject["repair"]["npmrc"]).unwrap_or_default(),
            unsure: !checked,
            missing: strs(&inject["repair"]["missing"]).unwrap_or_default(),
        }
    }
}

/// The environment variables among `names` that set a workspace filter for
/// npm: npm reads `npm_config_<name>` with any case, `_` for `-`
/// (`NPM_CONFIG_workspace=a` filters).
fn filter_env(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            l.strip_prefix("npm_config_").is_some_and(|k| ["workspace", "workspaces", "include-workspace-root"].contains(&k.replace('_', "-").as_str()))
        })
        .cloned()
        .collect()
}

/// The package directories matchapack cloned in this run outside the root's
/// own node_modules (`inject.placed_outside`, the server's list: a
/// workspace member's `packages/b/node_modules/ms`, a package nested under a
/// `file:` dependency), as they may be removed: each one's parent, symlinks
/// resolved, inside the root (also resolved), and the directory itself a
/// real directory, not a symlink. Nothing derived from lockfile keys, and
/// never the root's node_modules: `npm ci` empties that one itself after
/// validating, and keeps its directory (and what is set on it).
struct Placed {
    dirs: Vec<std::path::PathBuf>,
    /// Listed, but behind a symlink to outside the project (or a symlink
    /// itself): left out.
    skipped: usize,
    /// Whether the run's record has the list (an older or partial record
    /// does not): when not, nothing outside the root's node_modules is
    /// removed, and the advice says so.
    known: bool,
}

fn placed_outside(root: &std::path::Path, inject: Option<&Value>) -> Placed {
    let Some(list) = inject.and_then(|i| i["placed_outside"].as_array()) else {
        return Placed { dirs: vec![], skipped: 0, known: false };
    };
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ids = inject.and_then(|i| i["placed_outside_ids"].as_object());
    let (mut dirs, mut skipped) = (vec![], 0);
    for key in list.iter().filter_map(|k| k.as_str()) {
        let parts: Vec<&str> = key.split('/').collect();
        if parts.first() == Some(&"node_modules") || parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
            continue;
        }
        let d = root.join(key);
        let parent = d.parent().and_then(|p| std::fs::canonicalize(p).ok());
        let inside = parent.as_ref().is_some_and(|p| p.starts_with(&canonical_root) && !p.starts_with(canonical_root.join("node_modules")));
        match std::fs::symlink_metadata(&d) {
            Ok(m) if inside && m.is_dir() && !m.file_type().is_symlink() => {
                // Still the directory matchapack placed (its device and
                // inode then): npm's rollback after a failed install script
                // removes the clone and puts its own directory back there.
                // A record without the id (older) is taken as it is.
                use std::os::unix::fs::MetadataExt;
                let placed = ids.and_then(|o| o.get(key)).and_then(|v| Some((v.get(0)?.as_u64()?, v.get(1)?.as_u64()?)));
                if placed.is_none_or(|id| id == (m.dev(), m.ino())) && !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
            // Gone already: nothing to remove.
            Err(_) => {}
            _ => skipped += 1,
        }
    }
    Placed { dirs, skipped, known: true }
}

/// Before the automatic plain `npm ci` after a broken `npm ci`: remove the
/// package directories matchapack cloned in this run outside the root's
/// node_modules (`placed_outside`). npm ci before 10.8.1 keeps what it finds
/// in a member's own node_modules, clones of the broken run included; it
/// empties the root's itself. Nothing without npm's root report or the list.
/// How many package directories were cleared.
fn remove_placed_outside(inject: Option<&Value>) -> usize {
    let Some(root) = inject.and_then(|i| i["root"].as_str()).map(std::path::PathBuf::from) else {
        return 0;
    };
    // The deepest first: a package nested in another placed one is cleared
    // before the one it is in.
    let mut dirs = placed_outside(&root, inject).dirs;
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    let mut n = 0;
    for d in dirs {
        let r = removal_targets(&d).iter().try_for_each(|t| match std::fs::symlink_metadata(t) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(t),
            Ok(_) => std::fs::remove_file(t),
            Err(_) => Ok(()),
        });
        match r {
            Ok(()) => {
                // Gone when nothing is left in it.
                let _ = std::fs::remove_dir(d.join("node_modules"));
                let _ = std::fs::remove_dir(&d);
                n += 1;
            }
            Err(e) => eprintln!("matchapack: could not remove {}: {e}", d.display()),
        }
    }
    n
}

/// What to remove of a package directory matchapack placed: the
/// directory, or, when it has a `node_modules` (npm moves the nested
/// packages it keeps back under a package it replaced; they are not the
/// clone's), every other entry of it, as pacote does when it extracts over a
/// directory. Each path is inside `d`.
fn removal_targets(d: &std::path::Path) -> Vec<std::path::PathBuf> {
    let nm = d.join("node_modules");
    if !std::fs::symlink_metadata(&nm).is_ok_and(|m| m.is_dir()) {
        return vec![d.to_path_buf()];
    }
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(d)
        .map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name() != Some(std::ffi::OsStr::new("node_modules"))).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// The line before the automatic plain `npm ci` after a broken one.
fn rebuild_line(error: &anyhow::Error, removed: usize) -> String {
    let after = match removed {
        0 => String::new(),
        1 => ", after removing 1 package directory matchapack placed outside node_modules".to_string(),
        n => format!(", after removing {n} package directories matchapack placed outside node_modules"),
    };
    format!("matchapack: failed while npm was running ({}); running npm ci to rebuild node_modules{after}", one_line(error))
}

/// More than this many directories to remove: the advice lists them in a
/// file instead of one line each.
const LIST_IN_FILE: usize = 20;

/// The file listing `paths` (NUL-separated, for `xargs -0`), in the run
/// log's directory; None if it could not be written.
fn write_removal_list(paths: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    write_removal_list_in(trial::log_path().parent()?, paths)
}

/// `write_removal_list` into `dir`: readable by the user only, named
/// `repair-<pid>-<secs>.list`; such lists older than a day are removed.
fn write_removal_list_in(dir: &std::path::Path, paths: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir).ok()?;
    let day = std::time::Duration::from_secs(86_400);
    for e in std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()) {
        let n = e.file_name().to_string_lossy().to_string();
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|a| a > day);
        if n.starts_with("repair-") && n.ends_with(".list") && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let f = dir.join(format!("repair-{}-{}.list", std::process::id(), trial::now() as u64));
    let mut bytes = vec![];
    for p in paths {
        bytes.extend_from_slice(p.as_os_str().as_bytes());
        bytes.push(0);
    }
    let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&f).ok()?;
    out.write_all(&bytes).ok()?;
    Some(f)
}

/// What to tell a user after a run of the install family that broke, so
/// that they can check and rebuild: one or two
/// sentences, then each command on a line of its own. By default `npm ci`
/// with the command's own arguments, which rebuilds from the lockfile npm
/// wrote (npm ci takes an install command's flags and ignores package
/// names). Not where that rebuilds something else:
/// - no package-lock.json or npm-shrinkwrap.json in `root` (npm's install
///   root), or `package-lock` false: npm ci would refuse, or rebuild from a
///   lockfile this run did not update;
/// - `save` false (`--no-save`; `npm update` does not save package.json by
///   default, so not for it): the lockfile does not have what the run added;
///   for both: remove the root's node_modules and run the same command with
///   plain npm;
/// - a command filtered to some workspaces: `npm ci -w x` removes the
///   packages of the other workspaces, so npm
///   ci of the whole project rebuilds it;
/// - both: remove the root's node_modules, reinstall the whole project,
///   then the command again (the filtered command alone would bring back
///   that workspace's packages only).
///
/// Before those, every form removes `placed`: the package directories
/// matchapack cloned in this run outside the root's node_modules (npm ci
/// before 10.8.1 keeps what it finds in a member's own), one line each, or
/// from a file (`list_file`) when there are more than `LIST_IN_FILE`. A
/// whole-project command names the root with `--prefix` (no `cd`: fish
/// rejects a subshell), keeps the options npm parsed, one token each, only
/// when npm's parser read them back the same (`RepairSettings::flags`; else
/// none, and a sentence before the commands names every option it could not
/// repeat), unsets the filter's environment variables among `env` (the
/// names set in the environment), and names the npmrc files that set one.
/// Without npm's root report (`root` None) matchapack had placed nothing:
/// the same command again with plain npm, nothing removed. Credentials in
/// the arguments are shown as `***` (the run log's redaction), and the
/// advice says so; notes about what was left out follow the commands, on
/// lines of their own.
fn repair_advice(args: &[String], command: &str, root: Option<&std::path::Path>, s: &RepairSettings, env: &[String], placed: &Placed, list_file: Option<&std::path::Path>) -> String {
    let shown_args = trial::redact_args(args);
    let again = shell_line("MATCHAPACK=0 npm", &shown_args);
    let lines = |text: String, commands: Vec<String>| {
        std::iter::once(text).chain(commands.into_iter().map(|c| format!("  {c}"))).collect::<Vec<_>>().join("\n")
    };
    let Some(root) = root else {
        let advice = lines(
            "npm had not reported its install root, so matchapack had placed nothing: run the same command again with plain npm:".into(),
            vec![again],
        );
        return with_redaction_note(advice, shown_args != args);
    };
    let lockfile = ["package-lock.json", "npm-shrinkwrap.json"].iter().any(|f| root.join(f).is_file());
    let no_lock = !lockfile || s.package_lock == Some(false);
    let no_save = s.save == Some(false) && command != "update";
    let filtered = s.workspace_filter == Some(true);
    let flags = if s.unsure { vec![] } else { s.flags.clone().unwrap_or_default() };
    let shown_flags = trial::redact_args(&flags);
    let shown_missing = trial::redact_args(&s.missing);
    let redacted = shown_args != args || shown_flags != flags || shown_missing != s.missing;
    // The package directories matchapack cloned outside the root's
    // node_modules: one `rm -rf` each, or all from a file.
    let mut commands: Vec<String> = match list_file {
        Some(f) if placed.dirs.len() > LIST_IN_FILE => vec![format!("xargs -0 rm -rf -- < {}", shell_quote(&f.display().to_string()))],
        // A directory with a `node_modules` npm kept: its other entries.
        _ => placed.dirs.iter().map(|d| shell_line("rm -rf", &removal_targets(d).iter().map(|t| t.display().to_string()).collect::<Vec<_>>())).collect(),
    };
    let remove_root = shell_line("rm -rf", &[root.join("node_modules").display().to_string()]);
    let whole = |verb: &str| {
        let unset: Vec<String> = filter_env(env).iter().map(|v| format!("-u {} ", shell_quote(v))).collect();
        let env = if unset.is_empty() { String::new() } else { format!("env {}", unset.concat()) };
        let mut words = vec!["--prefix".to_string(), root.display().to_string()];
        words.extend(shown_flags.iter().cloned());
        shell_line(&format!("{env}MATCHAPACK=0 npm {verb}"), &words)
    };
    // Said before the commands: what the whole-project command leaves out.
    let mut notes = String::new();
    if filtered && s.unsure {
        notes.push_str(" matchapack could not repeat your options safely, so the whole-project command below has none of them: add the ones you need");
        if !shown_missing.is_empty() {
            let named: Vec<String> = shown_missing.iter().map(|m| shell_quote(m)).collect();
            notes.push_str(&format!(" (you used {}: add {})", named.join(", "), if named.len() == 1 { "it" } else { "them" }));
        }
        notes.push('.');
    }
    if filtered && !s.npmrc.is_empty() {
        notes.push_str(&format!(
            " Your {} npmrc sets a workspace filter, which a command line cannot unset: take it out for that command, or npm rebuilds only those workspaces.",
            s.npmrc.join(" and ")
        ));
    }
    let first = if commands.is_empty() {
        String::new()
    } else if commands.len() == 1 && placed.dirs.len() > LIST_IN_FILE {
        format!("remove what matchapack placed in {} package directories outside node_modules (the file in the first command lists it), then ", placed.dirs.len())
    } else {
        "remove what matchapack placed outside node_modules, then ".to_string()
    };
    let why = if no_lock {
        Some("There is no package-lock.json from this run for npm ci to rebuild from")
    } else if no_save {
        Some("This run saved neither package.json nor package-lock.json (--no-save), so npm ci would rebuild without what it added")
    } else {
        None
    };
    let text = match (why, filtered) {
        (Some(why), true) => {
            commands.extend([remove_root, whole("install"), again]);
            format!("{why}, and the command was filtered to some workspaces: check `git diff` of package.json, then {first}remove node_modules, reinstall the whole project with plain npm, and run the command again.{notes}")
        }
        (Some(why), false) => {
            commands.extend([remove_root, again]);
            format!("{why}: check `git diff` of package.json, then {first}remove node_modules and run the command again with plain npm:")
        }
        (None, true) => {
            commands.push(whole("ci"));
            format!("The command was filtered to some workspaces, and npm ci with the filter removes the packages of the others: check `git diff` of package.json and package-lock.json, then {first}rebuild the whole project with plain npm.{notes}")
        }
        (None, false) => {
            commands.push(shell_line("MATCHAPACK=0 npm ci", &shown_args[1..]));
            format!("Check `git diff` of package.json and package-lock.json, then {first}rebuild with plain npm:")
        }
    };
    let mut advice = lines(text, commands);
    if placed.skipped > 0 {
        let n = placed.skipped;
        advice.push_str(&format!(
            "\nmatchapack: {} behind a symlink to outside the project {} left out: matchapack removes nothing outside the project.",
            if n == 1 { "one package directory".to_string() } else { format!("{n} package directories") },
            if n == 1 { "is" } else { "are" }
        ));
    }
    if !placed.known {
        advice.push_str("\nmatchapack: this run's record does not list what matchapack placed outside node_modules, so nothing there is removed: check a workspace member's own node_modules yourself.");
    }
    with_redaction_note(advice, redacted)
}

/// The advice, and a line saying that credentials are shown as `***` when
/// any were.
fn with_redaction_note(advice: String, redacted: bool) -> String {
    if redacted {
        format!("{advice}\nmatchapack: credentials in the command line are shown as *** above: add them back, or put them in .npmrc.")
    } else {
        advice
    }
}

/// The command through the inject engine: npm itself runs, with the user's
/// arguments unchanged; matchapack declines only what it must know before
/// npm starts. `command`: npm's name for the routed word.
fn run_inject(args: &[String], command: &str, log: &dyn Fn(&str, Value)) -> i32 {
    let opts = match decide_inject(args) {
        Ok(o) => o,
        Err(reason) => {
            log("declined", json!({ "engine": "inject", "reason": reason }));
            return decline(args, command, &reason);
        }
    };
    util::set_quiet(true);
    // The install family is typed often and usually places nothing: no
    // banner, and one line at the end only when something was placed
    // (the owner's decision Q6).
    let ci = command == "ci";
    if ci {
        eprintln!("matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)");
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::inject::run(&opts)));
    let outcome = match outcome {
        Ok(o) => o,
        // A panic loses the run's stats; npm's root report is kept apart.
        Err(_) => Err(InstallError::Broke { error: anyhow::anyhow!("internal error (panic)"), inject: crate::inject::root_report() }),
    };
    // For the install family a second run of the command would not repair
    // anything (npm leaves in place what is there), and npm ci rebuilds
    // from the lockfile, which is not what plain npm would have produced
    // (the owner's decision Q5): stop, say how to check and rebuild.
    // npm's root and its package-lock setting when npm reported them;
    // otherwise the start directory.
    let stop = |e: &anyhow::Error, what: &str, inject: Option<&Value>| {
        // Without npm's root report matchapack had placed nothing (clones
        // are refused until the root is accepted, serve.rs).
        let root = inject.and_then(|i| i["root"].as_str()).map(std::path::PathBuf::from);
        let settings = inject.map(RepairSettings::of).unwrap_or_default();
        let command = inject.and_then(|i| i["command"].as_str()).unwrap_or(command);
        let env: Vec<String> = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).collect();
        // What matchapack placed outside the root's node_modules in this
        // run (the server's list); in a file when the list is long.
        let placed = root.as_deref().map(|r| placed_outside(r, inject)).unwrap_or(Placed { dirs: vec![], skipped: 0, known: true });
        let list = (placed.dirs.len() > LIST_IN_FILE).then(|| write_removal_list(&placed.dirs.iter().flat_map(|d| removal_targets(d)).collect::<Vec<_>>())).flatten();
        eprintln!("matchapack: {}; {what}. {}", one_line(e), repair_advice(args, command, root.as_deref(), &settings, &env, &placed, list.as_deref()));
    };
    // Stopped by a signal while npm ran (src/signals.rs): npm was told
    // to stop, or got the signal itself, and has exited. Nothing runs npm
    // again (no plain npm ci after a broken run, no plain npm after a
    // decline): the user asked for the install to stop. Logged as
    // interrupted; the status is npm's (main.rs ends by npm's signal when it
    // died of one). npm finishing with 0 all the same is a normal run.
    if let (Some(sig), Err(e)) = (crate::signals::received(), &outcome) {
        let (code, inject, broke) = match e {
            InstallError::Npm { code, inject } => (Some(*code), Some(inject.clone()), None),
            InstallError::Trap { error, code, inject } => (Some(*code), Some(inject.clone()), Some(error)),
            InstallError::Broke { error, inject } => (None, inject.clone(), Some(error)),
            InstallError::Declined(_) => (None, None, None),
        };
        crate::signals::say(sig, "npm", code);
        let mut rec = json!({ "engine": "inject", "signal": crate::signals::name(sig), "code": code,
                              "reason": format!("stopped by {}", crate::signals::name(sig)), "inject": inject });
        if let InstallError::Declined(why) = e {
            rec["declined"] = json!(one_line(why));
        }
        // The run broke as well: its error is kept, and so is the repair
        // advice (also for npm ci, which is not rebuilt here).
        if let Some(error) = broke {
            rec["error"] = json!(one_line(error));
            stop(error, "node_modules may not be what npm alone would have written", inject.as_ref());
        }
        log("interrupted", with_root(rec, inject.as_ref()));
        return code.filter(|c| *c != 0).unwrap_or(128 + sig);
    }
    match outcome {
        Ok(s) => {
            let by_npm = s.inject.as_ref().and_then(|i| i["fallbacks"].as_object()).map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>()).unwrap_or(0);
            // Without the hook npm extracted every package it installed.
            let by_npm = match s.inject.as_ref() {
                Some(i) if i["hook"] == "not-installed" => i["installed"].as_u64().unwrap_or(by_npm),
                _ => by_npm,
            };
            if ci {
                eprintln!(
                    "matchapack: added {} packages from the store in {:.1}s (inject engine; npm extracted {by_npm})",
                    s.placed, s.secs
                );
            } else if s.placed as u64 + by_npm > 0 {
                eprintln!(
                    "matchapack: {} package{} cloned from the store, npm extracted {by_npm} (inject engine; MATCHAPACK=0 npm {} for plain npm)",
                    s.placed,
                    if s.placed == 1 { "" } else { "s" },
                    args[0]
                );
            }
            log(
                "installed",
                with_root(
                    json!({ "engine": "inject", "secs": s.secs, "packages": s.placed, "store_new": s.store_new,
                            "bytes_logical": s.bytes_logical, "bytes_store_new": s.bytes_store_new, "inject": s.inject }),
                    s.inject.as_ref(),
                ),
            );
            0
        }
        Err(InstallError::Declined(e)) => {
            log("declined", json!({ "engine": "inject", "reason": one_line(&e) }));
            decline(args, command, &one_line(&e))
        }
        // npm ran and failed, and has said why: its status is the answer.
        Err(InstallError::Npm { code, inject }) => {
            log("npm_failed", with_root(json!({ "engine": "inject", "code": code, "reason": format!("npm exited with status {code}"), "inject": inject }), Some(&inject)));
            code
        }
        // npm read a key of the hook's result that pacote's own result does
        // not have (inject.rs `trap_fired`).
        Err(InstallError::Trap { error, code, inject }) => {
            let ci = inject["command"].as_str().unwrap_or(command) == "ci";
            log("broke", with_root(json!({ "engine": "inject", "reason": one_line(&error), "repair": if ci { "npm ci" } else { "stopped" }, "inject": inject }), Some(&inject)));
            if ci {
                let removed = remove_placed_outside(Some(&inject));
                eprintln!("{}", rebuild_line(&error, removed));
                return exec_npm(args);
            }
            stop(&error, "node_modules, package.json and package-lock.json may not be what npm would have written", Some(&inject));
            if code != 0 { code } else { 1 }
        }
        // A panic, a lost child status: with npm's settings when npm had
        // reported them (the root, save, the workspace filter), so that the
        // advice is the one that fits.
        Err(InstallError::Broke { error, inject }) => {
            let ci = inject.as_ref().and_then(|i| i["command"].as_str()).unwrap_or(command) == "ci";
            let rec = json!({ "engine": "inject", "reason": one_line(&error), "repair": if ci { "npm ci" } else { "stopped" }, "inject": inject });
            log("broke", with_root(rec, inject.as_ref()));
            if ci {
                let removed = remove_placed_outside(inject.as_ref());
                eprintln!("{}", rebuild_line(&error, removed));
                return exec_npm(args);
            }
            // pacote's extraction not what the clones reproduce (serve.rs
            // `halt_on_extraction`):
            // npm finished, but its files may not be what it alone wrote.
            let what = if inject.as_ref().is_some_and(|i| i["extraction_unexpected"].is_string()) {
                "node_modules may not be what npm alone would have written"
            } else {
                "npm may not have finished as it would have alone"
            };
            stop(&error, what, inject.as_ref());
            1
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
/// `command`: npm's name for the routed word. The run is logged by the
/// caller in every case; only the line is left out (`quiet_decline`).
fn decline(args: &[String], command: &str, reason: &str) -> i32 {
    if std::env::var("MATCHAPACK_NO_FALLBACK").as_deref() == Ok("1") {
        eprintln!("matchapack: not handled, left unchanged ({reason})");
        return 3;
    }
    if !quiet_decline(command, reason) {
        eprintln!("matchapack: running plain npm instead ({reason})");
    }
    exec_npm(args)
}

/// Declines the install family logs without printing a line (the owner's
/// decision, 2026-11-15): a global install, a project outside the trial
/// scope, and a run that installs nothing. These commands are typed often,
/// `npm install -g <pkg>` above all, and the line would say every time
/// what the user already knows. `npm ci` keeps the line, and so does every
/// other reason (a gate that says something about this npm or project).
/// The reasons come from `early_flags`, serve.rs `accept_root` and
/// `trial::in_scope`; the unit tests run each of them through this.
pub(crate) fn quiet_decline(command: &str, reason: &str) -> bool {
    command != "ci"
        && (reason == trial::OUT_OF_SCOPE
            || reason.ends_with("makes npm install globally")
            || reason == "npm installs globally"
            || reason.contains("installs nothing"))
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
/// install under new extraction code (pacote, tar, libuv) compares every store entry it uses
/// with pacote's extraction; npm placing packages the hook never saw is
/// reported). A major is listed once the inject harness has run on it
/// (`scripts/verify-npm.sh`, `scripts/test-inject.sh`):
/// 10 (10.8.2 and 10.9.2, also a 3-workspace project; 10.5.2, 10.7.0 and
/// 10.9.10), 11 (11.17–11.20 in use), 12 (12.2.2). Inside a major,
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
/// and the date (src/npm-fingerprints.json; docs/architecture.md, "Start-up:
/// the lazy hook and npm's install root"). Inside a listed major, this is what decides whether an npm
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

    #[test]
    fn the_record_carries_the_before_and_after_state() {
        let d = std::env::temp_dir().join(format!("matchapack-wrapper-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("package.json"), "{}").unwrap();
        let before = crate::replay::state(&d, crate::replay::NPM_LOCKFILES);
        std::fs::write(d.join("package-lock.json"), "x").unwrap();
        let inject = serde_json::json!({ "root": d.to_str().unwrap(), "command": "install", "before": before });
        let r = super::with_root(serde_json::json!({ "engine": "inject", "inject": inject.clone() }), Some(&inject));
        assert!(r["inject"].get("before").is_none(), "the before-state is logged once, at the top level");
        assert_eq!(r["files_complete"], true);
        assert_eq!(r["node_modules_before"], "absent");
        assert_eq!(r["files_before"]["package-lock.json"], serde_json::Value::Null, "absent before the run");
        assert_eq!(r["files_after"]["package-lock.json"], "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881", "sha256 of x, after the run");
        assert_eq!(r["files_before"]["package.json"], r["files_after"]["package.json"]);
        // No root reported: nothing.
        let r = super::with_root(serde_json::json!({}), Some(&serde_json::json!({ "before": before })));
        assert!(r.get("files_after").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    use super::{early_flags, fingerprints_allowed, inject_version_allowed, is_fingerprint, npm_fingerprints, quiet_decline};

    #[test]
    fn flags_that_decline_before_node_starts() {
        let f = |a: &[&str]| early_flags(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // Each of these declines without a line for the install family,
        // with the line for npm ci (`quiet_decline`).
        for a in [&["-g"][..], &["--global"], &["--location=global"], &["--no-audit", "--location", "global"]] {
            let why = f(a).unwrap_err();
            assert!(why.contains("makes npm install globally"), "{a:?}");
            assert!(quiet_decline("install", &why) && quiet_decline("uninstall", &why) && !quiet_decline("ci", &why), "{why}");
        }
        for a in [&["--dry-run"][..], &["--help"], &["-h"], &["-?"], &["--usage"], &["--package-lock-only"]] {
            let why = f(a).unwrap_err();
            assert!(why.contains("installs nothing"), "{a:?}");
            assert!(quiet_decline("update", &why) && !quiet_decline("ci", &why), "{why}");
        }
        let why = crate::trial::OUT_OF_SCOPE;
        assert!(quiet_decline("install", why) && !quiet_decline("ci", why));
        assert!(!quiet_decline("install", "umask is 002 (npm ci would give group write)"));
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

    /// The three lists of routed first words cannot drift: the Rust table,
    /// the `case` pattern of shell/matchapack.sh, the `contains` list of
    /// shell/matchapack.fish.
    #[test]
    fn the_shell_functions_route_the_words_of_the_table() {
        let table: Vec<&str> = super::ROUTED.iter().map(|(w, _)| *w).collect();
        // The sh `case` pattern: from `case "${1-}" in` to the `)` that ends it.
        let sh = include_str!("../shell/matchapack.sh");
        let pat = sh.split("case \"${1-}\" in").nth(1).and_then(|r| r.split(')').next()).unwrap();
        let sh_words: Vec<&str> = pat.split(['|', '\\', '\n', ' ']).map(str::trim).filter(|w| !w.is_empty()).collect();
        assert_eq!(sh_words, table, "shell/matchapack.sh");
        // The fish `contains` list, over continued lines.
        let fish = include_str!("../shell/matchapack.fish");
        let rest = fish.split("contains -- \"$argv[1]\"").nth(1).unwrap();
        let mut fish_words = vec![];
        for line in rest.lines() {
            let cont = line.trim_end().ends_with('\\');
            fish_words.extend(line.trim_end().trim_end_matches('\\').split_whitespace());
            if !cont {
                break;
            }
        }
        assert_eq!(fish_words, table, "shell/matchapack.fish");
        // Each word names one of the commands the server accepts.
        for (w, c) in super::ROUTED {
            assert!(crate::serve::ROOT_COMMANDS.contains(c), "{w} → {c}");
            assert_eq!(super::routed(w), Some(*c));
        }
        for w in ["run", "test", "exec", "link", "dedupe", "it", "cit", "upd", "--version", "", "--prefix"] {
            assert_eq!(super::routed(w), None, "{w}");
        }
    }

    #[test]
    fn the_repair_command_keeps_the_commands_arguments() {
        use super::{filter_env, placed_outside, repair_advice, shell_quote, Placed, RepairSettings};
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let d = crate::store::tests::scratch("wrapper-repair");
        std::fs::create_dir_all(&d).unwrap();
        let root = shell_quote(&d.display().to_string());
        let nm = shell_quote(&d.join("node_modules").display().to_string());
        let rm = format!("  rm -rf {nm}");
        let none = Placed { dirs: vec![], skipped: 0, known: true };
        let ends = |s: String, tail: &str| assert!(s.ends_with(tail), "{s}\n  does not end with\n{tail}");
        let known = RepairSettings { package_lock: Some(true), save: Some(true), workspace_filter: Some(false), flags: Some(vec![]), npmrc: vec![], unsure: false, missing: vec![] };
        let adv = |args: &[&str], cmd: &str, s: &RepairSettings| repair_advice(&a(args), cmd, Some(d.as_path()), s, &[], &none, None);
        // No lockfile in the root: npm ci would refuse; the root's
        // node_modules removed, then the same command with plain npm; each
        // command on its own line.
        let s = adv(&["install", "--no-package-lock", "left-pad"], "install", &known);
        assert!(s.starts_with("There is no package-lock.json from this run"), "{s}");
        ends(s, &format!("then remove node_modules and run the command again with plain npm:\n{rm}\n  MATCHAPACK=0 npm install --no-package-lock left-pad"));
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();
        // The default: npm ci with the command's arguments, quoted (`^` is
        // a glob character in zsh with EXTENDED_GLOB; `=x` is zsh's `=cmd`);
        // npm ci empties the root's node_modules itself, so no `rm` (and
        // never an `rm -rf` with no path).
        let s = adv(&["install", "--omit=dev", "left-pad@1.3.0"], "install", &known);
        assert!(!s.contains("rm -rf"), "{s}");
        ends(s, "rebuild with plain npm:\n  MATCHAPACK=0 npm ci --omit=dev left-pad@1.3.0");
        ends(adv(&["i", "a b", "it's", "", "left-pad@^1.3.0", "=x"], "install", &RepairSettings::default()), "MATCHAPACK=0 npm ci 'a b' 'it'\\''s' '' 'left-pad@^1.3.0' '=x'");
        assert_eq!(shell_quote("--omit=dev"), "--omit=dev");
        // A backslash outside the quotes (fish reads `\\` inside them);
        // fish's `%self`; zsh's MAGIC_EQUAL_SUBST on `==`.
        assert_eq!(shell_quote("--cache=C:\\x\\'y"), "'--cache=C:'\\\\'x'\\\\''\\''y'");
        assert_eq!((shell_quote("%self"), shell_quote("--opt==ls"), shell_quote("--a=b")), ("'%self'".into(), "'--opt==ls'".into(), "--a=b".into()));
        ends(adv(&["uninstall"], "uninstall", &known), "\n  MATCHAPACK=0 npm ci");
        // A lockfile this run did not write (package-lock=false): not npm ci.
        let nolock = RepairSettings { package_lock: Some(false), ..known.clone() };
        ends(adv(&["update"], "update", &nolock), &format!("run the command again with plain npm:\n{rm}\n  MATCHAPACK=0 npm update"));
        // --no-save: the lockfile lacks what the run added; npm update saves
        // no package.json by default, so npm ci stays right for it.
        let nosave = RepairSettings { save: Some(false), ..known.clone() };
        let s = adv(&["install", "--no-save", "left-pad"], "install", &nosave);
        assert!(s.starts_with("This run saved neither package.json nor package-lock.json (--no-save)"), "{s}");
        ends(s, "\n  MATCHAPACK=0 npm install --no-save left-pad");
        ends(adv(&["uninstall", "--no-save", "x"], "uninstall", &nosave), "\n  MATCHAPACK=0 npm uninstall --no-save x");
        ends(adv(&["update", "x"], "update", &nosave), "\n  MATCHAPACK=0 npm ci x");
        // Filtered to some workspaces: npm ci of the whole project, the root
        // named with --prefix (no cd: fish rejects a subshell), with npm's
        // own parse of the options when it passed the check.
        let ws = RepairSettings { workspace_filter: Some(true), ..known.clone() };
        let parsed = RepairSettings { flags: Some(a(&["--omit=dev"])), ..ws.clone() };
        let s = adv(&["install", "--prefi=project", "-w", "a", "left-pad", "--omit", "dev"], "install", &parsed);
        assert!(s.starts_with("The command was filtered to some workspaces"), "{s}");
        ends(s, &format!("rebuild the whole project with plain npm.\n  MATCHAPACK=0 npm ci --prefix {root} --omit=dev"));
        // A filter from the environment, in any case, is unset for that
        // command; one from an npmrc cannot be, and the advice says so,
        // before the commands.
        let env = a(&["PATH", "NPM_CONFIG_workspace", "npm_config_include_workspace_root", "npm_config_workspaces_update", "npm_config_omit"]);
        assert_eq!(filter_env(&env), a(&["NPM_CONFIG_workspace", "npm_config_include_workspace_root"]));
        let s = repair_advice(&a(&["i", "x"]), "install", Some(d.as_path()), &parsed, &env, &none, None);
        ends(s, &format!("\n  env -u NPM_CONFIG_workspace -u npm_config_include_workspace_root MATCHAPACK=0 npm ci --prefix {root} --omit=dev"));
        let rc = RepairSettings { npmrc: vec!["project".into()], ..parsed.clone() };
        let s = adv(&["i", "x"], "install", &rc);
        assert!(s.contains("plain npm. Your project npmrc sets a workspace filter, which a command line cannot unset: take it out for that command, or npm rebuilds only those workspaces.\n  MATCHAPACK=0 npm ci"), "{s}");
        // Unsure (the check failed, could not be asked, or no options were
        // reported): no options, not the typed words either; the sentence
        // comes first and names every option it could not repeat.
        let unsure = RepairSettings { flags: None, unsure: true, missing: a(&["--ignore-scripts", "--omit=dev", "--audit=false"]), ..ws.clone() };
        let s = adv(&["install", "--no-tag", "false", "-w", "a", "x", "--ignore-scripts", "--omit=dev", "--no-audit"], "install", &unsure);
        assert!(s.contains("plain npm. matchapack could not repeat your options safely, so the whole-project command below has none of them: add the ones you need (you used --ignore-scripts, --omit=dev, --audit=false: add them).\n"), "{s}");
        ends(s, &format!("\n  MATCHAPACK=0 npm ci --prefix {root}"));
        // The options it names are shown as the log shows them: no secret.
        let secret = RepairSettings { missing: a(&["--otp=123456", "--//r.example/:_authToken=FAKETOKEN123", "--_password=hunter2"]), ..unsure.clone() };
        let s = adv(&["install", "--include", "false", "-w", "a"], "install", &secret);
        assert!(s.contains("(you used '--otp=***', '--//r.example/:_authToken=***', '--_password=***': add them)") && s.contains("credentials in the command line are shown as ***"), "{s}");
        assert!(!s.contains("123456") && !s.contains("FAKETOKEN") && !s.contains("hunter2"), "{s}");
        let of = |repair: serde_json::Value| RepairSettings::of(&serde_json::json!({ "workspace_filter": true, "repair": repair }));
        for r in [
            serde_json::json!({ "args": ["--omit=dev"], "checked": null }),
            serde_json::json!({ "args": ["--omit=dev"], "checked": false }),
            serde_json::json!({ "checked": true }),
            serde_json::json!(null),
        ] {
            let s = of(r.clone());
            assert!(s.unsure && s.flags.is_none(), "{r}: {s:?}");
            ends(repair_advice(&a(&["install", "-w", "a", "--omit=dev"]), "install", Some(d.as_path()), &s, &[], &none, None), &format!("\n  MATCHAPACK=0 npm ci --prefix {root}"));
        }
        let s = of(serde_json::json!({ "args": ["--omit=dev"], "checked": true }));
        assert!(!s.unsure && s.flags == Some(a(&["--omit=dev"])), "{s:?}");
        // Filtered, and --no-save (or no lockfile): remove the root's
        // node_modules, reinstall the whole project, then the command again.
        let both = RepairSettings { save: Some(false), ..parsed.clone() };
        let s = adv(&["install", "--no-save", "left-pad", "-w", "a"], "install", &both);
        assert!(s.starts_with("This run saved neither package.json nor package-lock.json (--no-save), so npm ci would rebuild without what it added, and the command was filtered to some workspaces"), "{s}");
        ends(s, &format!("and run the command again.\n{rm}\n  MATCHAPACK=0 npm install --prefix {root} --omit=dev\n  MATCHAPACK=0 npm install --no-save left-pad -w a"));
        // Credentials are shown as *** (the run log's redaction), and the
        // advice says so; without any, no such line.
        let cred = RepairSettings { flags: Some(a(&["--//r.example/:_authToken=SECRET", "--_password=-abc"])), ..ws.clone() };
        let s = adv(&["install", "--//r.example/:_authToken=SECRET", "--_password=-abc", "-w", "a"], "install", &cred);
        assert!(!s.contains("SECRET") && !s.contains("-abc"), "{s}");
        assert!(s.contains("'--//r.example/:_authToken=***' '--_password=***'"), "{s}");
        assert!(s.ends_with("matchapack: credentials in the command line are shown as *** above: add them back, or put them in .npmrc."), "{s}");
        assert!(!adv(&["i", "x"], "install", &parsed).contains("credentials"));
        let s = adv(&["install", "--no-save", "--//r.example/:_authToken=SECRET", "x"], "install", &nosave);
        assert!(!s.contains("SECRET") && s.contains("credentials in the command line"), "{s}");
        // Without npm's root report matchapack had placed nothing: the same
        // command again, no npm ci, nothing removed (`-w a` stays as typed).
        let s = repair_advice(&a(&["install", "left-pad@1.3.0", "-w", "a"]), "install", None, &RepairSettings::default(), &[], &none, None);
        assert_eq!(s, "npm had not reported its install root, so matchapack had placed nothing: run the same command again with plain npm:\n  MATCHAPACK=0 npm install left-pad@1.3.0 -w a");
        std::fs::remove_file(d.join("package-lock.json")).unwrap();
        std::fs::write(d.join("npm-shrinkwrap.json"), "{}").unwrap();
        ends(adv(&["rm", "x"], "uninstall", &known), "\n  MATCHAPACK=0 npm ci x");
        // No lockfile and filtered: also the whole-project reinstall first.
        std::fs::remove_file(d.join("npm-shrinkwrap.json")).unwrap();
        let s = adv(&["i", "x", "-w", "a"], "install", &parsed);
        assert!(s.starts_with("There is no package-lock.json from this run for npm ci to rebuild from, and the command was filtered"), "{s}");
        std::fs::write(d.join("package-lock.json"), "{}").unwrap();

        // What matchapack placed outside the root's node_modules (the
        // server's list, `inject.placed_outside`), and nothing else: not
        // the root's node_modules, nothing outside the project (a member
        // that is a symlink to elsewhere, a node_modules that is a symlink),
        // not what is gone already.
        for p in ["node_modules/a", "packages/b/node_modules/ms", "packages/c/node_modules/@s/p"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        let other = crate::store::tests::scratch("wrapper-repair-other");
        std::fs::create_dir_all(other.join("ext/node_modules/y")).unwrap();
        std::fs::create_dir_all(other.join("nm/z")).unwrap();
        std::os::unix::fs::symlink(other.join("ext"), d.join("packages/ext")).unwrap();
        std::fs::create_dir_all(d.join("packages/l")).unwrap();
        std::os::unix::fs::symlink(other.join("nm"), d.join("packages/l/node_modules")).unwrap();
        let inject = serde_json::json!({ "placed_outside": ["node_modules/a", "packages/b/node_modules/ms", "packages/c/node_modules/@s/p", "packages/ext/node_modules/y", "packages/l/node_modules/z", "packages/gone/node_modules/q", "../out/node_modules/w"] });
        let p = placed_outside(&d, Some(&inject));
        assert_eq!((p.dirs.clone(), p.skipped, p.known), (vec![d.join("packages/b/node_modules/ms"), d.join("packages/c/node_modules/@s/p")], 2, true));
        let s = repair_advice(&a(&["install", "x"]), "install", Some(d.as_path()), &known, &[], &p, None);
        let ms = shell_quote(&d.join("packages/b/node_modules/ms").display().to_string());
        assert!(s.contains(&format!("then remove what matchapack placed outside node_modules, then rebuild with plain npm:\n  rm -rf {ms}\n  rm -rf ")), "{s}");
        assert!(s.contains("\n  MATCHAPACK=0 npm ci x\nmatchapack: 2 package directories behind a symlink to outside the project are left out: matchapack removes nothing outside the project."), "{s}");
        assert!(!s.contains(&other.display().to_string()) && !s.contains("packages/ext") && !s.contains("packages/l/") && !s.contains(&format!("rm -rf {nm}")), "{s}");
        // More than 20: one command over a file that lists them.
        let many = Placed { dirs: (0..21).map(|i| d.join(format!("packages/m{i}/node_modules/x"))).collect(), skipped: 0, known: true };
        let list = d.join("repair.list");
        let s = repair_advice(&a(&["install", "x"]), "install", Some(d.as_path()), &known, &[], &many, Some(list.as_path()));
        ends(s, &format!("then remove what matchapack placed in 21 package directories outside node_modules (the file in the first command lists it), then rebuild with plain npm:\n  xargs -0 rm -rf -- < {}\n  MATCHAPACK=0 npm ci x", shell_quote(&list.display().to_string())));
        // The file itself: written for the user only, followed with the
        // printed command (sh), it removes what it lists; an old list goes.
        let ld = d.join("lists");
        std::fs::create_dir_all(&ld).unwrap();
        let old = ld.join("repair-1-1.list");
        std::fs::write(&old, "x").unwrap();
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(two_days).unwrap();
        let targets: Vec<_> = (0..25).map(|i| d.join(format!("t/p{i} x/node_modules/y"))).collect();
        targets.iter().for_each(|t| std::fs::create_dir_all(t).unwrap());
        let f = super::write_removal_list_in(&ld, &targets).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!old.exists());
        let ok = std::process::Command::new("sh").arg("-c").arg(format!("xargs -0 rm -rf -- < {}", shell_quote(&f.display().to_string()))).status().unwrap();
        assert!(ok.success() && targets.iter().all(|t| !t.exists()) && d.join("t/p0 x/node_modules").is_dir());
        // A placed directory with a node_modules npm kept (a nested package
        // moved back under a package it replaced): only its other entries
        // are removed, by the advice and by the automatic path.
        let kept = d.join("packages/k/node_modules/is-odd");
        std::fs::create_dir_all(kept.join("node_modules/is-number")).unwrap();
        std::fs::write(kept.join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(kept.join("lib")).unwrap();
        assert_eq!(super::removal_targets(&kept), vec![kept.join("lib"), kept.join("package.json")]);
        let p = Placed { dirs: vec![kept.clone()], skipped: 0, known: true };
        let s = repair_advice(&a(&["install", "x"]), "install", Some(d.as_path()), &known, &[], &p, None);
        assert!(s.contains(&format!("\n  rm -rf {} {}\n", shell_quote(&kept.join("lib").display().to_string()), shell_quote(&kept.join("package.json").display().to_string()))), "{s}");
        let inject = serde_json::json!({ "root": d.display().to_string(), "placed_outside": ["packages/k/node_modules/is-odd"] });
        assert_eq!(super::remove_placed_outside(Some(&inject)), 1);
        assert!(kept.join("node_modules/is-number").is_dir() && !kept.join("lib").exists() && !kept.join("package.json").exists());
        // A path that holds another directory than the one placed (npm's
        // rollback put its own back): not listed.
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&kept).unwrap();
        let same = serde_json::json!({ "placed_outside": ["packages/k/node_modules/is-odd"], "placed_outside_ids": { "packages/k/node_modules/is-odd": [m.dev(), m.ino()] } });
        let moved = serde_json::json!({ "placed_outside": ["packages/k/node_modules/is-odd"], "placed_outside_ids": { "packages/k/node_modules/is-odd": [m.dev(), m.ino() + 1] } });
        assert_eq!(placed_outside(&d, Some(&same)).dirs, vec![kept.clone()]);
        assert!(placed_outside(&d, Some(&moved)).dirs.is_empty());
        // No list in the run's record: nothing outside the root's
        // node_modules is removed, and the advice says so.
        let p = placed_outside(&d, Some(&serde_json::json!({})));
        assert!(!p.known && p.dirs.is_empty());
        let s = repair_advice(&a(&["install", "x"]), "install", Some(d.as_path()), &known, &[], &p, None);
        assert!(s.ends_with("\nmatchapack: this run's record does not list what matchapack placed outside node_modules, so nothing there is removed: check a workspace member's own node_modules yourself."), "{s}");
        assert!(other.join("ext/node_modules/y").is_dir() && other.join("nm/z").is_dir());
        let _ = std::fs::remove_dir_all(&other);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn npm_fingerprint_list() {
        let list = npm_fingerprints();
        // The eight texts of pacote's lib/fetcher.js in npm 10.0.0 to 12.2.2,
        // each listed after test-inject.sh and verify-npm.sh passed on a
        // release with it, must stay; a fingerprint added later needs no
        // change here (no fixed count), only distinct entries.
        for p in ["07f98090ee5bc371", "8d7208cf1a9e98ee", "cef1bd59a34dfe60", "1a385341f75d6c77", "45eabb392a0c7c33", "6972084f1fa0d668", "97dbf9a402ad3329", "b7d5a64232232301"] {
            assert!(list.iter().any(|f| f.starts_with(p)), "{p}");
        }
        assert!(list.len() >= 8, "{list:?}");
        assert_eq!(list.iter().collect::<std::collections::BTreeSet<_>>().len(), list.len(), "duplicates: {list:?}");
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
