//! The Yarn 1 engine (`matchapack yarn <args>`; NOTES "Yarn 1 engine"): Yarn 1
//! runs its own `yarn install`, in a node child started here through the hook
//! in src/yarn1-hook.js, and during Yarn's link step each file Yarn copies
//! from its cache into node_modules is made as a copy-on-write clone of the
//! cache file instead (macOS `clonefile(2)`; Node cannot clone on macOS).
//! Yarn's own code places every file, so the tree is Yarn's by construction;
//! no store entry is used (Yarn's cache is the source), so `store gc`,
//! `store verify` and the project records are not involved.
//!
//! A clone is made to look like the copy Node's `fs.copyFile` would have
//! left, by facts the hook measures at start with Node's own copy (times of
//! an empty and a non-empty file, the group): times Node's copy keeps are left
//! as the clone has them (the cache file's), the others set to now; the group
//! is set to the cache file's when Node's copy does that (libuv's `fchown`,
//! result ignored, so `lchown` with the result ignored); every extended
//! attribute but `com.apple.provenance` is removed (Node's copy carries none
//! over; the kernel adds `com.apple.provenance` to both). Yarn's `fixTimes`
//! then runs as after a copy. A clone that cannot be made so is removed, and
//! Node copies the file.
//!
//! Protocol (JSON lines over fd 3, the inject engine's transport,
//! `serve::serve_lines`):
//!   node → matchapack
//!     {"id":0,"probe":["/…/nonempty","/…/nonempty.copy"]}  before Yarn starts
//!     {"declined":"why"}                     the hook stops; plain Yarn runs
//!     {"hello":true,"yarn":"1.22.22","first":128,"facts":{…},"hook":"on"}
//!       (a workspace command's Yarn adds "child":true and "cwd"; its facts
//!       must be the parent's, or its requests are answered "copy", fatal)
//!     {"link_step":"start"} / {"link_step":"end","counts":{"first-copies":128}}
//!     {"id":7,"src":"/cache/…/f.js","dest":"/project/node_modules/…/f.js"}
//!   matchapack → node
//!     {"id":0,"shares":false}
//!     {"id":7,"ok":true}
//!     {"id":7,"ok":false,"reason":"rejected-dest","fatal":false}
//! `fatal`: no later request can be cloned (the filesystem pair cannot
//! clone, the cache is on another volume, Yarn's cache folder is unknown):
//! the hook stops asking.
//!
//! What a request may name (it names paths; a Yarn cache entry's name cannot
//! be derived from the lockfile): `src` an absolute plain path, a regular file
//! (lstat; never a symlink) whose parent resolves inside Yarn's cache folder;
//! `dest` an absolute plain path whose parent resolves inside the project
//! root and below a `node_modules` of it. The cache folder comes from the
//! user's Yarn (`yarn cache dir`, run here in parallel with the install,
//! with the same working directory, environment and cache arguments); until
//! it is known every request is answered "copy". These checks catch a
//! confused hook, not an attacker: this process does nothing node could not
//! do with `fs.copyFile` as the same user. The verdict is kept per parent
//! directory (there are about seven files per directory); a directory
//! swapped for a symlink after its check is not caught (as in serve.rs).

use crate::serve::serve_lines;
use crate::{trial, util};
use anyhow::{Context, Result};
use crate::replay;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Yarn options that take a value (required or optional), as Yarn's own
/// command-line parser binds them (`commander.optionFor`, lib/cli.js "get
/// command name"): the word after one of these is its value, not a command.
const VALUE_OPTIONS: &[&str] = &[
    "--use-yarnrc",
    "--prod",
    "--production",
    "--link-folder",
    "--global-folder",
    "--modules-folder",
    "--preferred-cache-folder",
    "--cache-folder",
    "--mutex",
    "--emoji",
    "--cwd",
    "--proxy",
    "--https-proxy",
    "--registry",
    "--network-concurrency",
    "--network-timeout",
    "--scripts-prepend-node-path",
    "--otp",
];

/// Options that move the install out of the project matchapack checks
/// (the clone server accepts destinations inside it only).
const MOVING_OPTIONS: &[&str] = &["--modules-folder", "--cwd", "--global-folder"];

/// Options that decide where Yarn's cache is: passed to `yarn cache dir`.
const CACHE_OPTIONS: &[&str] = &["--cache-folder", "--preferred-cache-folder", "--use-yarnrc", "--no-default-rc"];

/// How long requests wait for Yarn's cache folder (`yarn cache dir`, run in
/// parallel with the install), counted once from the server's start, not per
/// request: after that, while it is still unknown, every request is answered
/// "copy" at once (per request, a slow `yarn cache dir` held each worker for
/// the full wait, backed the queue up and set off the hook's slow-answer
/// rule; CI run 43306715867). Measured: with a
/// warm cache a small install reaches its link step before `yarn cache dir`
/// answers, and answering "copy" at once cloned nothing (yarn1-emptyfirst:
/// 314 of 314 files).
const CACHE_WAIT: Duration = Duration::from_secs(10);

/// The arguments a replay of a full install gives Yarn (scripts/verify-sample.sh
/// --yarn, through `verify-yarn1.sh`, which says `install` itself and gives
/// the cache folder the run used): `args` without the command word
/// `install` and without `--cache-folder` / `--preferred-cache-folder` and
/// their values. The command word is found as Yarn finds it (`command_at`).
fn replay_args(args: &[String]) -> Vec<String> {
    without_cache_folder(args, command_at(args).filter(|&i| args[i] == "install"))
}

/// `args` without `--cache-folder` / `--preferred-cache-folder` and their
/// values, and without the word at `at`.
fn without_cache_folder(args: &[String], at: Option<usize>) -> Vec<String> {
    let mut out = vec![];
    let mut skip_value = false;
    for (i, a) in args.iter().enumerate() {
        if std::mem::take(&mut skip_value) || Some(i) == at {
            continue;
        }
        let name = option_name(a);
        if name == "--cache-folder" || name == "--preferred-cache-folder" {
            skip_value = name == a.as_str();
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// The name of the command Yarn will run (empty: Yarn's default, install),
/// found the way Yarn finds it.
pub fn command(args: &[String]) -> &str {
    command_at(args).map_or("", |i| args[i].as_str())
}

/// Where in `args` the command name is (`command`): the first word that is
/// neither an option nor the value of the option before it.
fn command_at(args: &[String]) -> Option<usize> {
    let mut prev_binds = false;
    for (i, a) in args.iter().enumerate() {
        let is_option = a.starts_with('-');
        if !is_option && !prev_binds {
            return Some(i);
        }
        prev_binds = is_option && VALUE_OPTIONS.contains(&a.as_str());
    }
    None
}

/// The Yarn commands the engine runs (NOTES "Yarn 1 commands"), as Yarn
/// names them (`src/cli/aliases.js`: `upgrade-interactive` is
/// `upgradeInteractive`). Each runs Yarn's link step (`Install.init`) in
/// the process the hook is in: `add` and `upgrade` (an `Add`), `remove` (an
/// `Install` after it edits package.json; removing a package can move
/// others, which Yarn copies), `upgrade-interactive` (one `Add` per
/// dependency type and workspace, after its prompt). `yarn global …` installs
/// outside the project and stays plain Yarn.
const ENGINE_COMMANDS: &[&str] = &["", "install", "add", "upgrade", "remove", "upgrade-interactive", "upgradeInteractive"];

/// `yarn workspace <name> <command> …`: Yarn starts a second Yarn, `node
/// <Yarn's bundle> <command> …` in the workspace's directory; the hook
/// starts that one with itself too (src/yarn1-hook.js, `wrapWorkspaceChild`). Not
/// `install`: from a workspace Yarn installs at the root, as `yarn install`
/// there does (routed).
const WORKSPACE_COMMANDS: &[&str] = &["add", "remove", "upgrade", "upgrade-interactive", "upgradeInteractive"];

/// The command the engine runs for `matchapack yarn <args>`, as logged
/// (`install`, `add`, `upgrade`, `remove`, `upgrade-interactive`,
/// `workspace add`, …), or None: plain Yarn (`--version`, `--help`, every
/// other command). Found as Yarn finds it (`command`); for `workspace`, the
/// two words after it must be the workspace's name and one of
/// WORKSPACE_COMMANDS, as Yarn splits them (`src/cli/index.js`,
/// PROXY_COMMANDS: the first two words after `workspace` are its own).
pub fn engine_command(args: &[String]) -> Option<String> {
    if args.first().is_some_and(|a| a == "-v") || args.iter().any(|a| a == "--version" || a == "--help" || a == "-h") {
        return None;
    }
    let c = command(args);
    if ENGINE_COMMANDS.contains(&c) {
        return Some(match c {
            "" => "install".into(),
            "upgradeInteractive" => "upgrade-interactive".into(),
            c => c.into(),
        });
    }
    let sub = workspace_args(args)?;
    let c = if sub[1] == "upgradeInteractive" { "upgrade-interactive" } else { sub[1].as_str() };
    WORKSPACE_COMMANDS.contains(&c).then(|| format!("workspace {c}"))
}

/// For `yarn [flags] workspace <name> <words…>`: `[<name>, <words…>]`, the
/// name and the arguments of the Yarn the workspace command starts. None
/// when the command is not `workspace`, or the name or the command is
/// missing or an option.
fn workspace_args(args: &[String]) -> Option<&[String]> {
    let at = command_at(args).filter(|&i| args[i] == "workspace")?;
    let rest = &args[at + 1..];
    (rest.len() >= 2 && !rest[0].starts_with('-') && !rest[1].starts_with('-')).then_some(rest)
}

fn option_name(a: &str) -> &str {
    a.split('=').next().unwrap_or(a)
}

/// The cache options among `args`, with their values, for `yarn cache dir`.
fn cache_args(args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = option_name(a);
        if !CACHE_OPTIONS.contains(&name) {
            continue;
        }
        out.push(a.clone());
        if name == a.as_str() && VALUE_OPTIONS.contains(&name) {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

/// The Yarn the user's `yarn` runs, as matchapack starts it (`node <js>`).
#[derive(Debug, Clone)]
pub struct YarnBin {
    /// Yarn 1's `bin/yarn.js`, or corepack's `dist/yarn.js`.
    pub js: PathBuf,
    /// Yarn's version; empty for corepack until it has chosen (the hello).
    pub version: String,
    /// `js` is corepack's launcher (`corepack enable` links `yarn` to it):
    /// corepack picks the project's Yarn (`packageManager`, its defaults),
    /// installs it into its cache when needed, and starts that Yarn's
    /// `bin/yarn.js` in the same process, where the hook takes over.
    pub corepack: bool,
    /// What the user's `yarn` adds to Yarn's environment before it starts
    /// `js`: Homebrew's launcher sets PREFIX (`homebrew_launcher`). Given to
    /// every Yarn started here (the install, `yarn cache dir`, plain Yarn on
    /// a decline), so Yarn and its scripts see what plain Homebrew Yarn sees.
    pub env: Vec<(String, String)>,
    /// How node is started for `js`: the `node` on PATH, or, behind a
    /// version manager's shim, exactly the node and environment the shim
    /// gives Yarn (`util::probe_launcher`).
    pub node: util::NodeStart,
}

/// The first Yarn 1 whose link step asks for a clone (`fs.copyFile` with
/// COPYFILE_FICLONE, yarn-classic `src/util/fs-normalized.js`). An older one
/// copies with no flag, the hook leaves every copy to Node, and the engine
/// would cost a start-up for no clone (measured on all 17 releases from
/// 1.0.0 to 1.9.4: 0 files cloned; NOTES "Yarn 1 commands").
const YARN1_FLOOR_MINOR: u32 = 10;

/// Whether Yarn 1 `version` is at or above 1.10.0 (`YARN1_FLOOR_MINOR`):
/// the minor number compared as a number ("1.9" sorts after "1.10" as text).
fn yarn1_clones(version: &str) -> bool {
    let minor = version.strip_prefix("1.").and_then(|r| r.split('.').next()).and_then(|m| m.parse::<u32>().ok());
    minor.is_some_and(|m| m >= YARN1_FLOOR_MINOR)
}

/// The version of Yarn 1's npm package whose `bin/yarn.js` is `js`, or why
/// it is not one (or one too old to clone with).
fn yarn1_package(js: &Path) -> std::result::Result<String, String> {
    let pkg = js.parent().and_then(|b| b.parent()).map(|d| d.join("package.json"));
    let v: Option<Value> = pkg.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    let name = v.as_ref().and_then(|v| v["name"].as_str());
    let version = v.as_ref().and_then(|v| v["version"].as_str()).unwrap_or("").to_string();
    if !js.is_file() || name != Some("yarn") {
        return Err(format!("{} is not Yarn 1's npm package", js.display()));
    }
    if !version.starts_with("1.") {
        return Err(format!("yarn {version} at {} is not Yarn 1", js.display()));
    }
    if !yarn1_clones(&version) {
        return Err(format!("yarn {version} is older than 1.{YARN1_FLOOR_MINOR}.0, the first Yarn 1 that asks for a clone: nothing could be cloned"));
    }
    Ok(version)
}

/// corepack's launcher for `yarn`: `dist/yarn.js` of the package named
/// corepack (bundled with Node <= 24; `npm i -g corepack` from Node 25 on).
fn is_corepack_launcher(real: &Path) -> bool {
    let dist = real.parent();
    real.file_name().is_some_and(|n| n == "yarn.js")
        && dist.and_then(Path::file_name).is_some_and(|n| n == "dist")
        && dist
            .and_then(Path::parent)
            .and_then(|d| std::fs::read(d.join("package.json")).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some_and(|v| v["name"] == "corepack")
}

/// Homebrew's launcher for Yarn 1 (homebrew-core Formula/y/yarn.rb): Yarn
/// 1's package in `<keg>/libexec`, and `<keg>/bin/yarn` the script
/// `Pathname#write_env_script` writes (Homebrew/brew,
/// Library/Homebrew/extend/pathname.rb), byte for byte:
///
/// ```text
/// #!/bin/bash
/// PREFIX="<HOMEBREW_PREFIX>" exec "<keg>/libexec/bin/yarn.js"  "$@"
/// ```
///
/// (two spaces: the formula passes no arguments). `real` is that script,
/// symlinks resolved. Returns the `yarn.js` as the script names it (Yarn's
/// argv[1] under plain Homebrew Yarn) and the PREFIX it sets. Only this exact
/// text, with a PREFIX and a path that bash takes literally inside double
/// quotes (no `"`, `$`, backquote, backslash or newline), naming its own
/// keg's `libexec/bin/yarn.js`; anything else is not recognized.
fn homebrew_launcher(real: &Path) -> Option<(PathBuf, String)> {
    const HEAD: &str = "#!/bin/bash\nPREFIX=\"";
    const MID: &str = "\" exec \"";
    const TAIL: &str = "\"  \"$@\"\n";
    let bin = real.parent()?;
    if real.file_name()? != "yarn" || bin.file_name()? != "bin" {
        return None;
    }
    let meta = std::fs::metadata(real).ok()?;
    if !meta.is_file() || meta.len() > 4096 {
        return None;
    }
    let text = String::from_utf8(std::fs::read(real).ok()?).ok()?;
    let (prefix, target) = text.strip_prefix(HEAD)?.strip_suffix(TAIL)?.split_once(MID)?;
    let literal = |s: &str| s.starts_with('/') && !s.contains(['"', '$', '`', '\\', '\n']);
    if !literal(prefix) || !literal(target) {
        return None;
    }
    let target = PathBuf::from(target);
    let own = bin.parent()?.join("libexec/bin/yarn.js");
    let same = std::fs::canonicalize(&target).ok()? == std::fs::canonicalize(own).ok()?;
    same.then(|| (target, prefix.to_string()))
}

/// The Yarn the user's `yarn` runs: MATCHAPACK_YARN_JS, or `yarn` on PATH,
/// resolved to a file of Yarn 1's npm package (an npm-global install:
/// `…/yarn/bin/yarn.js`, or the `bin/yarn` sh launcher beside it), to
/// Homebrew's launcher for it (`homebrew_launcher`), or to corepack's
/// launcher. A `yarn` on PATH that is none of these (a version manager's
/// shim: volta, asdf, mise in shim mode) is asked what it starts in this
/// directory (`util::probe_launcher`), and that is resolved the same way.
/// Yarn 2+ or another launcher is not handled.
pub fn find_yarn_js() -> std::result::Result<YarnBin, String> {
    match std::env::var_os("MATCHAPACK_YARN_JS") {
        Some(p) => yarn_bin(Path::new(&p)),
        None => {
            let path = std::env::var_os("PATH").ok_or("no PATH")?;
            let cwd = std::env::current_dir().map_err(|e| format!("the current directory: {e}"))?;
            find_yarn_on(&path, &cwd, &|| util::private_run_dir().map_err(|e| format!("{e:#}")))
        }
    }
}

/// `find_yarn_js` for the `yarn` on `path`, run in `cwd`.
fn find_yarn_on(path: &std::ffi::OsStr, cwd: &Path, run_dir: &dyn Fn() -> std::result::Result<PathBuf, String>) -> std::result::Result<YarnBin, String> {
    let p = std::env::split_paths(path).map(|d| d.join("yarn")).find(|p| p.is_file()).ok_or("no yarn on PATH")?;
    match yarn_bin(&p) {
        Err(e) if e.contains(NOT_HANDLED) => {
            util::known_launcher(&p, "yarn").map_err(|why| format!("{e}; {why}"))?;
            let (target, launch) = util::probe_launcher(&p, "yarn", path, cwd, &run_dir()?).map_err(|why| format!("{e}; {why}"))?;
            let y = yarn_entry(&target).map_err(|why| format!("the yarn on PATH ({}) starts {}: {why}", p.display(), target.display()))?;
            Ok(YarnBin { node: util::NodeStart::Launch(launch), ..y })
        }
        // The `yarn` on PATH itself: node as plain Yarn would get it
        // (`util::entry_drops_dyld`).
        r => r.map(|y| YarnBin { node: util::NodeStart::for_entry(&p), ..y }),
    }
}

const NOT_HANDLED: &str = "is neither Yarn 1's npm package, Homebrew's launcher for it, nor corepack's launcher";

/// The Yarn a launcher's node was started on (`js`, symlinks resolved):
/// only the script itself, Yarn 1's `bin/yarn.js` or corepack's launcher,
/// never a file beside it (`yarn_bin` would take a `bin/yarn.js` beside a
/// `bin/policy.js` the launcher starts, and bypass it).
fn yarn_entry(js: &Path) -> std::result::Result<YarnBin, String> {
    if is_corepack_launcher(js) {
        return Ok(YarnBin { js: js.to_path_buf(), version: String::new(), corepack: true, env: Vec::new(), node: util::NodeStart::default() });
    }
    if !(js.file_name().is_some_and(|n| n == "yarn.js") && js.parent().and_then(Path::file_name).is_some_and(|n| n == "bin")) {
        return Err("not Yarn 1's bin/yarn.js or corepack's launcher".into());
    }
    let version = yarn1_package(js)?;
    Ok(YarnBin { js: js.to_path_buf(), version, corepack: false, env: Vec::new(), node: util::NodeStart::default() })
}

/// `find_yarn_js` for the file `p`.
fn yarn_bin(p: &Path) -> std::result::Result<YarnBin, String> {
    let real = std::fs::canonicalize(p).map_err(|e| format!("resolving {}: {e}", p.display()))?;
    if is_corepack_launcher(&real) {
        return Ok(YarnBin { js: real, version: String::new(), corepack: true, env: Vec::new(), node: util::NodeStart::default() });
    }
    let mut env = Vec::new();
    let js = if real.file_name().is_some_and(|n| n == "yarn.js") {
        real.clone()
    } else if let (false, Some((js, prefix))) = (real.with_file_name("yarn.js").exists(), homebrew_launcher(&real)) {
        env.push(("PREFIX".to_string(), prefix));
        js
    } else {
        real.with_file_name("yarn.js")
    };
    let version = yarn1_package(&js).map_err(|e| {
        if e.ends_with("npm package") {
            format!("{} {NOT_HANDLED} (Yarn 2+ or another launcher is not handled)", real.display())
        } else {
            e
        }
    })?;
    Ok(YarnBin { js, version, corepack: false, env, node: util::NodeStart::default() })
}

/// Whether the project at `cwd` is a Yarn 2+ one, decided from files only:
/// a few `stat` calls and small reads, no node and no yarn.js lookup (the
/// owner runs `yarn install` in Yarn 4 worktrees far more often than in Yarn
/// 1 ones; design review 2026-11-13 §4 item 8). Yarn 2+: a `.yarnrc.yml`
/// between `cwd` and the lockfile folder (not above it: a user's
/// `~/.yarnrc.yml` does not make a Yarn 1 project a Yarn 4 one; Yarn 1.22
/// runs a `yarnPath` from it itself), a `packageManager` of `yarn@` other
/// than `yarn@1.`, or a Yarn 2+ lockfile (`__metadata:`). Without a
/// `yarn.lock` only `cwd` itself is looked at.
fn yarn2_project(cwd: &Path) -> Option<String> {
    let root = lockfile_root(cwd);
    for d in cwd.ancestors() {
        if d.join(".yarnrc.yml").exists() {
            return Some(format!("{} has a .yarnrc.yml (a Yarn 2+ project)", d.display()));
        }
        if root.as_deref().is_none_or(|r| d == r) {
            break;
        }
    }
    let dirs = [Some(cwd), root.as_deref()];
    for d in dirs.into_iter().flatten() {
        if let Some(pm) = package_manager(d) {
            if pm.starts_with("yarn@") && !pm.starts_with("yarn@1.") {
                return Some(format!("packageManager is {pm} (a Yarn 2+ project)"));
            }
        }
    }
    let lock = std::fs::read_to_string(root?.join("yarn.lock")).ok()?;
    lock.lines().any(|l| l.starts_with("__metadata:")).then(|| "yarn.lock is a Yarn 2+ lockfile".into())
}

/// The `packageManager` field of `dir/package.json`.
fn package_manager(dir: &Path) -> Option<String> {
    let text = std::fs::read(dir.join("package.json")).ok()?;
    serde_json::from_slice::<Value>(&text).ok().and_then(|v| v["packageManager"].as_str().map(String::from))
}

/// Yarn's lockfile folder: the nearest directory at or above `cwd` that holds
/// `yarn.lock` (with workspaces, the workspace root).
fn lockfile_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| d.join("yarn.lock").is_file()).map(Path::to_path_buf)
}

/// A `.yarnrc` line that sets `yarn-path` (`yarn-path "…"`, `"yarn-path" …`).
fn sets_yarn_path(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim_start();
        let key = l.strip_prefix('"').map(|r| r.split('"').next().unwrap_or("")).unwrap_or_else(|| l.split_whitespace().next().unwrap_or(""));
        key == "yarn-path"
    })
}

/// The checks that must hold before Yarn starts (each failure: plain Yarn,
/// nothing changed). Yarn's flags and settings are Yarn's to read; only
/// what moves the install, or makes another program than Yarn 1's bundle
/// run, is refused here.
fn gates(cwd: &Path, args: &[String]) -> std::result::Result<PathBuf, String> {
    for a in args {
        if MOVING_OPTIONS.contains(&option_name(a)) {
            return Err(format!("option {a} moves the install out of this directory"));
        }
    }
    let root = lockfile_root(cwd).ok_or("no yarn.lock in this directory or above")?;
    // A Yarn 2+ project never gets here (`yarn2_project`, before anything
    // else); another package manager's project does.
    for d in [cwd, root.as_path()] {
        if let Some(pm) = package_manager(d) {
            if !pm.starts_with("yarn@1.") {
                return Err(format!("packageManager is {pm}"));
            }
        }
    }
    // Yarn 1 re-runs itself as another file in a child process the hook is
    // not in.
    let mut rcs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".yarnrc")).collect();
    rcs.push(crate::home().join(".yarnrc"));
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--use-yarnrc" {
            if let Some(v) = it.next() {
                rcs.push(cwd.join(v));
            }
        } else if let Some(v) = a.strip_prefix("--use-yarnrc=") {
            rcs.push(cwd.join(v));
        }
    }
    for rc in rcs {
        if std::fs::read_to_string(&rc).is_ok_and(|t| sets_yarn_path(&t)) {
            return Err(format!("{} sets yarn-path (Yarn would run another file)", rc.display()));
        }
    }
    if std::env::var_os("YARN_YARN_PATH").is_some() {
        return Err("YARN_YARN_PATH is set (Yarn would run another file)".into());
    }
    if std::fs::symlink_metadata(root.join("node_modules")).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{}/node_modules is a symlink", root.display()));
    }
    trial::in_scope(cwd)?;
    Ok(root)
}

/// What Node's copy keeps here, measured by the hook (src/yarn1-hook.js
/// `probe`). `mtime` (a non-empty copy kept its mtime) is never true here:
/// the hook declines then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    pub empty_atime: bool,
    pub empty_mtime: bool,
    pub atime: bool,
    /// The copy takes the source's owner and group (None: not measurable,
    /// the user is in one group only).
    pub group: Option<bool>,
}

impl Facts {
    fn from_json(v: &Value) -> Option<Facts> {
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
        if b("mtime")? {
            return None;
        }
        Some(Facts { empty_atime: b("empty_atime")?, empty_mtime: b("empty_mtime")?, atime: b("atime")?, group: b("group") })
    }

    /// The times a clone of a file of `len` bytes keeps (atime, mtime).
    fn kept(&self, len: u64) -> (bool, bool) {
        if len == 0 { (self.empty_atime, self.empty_mtime) } else { (self.atime, false) }
    }
}

/// Where Yarn's cache folder stands for this run.
#[derive(Debug, Clone)]
enum Cache {
    /// `yarn cache dir` has not answered yet.
    Pending,
    Known(PathBuf),
    /// No clone can be made from it (why).
    Unusable(String),
}

#[derive(Debug)]
enum Outcome {
    Cloned { bytes: u64 },
    Copy { reason: String, fatal: bool, message: Option<String> },
}

fn copy(reason: impl Into<String>) -> Outcome {
    Outcome::Copy { reason: reason.into(), fatal: false, message: None }
}

/// What a run did, for the log and the summary line.
#[derive(Debug, Default)]
pub struct Stats {
    pub hello: Option<Value>,
    /// The hello of the Yarn a workspace command started (its hook's).
    pub child_hello: Option<Value>,
    /// Why that Yarn's requests were left to its Node: its hook measured
    /// other facts than the parent's (`Server::record_message`).
    pub child_facts_differ: Option<String>,
    pub declined: Option<String>,
    pub cloned: u64,
    pub cloned_bytes: u64,
    /// Files Node copied, by reason (the server's refusals and the hook's
    /// own counts).
    pub copied: BTreeMap<String, u64>,
    pub link_steps: u64,
    pub link_step_ends: u64,
    pub cache: Option<String>,
    /// When `yarn cache dir` answered (or failed), seconds after the start.
    pub cache_secs: Option<f64>,
    pub errors: Vec<String>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    pub fn copied_total(&self) -> u64 {
        self.copied.values().sum()
    }

    pub fn to_json(&self) -> Value {
        let wall = match (self.first, self.last) {
            (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
            _ => 0.0,
        };
        json!({
            "cloned": self.cloned,
            "cloned_bytes": self.cloned_bytes,
            "copied": self.copied,
            "copied_total": self.copied_total(),
            "link_steps": self.link_steps,
            "yarn": self.hello.as_ref().and_then(|h| h.get("yarn")).cloned().unwrap_or(Value::Null),
            // The Yarn and the node that ran (under corepack, the Yarn it
            // chose): what scripts/verify-sample.sh --yarn replays with.
            "yarn_js": self.hello.as_ref().and_then(|h| h.get("yarn_js")).cloned().unwrap_or(Value::Null),
            "node": self.hello.as_ref().and_then(|h| h.get("node")).cloned().unwrap_or(Value::Null),
            "node_path": self.hello.as_ref().and_then(|h| h.get("node_path")).cloned().unwrap_or(Value::Null),
            "first": self.hello.as_ref().and_then(|h| h.get("first")).cloned().unwrap_or(Value::Null),
            "facts": self.hello.as_ref().and_then(|h| h.get("facts")).cloned().unwrap_or(Value::Null),
            "cache": self.cache,
            "cache_secs": self.cache_secs,
            // Set when Yarn ran: the hook in the Yarn a workspace command
            // started declined, and that Yarn ran without clones.
            "declined": self.declined,
            "child_facts_differ": self.child_facts_differ,
            "serve_wall_secs": (wall * 100.0).round() / 100.0,
            "errors": self.errors,
        })
    }
}

/// Test switches, read once from the environment (scripts/test-yarn1.sh).
#[derive(Debug, Default, Clone, Copy)]
struct TestSwitches {
    /// Refuse every Nth clone request without cloning (EIO).
    fail_every: u64,
    /// Make every Nth clone, then fail its fix-up.
    fail_fixup_every: u64,
    /// Sleep this long between every Nth clone and its fix-up (a slow
    /// answer: the hook must wait for it, not copy over it).
    slow_fixup: Option<(u64, Duration)>,
    /// Say the probe's copy shares blocks with its source.
    probe_shares: bool,
}

impl TestSwitches {
    fn from_env() -> TestSwitches {
        let n = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        TestSwitches {
            fail_every: n("MATCHAPACK_YARN_FAIL_EVERY"),
            fail_fixup_every: n("MATCHAPACK_YARN_FAIL_FIXUP_EVERY"),
            slow_fixup: match (n("MATCHAPACK_YARN_SLOW_FIXUP_EVERY"), n("MATCHAPACK_YARN_SLOW_FIXUP_MS")) {
                (0, _) | (_, 0) => None,
                (every, ms) => Some((every, Duration::from_millis(ms))),
            },
            probe_shares: std::env::var("MATCHAPACK_YARN_SIMULATE").as_deref() == Ok("probe-shares"),
        }
    }
}

pub struct Server {
    /// The project root (Yarn's lockfile folder), canonical.
    root: PathBuf,
    root_dev: u64,
    cache: Mutex<Cache>,
    cache_known: std::sync::Condvar,
    cache_wait: Duration,
    started: Instant,
    facts: OnceLock<Facts>,
    /// Set when the hello of a workspace command's Yarn (its hook's) names
    /// other facts than the parent's.
    child_differs: AtomicBool,
    /// Set once a clone failed because the filesystem pair cannot clone.
    unsupported: AtomicBool,
    stopping: AtomicBool,
    /// Verdicts per parent directory, as given (not resolved).
    dest_dirs: Mutex<HashMap<PathBuf, std::result::Result<(), String>>>,
    src_dirs: Mutex<HashMap<PathBuf, std::result::Result<(), String>>>,
    test: TestSwitches,
    requests: AtomicU64,
    pub stats: Mutex<Stats>,
}

fn plain_absolute(p: &Path) -> bool {
    p.is_absolute() && p.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

impl Server {
    pub fn new(root: PathBuf) -> Result<Server> {
        let root = std::fs::canonicalize(&root).with_context(|| format!("resolving {}", root.display()))?;
        let root_dev = std::fs::metadata(&root)?.dev();
        Ok(Server {
            root,
            root_dev,
            cache: Mutex::new(Cache::Pending),
            cache_known: std::sync::Condvar::new(),
            cache_wait: CACHE_WAIT,
            started: Instant::now(),
            facts: OnceLock::new(),
            child_differs: AtomicBool::new(false),
            unsupported: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            dest_dirs: Mutex::new(HashMap::new()),
            src_dirs: Mutex::new(HashMap::new()),
            test: TestSwitches::from_env(),
            requests: AtomicU64::new(0),
            stats: Mutex::new(Stats::default()),
        })
    }

    /// `yarn workspace <name> <command> …`: the directory the Yarn the
    /// workspace command started runs in, from its hook's hello; waits for it
    /// as `chosen_yarn` does. A directory of the project only (its
    /// destinations would be refused anywhere else).
    fn child_dir(&self, install_ended: &AtomicBool) -> std::result::Result<PathBuf, String> {
        loop {
            let named = self.stats.lock().unwrap().child_hello.as_ref().map(|h| h["cwd"].as_str().map(PathBuf::from));
            match named {
                Some(Some(d)) => {
                    let real = std::fs::canonicalize(&d).map_err(|e| format!("the workspace's Yarn runs in {}: {e}", d.display()))?;
                    if !plain_absolute(&d) || !real.starts_with(&self.root) {
                        return Err(format!("the workspace's Yarn runs in {}, outside the project", d.display()));
                    }
                    return Ok(d);
                }
                Some(None) => return Err("the workspace's hook names no directory".into()),
                None if install_ended.load(Ordering::Relaxed) => return Err("the install ended first".into()),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// Under corepack: the `bin/yarn.js` corepack chose, as the hook's hello
    /// names it (checked to be Yarn 1's npm package), once it has arrived;
    /// an error when the install ends first (corepack failed, or the hook
    /// declined).
    fn chosen_yarn(&self, install_ended: &AtomicBool) -> std::result::Result<PathBuf, String> {
        loop {
            let named = self.stats.lock().unwrap().hello.as_ref().map(|h| h["yarn_js"].as_str().map(PathBuf::from));
            match named {
                Some(Some(js)) => {
                    yarn1_package(&js)?;
                    return Ok(js);
                }
                Some(None) => return Err("the hook's hello names no yarn.js".into()),
                None if install_ended.load(Ordering::Relaxed) => return Err("the install ended first".into()),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// Yarn's cache folder as `yarn cache dir` printed it, or why it is not
    /// known. A folder on another volume than the project cannot be cloned
    /// from.
    pub fn set_cache(&self, dir: std::result::Result<PathBuf, String>) {
        let state = match dir.and_then(|d| std::fs::canonicalize(&d).map_err(|e| format!("resolving {}: {e}", d.display()))) {
            Ok(d) => match std::fs::metadata(&d) {
                Ok(m) if m.dev() != self.root_dev => Cache::Unusable(format!("Yarn's cache {} is on another volume than the project", d.display())),
                Ok(_) => Cache::Known(d),
                Err(e) => Cache::Unusable(format!("{}: {e}", d.display())),
            },
            Err(e) => Cache::Unusable(e),
        };
        {
            let mut st = self.stats.lock().unwrap();
            st.cache = Some(match &state {
                Cache::Known(d) => d.display().to_string(),
                Cache::Unusable(why) => format!("unusable: {why}"),
                Cache::Pending => "pending".into(),
            });
            st.cache_secs = Some((self.started.elapsed().as_secs_f64() * 100.0).round() / 100.0);
        }
        *self.cache.lock().unwrap() = state;
        self.cache_known.notify_all();
    }

    /// `dest`'s parent must resolve inside the project root, below one of its
    /// node_modules.
    fn check_dest(&self, dest: &Path) -> std::result::Result<(), String> {
        if !plain_absolute(dest) {
            return Err("the destination is not a plain absolute path".into());
        }
        let parent = dest.parent().ok_or("the destination has no parent")?;
        if let Some(v) = self.dest_dirs.lock().unwrap().get(parent) {
            return v.clone();
        }
        let v = (|| {
            let real = std::fs::canonicalize(parent).map_err(|e| format!("resolving {}: {e}", parent.display()))?;
            let rel = real.strip_prefix(&self.root).map_err(|_| format!("{} resolves outside the project ({})", parent.display(), real.display()))?;
            if !rel.components().any(|c| c.as_os_str() == "node_modules") {
                return Err(format!("{} is not inside a node_modules of the project", real.display()));
            }
            Ok(())
        })();
        self.dest_dirs.lock().unwrap().insert(parent.to_path_buf(), v.clone());
        v
    }

    /// `src` must be a regular file whose parent resolves inside `cache`.
    fn check_src(&self, src: &Path, cache: &Path) -> std::result::Result<std::fs::Metadata, String> {
        if !plain_absolute(src) {
            return Err("the source is not a plain absolute path".into());
        }
        let parent = src.parent().ok_or("the source has no parent")?;
        let known = self.src_dirs.lock().unwrap().get(parent).cloned();
        let v = match known {
            Some(v) => v,
            None => {
                let v = match std::fs::canonicalize(parent) {
                    Ok(real) if real.starts_with(cache) => Ok(()),
                    Ok(real) => Err(format!("{} resolves outside Yarn's cache ({})", parent.display(), real.display())),
                    Err(e) => Err(format!("resolving {}: {e}", parent.display())),
                };
                self.src_dirs.lock().unwrap().insert(parent.to_path_buf(), v.clone());
                v
            }
        };
        v?;
        match std::fs::symlink_metadata(src) {
            Ok(m) if m.file_type().is_file() => Ok(m),
            Ok(_) => Err(format!("{} is not a regular file", src.display())),
            Err(e) => Err(format!("{}: {e}", src.display())),
        }
    }

    fn clone_file(&self, src: &Path, dest: &Path) -> Outcome {
        let Some(facts) = self.facts.get().copied() else { return copy("no-hello") };
        // A workspace command's Yarn whose hook measured other facts: a clone
        // made to the parent's facts would not look like its Node's copy. In
        // a workspace command the parent Yarn links nothing (it starts that
        // Yarn and waits), so every request after the child's hello is the
        // child's; fatal: its hook stops asking.
        if self.child_differs.load(Ordering::Relaxed) {
            return Outcome::Copy { reason: "child-facts-differ".into(), fatal: true, message: None };
        }
        let cache = {
            let g = self.cache.lock().unwrap();
            // Until it is known, the deadline has passed, or the node child is
            // gone (`stop`: nobody is left to answer).
            let left = (self.started + self.cache_wait).saturating_duration_since(Instant::now());
            let (g, _) = self.cache_known.wait_timeout_while(g, left, |c| matches!(c, Cache::Pending) && !self.stopping.load(Ordering::Relaxed)).unwrap();
            g.clone()
        };
        let cache = match cache {
            Cache::Known(d) => d,
            Cache::Pending => return copy("cache-dir-pending"),
            Cache::Unusable(why) => return Outcome::Copy { reason: "cache-unusable".into(), fatal: true, message: Some(why) },
        };
        if self.unsupported.load(Ordering::Relaxed) {
            return Outcome::Copy { reason: "clone-unsupported".into(), fatal: true, message: None };
        }
        if let Err(e) = self.check_dest(dest) {
            return Outcome::Copy { reason: "rejected-dest".into(), fatal: false, message: Some(e) };
        }
        let meta = match self.check_src(src, &cache) {
            Ok(m) => m,
            Err(e) => return Outcome::Copy { reason: "rejected-src".into(), fatal: false, message: Some(e) },
        };
        // BSD file flags (compressed, hidden, immutable…) would be carried by
        // a clone, never by a copy.
        if sys::flags(&meta) != 0 {
            return copy("src-flags");
        }
        // clonefile(2) clears setuid and setgid, which Node's copy keeps
        // (libuv fchmods the source's mode): let Node copy such files.
        if std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o7000 != 0 {
            return copy("src-special-mode");
        }
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        if self.test.fail_every > 0 && n % self.test.fail_every == 0 {
            return copy("test-refused");
        }
        if let Err(e) = sys::clone(src, dest) {
            let no = errno(&e);
            if no == libc::EXDEV || no == libc::ENOTSUP || no == libc::EOPNOTSUPP || no == libc::ENOSYS {
                self.unsupported.store(true, Ordering::Relaxed);
                return Outcome::Copy { reason: "clone-unsupported".into(), fatal: true, message: Some(format!("clonefile {}: {e}", dest.display())) };
            }
            return Outcome::Copy { reason: format!("clone-errno-{no}"), fatal: false, message: Some(format!("clonefile {}: {e}", dest.display())) };
        }
        if let Some((every, pause)) = self.test.slow_fixup {
            if n % every == 0 {
                std::thread::sleep(pause);
            }
        }
        // (The test switch fails the fix-up before it starts: the clone still
        // has the cache file's xattrs and group, which a copy made over it
        // would keep.)
        let fixed = if self.test.fail_fixup_every > 0 && n % self.test.fail_fixup_every == 0 {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            fix_up(dest, &meta, facts)
        };
        if let Err(e) = fixed {
            // Not a copy's result: remove it; Node's copy makes the file.
            let _ = std::fs::remove_file(dest);
            return Outcome::Copy { reason: "fixup-failed".into(), fatal: false, message: Some(format!("{}: {e}", dest.display())) };
        }
        Outcome::Cloned { bytes: meta.len() }
    }

    fn tally(&self, o: &Outcome) {
        let mut s = self.stats.lock().unwrap();
        s.last = Some(Instant::now());
        match o {
            Outcome::Cloned { bytes } => {
                s.cloned += 1;
                s.cloned_bytes += bytes;
            }
            Outcome::Copy { reason, message, .. } => {
                *s.copied.entry(reason.clone()).or_default() += 1;
                if let Some(m) = message {
                    if s.errors.len() < 5 {
                        s.errors.push(format!("{reason}: {m}"));
                    }
                }
            }
        }
    }

    /// A message that needs no answer. Returns false when `v` is something else.
    fn record_message(&self, v: &Value) -> bool {
        if v.get("hello").is_some() {
            let facts = v.get("facts").and_then(Facts::from_json);
            let mut s = self.stats.lock().unwrap();
            if v.get("child") == Some(&Value::Bool(true)) {
                // The clones are made to the parent's facts (the only ones
                // kept); the child's own Node measured its own. Compared, not
                // assumed: on a mismatch (or no facts) its files are its
                // Node's copies.
                let parent = self.facts.get().copied();
                if facts.is_none() || facts != parent {
                    self.child_differs.store(true, Ordering::Relaxed);
                    let shown = |h: Option<&Value>| h.and_then(|h| h.get("facts")).map_or("none".to_string(), |f| f.to_string());
                    s.child_facts_differ = Some(format!(
                        "the workspace's Yarn measured other facts than the command's Yarn ({} against {}): its files are its Node's copies",
                        shown(Some(v)),
                        shown(s.hello.as_ref())
                    ));
                }
                s.child_hello = Some(v.clone());
            } else {
                if let Some(f) = facts {
                    let _ = self.facts.set(f);
                }
                s.hello = Some(v.clone());
            }
            return true;
        }
        if let Some(why) = v.get("declined").and_then(|d| d.as_str()) {
            self.stats.lock().unwrap().declined = Some(why.chars().take(300).collect());
            return true;
        }
        if let Some(step) = v.get("link_step").and_then(|s| s.as_str()) {
            let mut s = self.stats.lock().unwrap();
            if step == "start" {
                s.link_steps += 1;
            } else {
                s.link_step_ends += 1;
                if let Some(m) = v.get("counts").and_then(|c| c.as_object()) {
                    for (k, n) in m {
                        // A short word chosen by the hook; keep the log sane.
                        let key: String = k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
                        *s.copied.entry(key).or_default() += n.as_u64().unwrap_or(0);
                    }
                }
            }
            return true;
        }
        false
    }

    fn handle_line(&self, line: &str) -> Vec<String> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let Some(id) = v.get("id").cloned() else { return vec![] };
        if let Some(p) = v.get("probe").and_then(|p| p.as_array()) {
            let path = |i: usize| p.get(i).and_then(|x| x.as_str()).map(PathBuf::from);
            let shares = self.test.probe_shares || matches!((path(0), path(1)), (Some(a), Some(b)) if sys::shares_blocks(&a, &b));
            return vec![json!({ "id": id, "shares": shares }).to_string()];
        }
        self.stats.lock().unwrap().first.get_or_insert_with(Instant::now);
        let str_of = |k: &str| v.get(k).and_then(|x| x.as_str());
        let outcome = match (str_of("src"), str_of("dest")) {
            (Some(src), Some(dest)) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.clone_file(Path::new(src), Path::new(dest))))
                .unwrap_or_else(|_| copy("internal-error")),
            _ => copy("bad-request"),
        };
        self.tally(&outcome);
        let answer = match outcome {
            Outcome::Cloned { .. } => json!({ "id": id, "ok": true }),
            Outcome::Copy { reason, fatal, .. } => json!({ "id": id, "ok": false, "reason": reason, "fatal": fatal }),
        };
        vec![answer.to_string()]
    }

    /// The node child is gone: queued requests are dropped, and a worker
    /// waiting for Yarn's cache folder stops waiting (the lock is taken so
    /// that a worker between its check and its wait is not missed).
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _g = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        self.cache_known.notify_all();
    }

    pub fn serve(&self, stream: UnixStream, threads: usize) -> Result<()> {
        serve_lines(stream, threads, &self.stopping, &|v| self.record_message(v), &|l| self.handle_line(l))
    }
}

/// Make the clone at `dest` look like the copy Node's `fs.copyFile` leaves
/// (module comment). `src_meta`: the cache file's lstat.
fn fix_up(dest: &Path, src_meta: &std::fs::Metadata, facts: Facts) -> std::io::Result<()> {
    if facts.group == Some(true) {
        let m = std::fs::symlink_metadata(dest)?;
        if m.uid() != src_meta.uid() || m.gid() != src_meta.gid() {
            // As libuv's fchown(dst, src uid, src gid): the result is ignored
            // (a group the user is not in stays the directory's).
            sys::lchown(dest, src_meta.uid(), src_meta.gid());
        }
    }
    for name in crate::diff::xattr_names(dest)? {
        if name != "com.apple.provenance" {
            sys::remove_xattr(dest, &name)?;
        }
    }
    let (atime, mtime) = facts.kept(src_meta.len());
    if !(atime && mtime) {
        sys::set_times_now(dest, !atime, !mtime)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    const CLONE_NOFOLLOW: u32 = 0x0001;
    /// The clone is owned by the caller, its group the directory's (as a
    /// file Node creates).
    const CLONE_NOOWNERCOPY: u32 = 0x0002;

    fn c(p: &Path) -> io::Result<CString> {
        Ok(CString::new(p.as_os_str().as_bytes())?)
    }

    pub fn clone(src: &Path, dst: &Path) -> io::Result<()> {
        let (s, d) = (c(src)?, c(dst)?);
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn flags(m: &std::fs::Metadata) -> u32 {
        use std::os::macos::fs::MetadataExt;
        m.st_flags()
    }

    pub fn lchown(p: &Path, uid: u32, gid: u32) {
        if let Ok(cp) = c(p) {
            unsafe { libc::lchown(cp.as_ptr(), uid, gid) };
        }
    }

    pub fn remove_xattr(p: &Path, name: &str) -> io::Result<()> {
        let (cp, n) = (c(p)?, CString::new(name)?);
        if unsafe { libc::removexattr(cp.as_ptr(), n.as_ptr(), libc::XATTR_NOFOLLOW) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn set_times_now(p: &Path, atime: bool, mtime: bool) -> io::Result<()> {
        let t = |now: bool| libc::timespec { tv_sec: 0, tv_nsec: if now { libc::UTIME_NOW } else { libc::UTIME_OMIT } };
        let ts = [t(atime), t(mtime)];
        let cp = c(p)?;
        if unsafe { libc::utimensat(libc::AT_FDCWD, cp.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// The device offset of a file's first block (F_LOG2PHYS_EXT).
    fn first_block(p: &Path) -> io::Result<i64> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(p)?;
        let mut l = libc::log2phys { l2p_flags: 0, l2p_contigbytes: 1, l2p_devoffset: 0 };
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut l) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(l.l2p_devoffset)
    }

    /// The two files share their first block (one is a clone of the other).
    pub fn shares_blocks(a: &Path, b: &Path) -> bool {
        let same_dev = matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if x.dev() == y.dev());
        same_dev && matches!((first_block(a), first_block(b)), (Ok(x), Ok(y)) if x == y)
    }
}

/// Elsewhere the engine never runs (plain Yarn already clones on Linux: its
/// libuv uses FICLONE); this keeps the module building and testable.
#[cfg(not(target_os = "macos"))]
mod sys {
    use std::io;
    use std::path::Path;

    pub fn clone(_src: &Path, _dst: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    pub fn flags(_m: &std::fs::Metadata) -> u32 {
        0
    }

    pub fn lchown(_p: &Path, _uid: u32, _gid: u32) {}

    pub fn remove_xattr(_p: &Path, _name: &str) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    pub fn set_times_now(_p: &Path, _atime: bool, _mtime: bool) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    pub fn shares_blocks(_a: &Path, _b: &Path) -> bool {
        false
    }
}

/// How a `matchapack yarn` install ended when Yarn did not succeed.
enum RunError {
    /// Refused before Yarn started: plain Yarn can take over as is.
    Declined(String),
    /// Yarn ran and exited with this status, having said why itself.
    Yarn { code: i32, stats: Value },
}

pub struct Summary {
    pub secs: f64,
    pub stats: Value,
    pub unseen: bool,
}

/// What a replay needs from the arguments, logged as `{"kind", "args"}`:
/// for a full install (`kind` "install") the arguments after `install`
/// (`replay_args`); for another command (`kind` "command": `add`, `remove`,
/// `workspace <name> add`, …) the whole command line, the command word
/// included, without the cache folder options. A replay gives the cache
/// folder the run used itself. Under the key `args`, so trial.rs redacts
/// them as command-line words (a secret option's value, credentials in a
/// URL) like the record's own `args`.
fn replay(args: &[String]) -> Value {
    if command(args).is_empty() || command(args) == "install" {
        json!({ "kind": "install", "args": replay_args(args) })
    } else {
        json!({ "kind": "command", "args": without_cache_folder(args, None) })
    }
}

fn now_mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok()
}

/// How long `yarn cache dir` may take (it took 0.2–0.45 s in the review),
/// counted from its start. After that it is killed, and nothing in this
/// install is cloned.
const CACHE_DIR_DEADLINE: Duration = Duration::from_secs(15);

/// How long a killed `yarn cache dir` is given to go before it is left
/// behind (SIGKILL cannot be refused, but a process stuck in the kernel is
/// reaped only once it returns; waiting for it would hang the install).
const REAP_WAIT: Duration = Duration::from_secs(2);

/// A pipe read on a thread of its own: what was read so far, and a message
/// when the pipe is at its end.
type Drain = (std::sync::Arc<Mutex<Vec<u8>>>, std::sync::mpsc::Receiver<()>);

/// `yarn cache dir`, started (`start_yarn_cache_dir`): its process and the
/// threads reading its pipes.
struct CacheDirAsk {
    child: std::process::Child,
    out: Drain,
    err: Drain,
    started: Instant,
}

/// Start `yarn cache dir` as the user's Yarn answers it for this install
/// (the same node, yarn.js, working directory, environment and cache
/// arguments).
///
/// Call it on the thread that starts the install's node, before that node is
/// started, never in parallel with it (under corepack: once the hook's hello
/// has come, when that spawn has finished; `run` says why that holds). On macOS a pipe is made and only then
/// marked close-on-exec (two calls: there is no pipe2), so a process forked
/// on another thread in between inherits the pipe and keeps it through its
/// exec. With both spawns racing (as until 2026-11-13), the install's node
/// could hold the write end of this pipe, whose reader then saw the end only
/// when the install ended: the cache folder stayed unknown for the whole
/// install (CI run 43306715867; NOTES "The cause"). The other way round,
/// `yarn cache dir` could hold the pipe on which the install's node reports
/// its exec, and the install started only when `yarn cache dir` ended.
fn start_yarn_cache_dir(node: &util::NodeStart, yarn_js: &Path, env: &[(String, String)], cwd: &Path, args: &[String]) -> std::result::Result<CacheDirAsk, String> {
    use std::io::Read;
    let started = Instant::now();
    let mut child = node
        .command()
        .arg(yarn_js)
        .arg("cache")
        .arg("dir")
        .args(cache_args(args))
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("yarn cache dir: {e}"))?;
    // Read on threads, so a full pipe cannot stall the child while it is
    // waited for; collected with a bound (`collect`, PIPE_WAIT), so a process
    // that inherited the pipe (and outlives the child) cannot stall this one.
    // The threads are not joined: one whose pipe stays open ends with this
    // process.
    fn drain(r: Option<impl Read + Send + 'static>) -> Drain {
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel();
        let b = buf.clone();
        std::thread::spawn(move || {
            if let Some(mut o) = r {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = o.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    b.lock().unwrap().extend_from_slice(&chunk[..n]);
                }
            }
            let _ = tx.send(());
        });
        (buf, rx)
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    Ok(CacheDirAsk { child, out, err, started })
}

/// How long a pipe of `yarn cache dir` is read after its process ended (a
/// process that inherited the pipe keeps it open).
const PIPE_WAIT: Duration = Duration::from_secs(2);

/// What was read until the pipe closed, until PIPE_WAIT after the child
/// ended, or until `cancel` is set (the install has ended).
fn collect((buf, done): Drain, cancel: &AtomicBool) -> String {
    use std::sync::mpsc::RecvTimeoutError::Timeout;
    let t = Instant::now();
    while t.elapsed() < PIPE_WAIT && !cancel.load(Ordering::Relaxed) && done.recv_timeout(Duration::from_millis(10)) == Err(Timeout) {}
    String::from_utf8_lossy(&buf.lock().unwrap()).into_owned()
}

impl CacheDirAsk {
    /// Kill the process and give it REAP_WAIT to go.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let t = Instant::now();
        while t.elapsed() < REAP_WAIT && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The folder `yarn cache dir` printed. The process is killed `deadline`
    /// after its start, or as soon as `cancel` is set (the install has
    /// ended: the answer is no longer needed). No wait here is unbounded:
    /// the process is polled; a killed one is given REAP_WAIT; a pipe is
    /// read at most PIPE_WAIT after the process ended, and no longer once
    /// the install has ended (the end of an install waits for this).
    fn answer(mut self, deadline: Duration, cancel: &AtomicBool) -> std::result::Result<PathBuf, String> {
        let last_line = |text: &str| text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) if cancel.load(Ordering::Relaxed) => {
                    self.kill();
                    return Err("the install ended first".to_string());
                }
                Ok(None) if self.started.elapsed() > deadline => {
                    self.kill();
                    let said = last_line(&collect(self.err, cancel));
                    let e = format!("yarn cache dir did not answer in {}s", deadline.as_secs());
                    return Err(if said.is_empty() { e } else { format!("{e} (it said: {said})") });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    self.kill();
                    return Err(format!("yarn cache dir: {e}"));
                }
            }
        };
        let text = collect(self.out, cancel);
        if !status.success() {
            let said = last_line(&collect(self.err, cancel));
            return Err(format!("yarn cache dir exited with {status}{}", if said.is_empty() { String::new() } else { format!(": {said}") }));
        }
        let line = last_line(&text);
        if !Path::new(&line).is_absolute() {
            // (Its pipe may have been left before its line was read.)
            let ended = cancel.load(Ordering::Relaxed);
            return Err(if ended { "the install ended first".to_string() } else { format!("yarn cache dir printed {line:?}") });
        }
        Ok(PathBuf::from(line))
    }
}

fn run(cwd: &Path, root: &Path, yarn: &YarnBin, args: &[String]) -> std::result::Result<Summary, RunError> {
    let t0 = Instant::now();
    let node = yarn.node.clone();
    let prepared = (|| -> Result<_> {
        let run_dir = util::private_run_dir()?;
        let helper = run_dir.join("yarn1-hook.js");
        let _ = std::fs::remove_file(&helper);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&helper)?;
            f.write_all(include_str!("yarn1-hook.js").as_bytes())?;
        }
        let (ours, theirs) = UnixStream::pair().context("creating the socket for the clone requests")?;
        Ok((run_dir, helper, ours, theirs))
    })();
    let (run_dir, helper, ours, theirs) = prepared.map_err(|e| RunError::Declined(format!("{e:#}")))?;
    let server = match Server::new(root.to_path_buf()) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&helper);
            return Err(RunError::Declined(format!("{e:#}")));
        }
    };
    let integrity = root.join("node_modules/.yarn-integrity");
    let integrity_before = now_mtime(&integrity);

    // Not util::node_cmd: Yarn runs here as the user's `yarn`, and the
    // scripts it starts see the user's environment.
    let mut cmd = node.command();
    cmd.arg(&helper)
        .arg(&yarn.js)
        .args(args)
        .current_dir(cwd)
        .envs(yarn.env.iter().map(|(k, v)| (k, v)))
        .env("MATCHAPACK_YARN_FD", crate::inject::CHILD_FD.to_string())
        .env("MATCHAPACK_YARN_PROBE_DIR", &run_dir)
        .env_remove("MATCHAPACK_YARN_FAIL_EVERY")
        .env_remove("MATCHAPACK_YARN_FAIL_FIXUP_EVERY")
        .env_remove("MATCHAPACK_YARN_SLOW_FIXUP_EVERY")
        .env_remove("MATCHAPACK_YARN_SLOW_FIXUP_MS");
    if yarn.corepack {
        cmd.env("MATCHAPACK_YARN_LAUNCHER", "corepack");
    } else {
        cmd.env_remove("MATCHAPACK_YARN_LAUNCHER");
    }
    // `yarn workspace <name> add …`: the hook starts the Yarn the workspace
    // command starts with itself, and passes its requests on (src/yarn1-hook.js).
    if workspace_args(args).is_some() {
        cmd.env("MATCHAPACK_YARN_WRAP_CHILD", "workspace");
    } else {
        cmd.env_remove("MATCHAPACK_YARN_WRAP_CHILD");
    }
    cmd.env_remove("MATCHAPACK_YARN_CHILD");
    let deadline = std::env::var("MATCHAPACK_YARN_CACHE_DIR_SECS").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs_f64).unwrap_or(CACHE_DIR_DEADLINE);
    let install_ended = AtomicBool::new(false);
    // Yarn's cache folder, asked of the same Yarn: started here, before the
    // install's node and not beside it (start_yarn_cache_dir says why), and
    // answered in parallel with the install: requests wait for it
    // (CACHE_WAIT).
    //
    // Under corepack the Yarn is known only once corepack has chosen it, in
    // the install's node (the hello names it; running the launcher a second
    // time could resolve, download or prompt again). Then `yarn cache dir`
    // is started on the thread below, after the hello, and it is still the
    // only spawn in flight: the hello comes from the program the install's
    // node exec'd, so that spawn (fork, exec, its close-on-exec error pipe)
    // has finished, and nothing else in this process forks during an
    // install or makes a descriptor without close-on-exec in one call (the
    // workers clone, open with O_CLOEXEC, set times, groups and xattrs; the
    // socket and its clone were made before the install's node). NOTES
    // "Yarn 1 shell route", decision 4.
    //
    // For `yarn workspace <name> <command> …` the Yarn that links is the one
    // the workspace command starts, in the workspace's directory, with the
    // arguments after the name only (Yarn passes it nothing of its own): its
    // cache options are those, resolved in that directory (a relative
    // `--cache-folder`, a workspace's `.yarnrc`). That directory is known
    // from that Yarn's hello, so `yarn cache dir` is started after it, as
    // under corepack and for the same reason it is then the only spawn here.
    let workspace = workspace_args(args).is_some();
    let cache_from = workspace_args(args).map_or(args, |w| &w[1..]);
    let ask = if yarn.corepack || workspace { None } else { Some(start_yarn_cache_dir(&node, &yarn.js, &yarn.env, cwd, cache_from)) };
    let status = std::thread::scope(|s| {
        s.spawn(|| {
            let ask = match ask {
                Some(a) => a,
                None => {
                    let js = if yarn.corepack { server.chosen_yarn(&install_ended) } else { Ok(yarn.js.clone()) };
                    let dir = if workspace { server.child_dir(&install_ended) } else { Ok(cwd.to_path_buf()) };
                    js.and_then(|js| dir.and_then(|d| start_yarn_cache_dir(&node, &js, &yarn.env, &d, cache_from)))
                }
            };
            server.set_cache(ask.and_then(|a| a.answer(deadline, &install_ended)))
        });
        let serve = |sock| {
            let _ = server.serve(sock, crate::inject::threads());
        };
        let st = crate::inject::run_serving(&mut cmd, ours, theirs, serve, || server.stop());
        install_ended.store(true, Ordering::Relaxed);
        st
    });
    let _ = std::fs::remove_file(&helper);
    let status = match status {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(RunError::Yarn { code: 1, stats: json!({ "error": format!("waiting for node: {e}") }) });
        }
        Err(e) => return Err(RunError::Declined(format!("could not start node: {e}"))),
    };
    let stats = std::mem::take(&mut *server.stats.lock().unwrap());
    if stats.hello.is_none() && status.code() == Some(crate::inject::HELPER_DECLINED) {
        let why = stats.declined.clone().unwrap_or_else(|| "the hook could not start Yarn".into());
        return Err(RunError::Declined(why));
    }
    let mut out = stats.to_json();
    // Yarn wrote .yarn-integrity in this run (it linked), but the hook saw
    // no link step: the hook no longer fits this Yarn, or another Yarn ran.
    let linked = now_mtime(&integrity).is_some_and(|t| Some(t) != integrity_before);
    // (A workspace command's Yarn whose hook declined ran plain, and said so.)
    let unseen = linked && stats.link_steps == 0 && stats.declined.is_none();
    out["linked"] = json!(linked);
    out["unseen"] = json!(unseen);
    if !status.success() {
        let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        return Err(RunError::Yarn { code, stats: out });
    }
    Ok(Summary { secs: t0.elapsed().as_secs_f64(), stats: out, unseen })
}

/// matchapack will not handle this install and nothing has been changed: run
/// plain Yarn (or exit 3 under MATCHAPACK_NO_FALLBACK=1, leaving the project
/// as it is).
fn decline(args: &[String], yarn: Option<&YarnBin>, reason: &str) -> i32 {
    if std::env::var("MATCHAPACK_NO_FALLBACK").as_deref() == Ok("1") {
        eprintln!("matchapack: not handled, left unchanged ({reason})");
        return 3;
    }
    eprintln!("matchapack: running plain yarn instead ({reason})");
    exec_yarn(args, yarn)
}

/// Replace this process with plain Yarn: the yarn.js matchapack would have
/// run (`yarn`, with the environment its launcher sets), else
/// MATCHAPACK_YARN_JS, else the `yarn` on PATH, the same order `find_yarn_js`
/// uses, so an install and any other command (or the `MATCHAPACK=0` bypass)
/// run the same Yarn. `MATCHAPACK=0` keeps any wrapper from routing back
/// here.
fn exec_yarn(args: &[String], yarn: Option<&YarnBin>) -> i32 {
    util::remove_run_dir();
    let chosen = yarn.map(|y| y.js.clone()).or_else(|| std::env::var_os("MATCHAPACK_YARN_JS").map(PathBuf::from));
    let mut cmd = match chosen.as_deref() {
        Some(js) => {
            let mut c = yarn.map(|y| y.node.command()).unwrap_or_else(|| std::process::Command::new("node"));
            c.arg(js).envs(yarn.iter().flat_map(|y| y.env.iter().map(|(k, v)| (k, v))));
            c
        }
        None => std::process::Command::new("yarn"),
    };
    let err = cmd.args(args).env("MATCHAPACK", "0").exec();
    eprintln!("matchapack: could not start yarn: {err}");
    127
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1048576.0)
}

/// `matchapack yarn <args>`: a bare `yarn`, `yarn install` and the other
/// commands of `engine_command` with clones of Yarn's cache; anything else,
/// and every command a gate refuses, is plain Yarn with the same arguments.
pub fn cli(args: Vec<String>) -> i32 {
    // On Linux plain Yarn already clones (its libuv uses FICLONE).
    let Some(cmd_name) = engine_command(&args).filter(|_| cfg!(target_os = "macos") && std::env::var("MATCHAPACK").as_deref() != Ok("0")) else {
        return exec_yarn(&args, None);
    };
    // Before anything of the install runs (`load_end`: trial::append).
    let load_start = trial::loadavg();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return exec_yarn(&args, None),
    };
    // First, and from files only: a Yarn 2+ project is plain Yarn, unlogged
    // and silent like a command that is not an install (no node start, no
    // yarn.js lookup; NOTES "Yarn 1 shell route").
    if let Some(why) = yarn2_project(&cwd) {
        if std::env::var("MATCHAPACK_NO_FALLBACK").as_deref() == Ok("1") {
            eprintln!("matchapack: not handled, left unchanged ({why})");
            return 3;
        }
        return exec_yarn(&args, None);
    }
    let found = find_yarn_js();
    let plain = found.as_ref().ok().cloned();
    let yarn_version = found.as_ref().map(|y| y.version.clone()).unwrap_or_default();
    let corepack = found.as_ref().is_ok_and(|y| y.corepack);
    let log = |outcome: &str, extra: Value| {
        let mut rec = json!({ "outcome": outcome, "engine": "yarn1", "command": cmd_name, "dir": cwd.display().to_string(), "yarn": yarn_version, "args": args, "load_start": load_start });
        if corepack {
            rec["launcher"] = json!("corepack");
        }
        if let (Value::Object(m), Value::Object(x)) = (&mut rec, extra) {
            m.extend(x);
        }
        trial::append(rec);
    };
    // Under corepack the version is known once it has chosen (the hello).
    let chosen = |stats: &Value| if corepack { json!({ "yarn": stats["yarn"] }) } else { json!({}) };
    let with = |mut a: Value, b: Value| {
        if let (Value::Object(m), Value::Object(x)) = (&mut a, b) {
            m.extend(x);
        }
        a
    };
    let checked = found.and_then(|y| gates(&cwd, &args).map(|root| (y, root)));
    let (yarn, root) = match checked {
        Ok(x) => x,
        Err(reason) => {
            log("declined", json!({ "reason": reason }));
            return decline(&args, plain.as_ref(), &reason);
        }
    };
    util::set_quiet(true);
    // Before Yarn starts: what a later sample needs to replay this run
    // (src/replay.rs).
    let before = replay::before(&root, replay::YARN_LOCKFILES);
    eprintln!("matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&cwd, &root, &yarn, &args)));
    util::remove_run_dir();
    match outcome {
        Ok(Ok(s)) => {
            let n = |k: &str| s.stats[k].as_u64().unwrap_or(0);
            // The Yarn a workspace command started declined: it ran as plain
            // Yarn and copied every file; say that, not "0 files cloned" and
            // a cache note that follows from it.
            if let Some(why) = s.stats["declined"].as_str() {
                eprintln!("matchapack: the workspace's Yarn ran as plain Yarn, without clones ({why})");
                log("installed", with(json!({ "secs": s.secs, "yarn1": s.stats, "root": root.display().to_string(), "node_modules_before": before["node_modules"], "files_before": before["files"], "files_after": replay::files_state(&root, replay::YARN_LOCKFILES), "replay": replay(&args) }), chosen(&s.stats)));
                return 0;
            }
            if let Some(why) = s.stats["child_facts_differ"].as_str() {
                eprintln!("matchapack: {why}");
            }
            if s.stats["linked"] == true || n("cloned") > 0 {
                eprintln!(
                    "matchapack: {} files cloned from Yarn's cache ({}), {} copied by Node, in {:.1}s",
                    n("cloned"),
                    mib(n("cloned_bytes")),
                    n("copied_total"),
                    s.secs
                );
            }
            if let Some(why) = s.stats["cache"].as_str().and_then(|c| c.strip_prefix("unusable: ")) {
                eprintln!("matchapack: no clones in this install: {why}");
            }
            if s.unseen {
                eprintln!(
                    "matchapack: Yarn linked node_modules but matchapack's hook never saw its link step: \
                     this Yarn may run another way (the install is Yarn's own and complete; nothing was cloned)"
                );
            }
            log("installed", with(json!({ "secs": s.secs, "yarn1": s.stats, "root": root.display().to_string(), "node_modules_before": before["node_modules"], "files_before": before["files"], "files_after": replay::files_state(&root, replay::YARN_LOCKFILES), "replay": replay(&args) }), chosen(&s.stats)));
            0
        }
        Ok(Err(RunError::Declined(reason))) => {
            log("declined", json!({ "reason": reason }));
            decline(&args, Some(&yarn), &reason)
        }
        // Yarn ran and failed, and has said why: its status is the answer
        // (plain Yarn would have failed the same way; nothing is retried).
        Ok(Err(RunError::Yarn { code, stats })) => {
            log("yarn_failed", with(json!({ "code": code, "reason": format!("yarn exited with status {code}"), "yarn1": stats }), chosen(&stats)));
            code
        }
        Err(_) => {
            // Yarn may have run: never start it twice.
            log("broke", json!({ "reason": "internal error (panic)" }));
            eprintln!("matchapack: internal error while yarn was running; run `MATCHAPACK=0 yarn` to install again");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_command_is_found_as_yarn_finds_it() {
        assert_eq!(command(&s(&[])), "");
        assert_eq!(command(&s(&["install", "--frozen-lockfile"])), "install");
        assert_eq!(command(&s(&["--frozen-lockfile"])), "");
        assert_eq!(command(&s(&["--cache-folder", "/c", "--frozen-lockfile"])), "");
        assert_eq!(command(&s(&["--cache-folder=/c", "add", "x"])), "add");
        assert_eq!(command(&s(&["--cache-folder", "/c", "add", "x"])), "add");
        // An optional value binds the next word too (`--production [prod]`).
        assert_eq!(command(&s(&["--production", "install"])), "");
        assert_eq!(command(&s(&["run", "build"])), "run");
        let handled = |a: &[&str]| engine_command(&s(a));
        for (a, want) in [
            (vec![], "install"),
            (vec!["install"], "install"),
            (vec!["--frozen-lockfile", "--cache-folder", "/c"], "install"),
            (vec!["install", "--production"], "install"),
            (vec!["add", "x"], "add"),
            (vec!["--cache-folder", "/c", "add", "x"], "add"),
            (vec!["-D", "add", "x"], "add"),
            (vec!["add", "x", "--dev", "--exact"], "add"),
            (vec!["upgrade"], "upgrade"),
            (vec!["upgrade", "--scope", "@y1", "--latest"], "upgrade"),
            (vec!["remove", "x"], "remove"),
            (vec!["upgrade-interactive", "--latest"], "upgrade-interactive"),
            (vec!["upgradeInteractive"], "upgrade-interactive"),
            (vec!["workspace", "w2", "add", "x"], "workspace add"),
            (vec!["--cache-folder", "/c", "workspace", "w2", "remove", "x"], "workspace remove"),
            (vec!["workspace", "w2", "upgrade", "--latest"], "workspace upgrade"),
            (vec!["workspace", "w2", "upgrade-interactive", "--latest"], "workspace upgrade-interactive"),
            (vec!["workspace", "w2", "upgradeInteractive"], "workspace upgrade-interactive"),
        ] {
            assert_eq!(handled(&a).as_deref(), Some(want), "{a:?}");
        }
        for a in [
            vec!["-v"],
            vec!["--version"],
            vec!["install", "--help"],
            vec!["add", "--help"],
            vec!["add", "-h", "x"],
            vec!["global", "add", "x"],
            vec!["run", "install"],
            vec!["run", "add"],
            vec!["why", "x"],
            vec!["workspace", "w2", "run", "add"],
            vec!["workspace", "w2", "install"],
            vec!["workspace", "w2"],
            vec!["workspace", "-x", "add", "y"],
            vec!["workspace", "w2", "--dev", "add"],
            vec!["workspaces", "run", "add"],
        ] {
            assert_eq!(handled(&a), None, "{a:?}");
        }
        // The Yarn a workspace command starts gets the words after the name.
        assert_eq!(workspace_args(&s(&["--cache-folder", "/c", "workspace", "w2", "add", "x", "--cache-folder", "/d"])), Some(&s(&["w2", "add", "x", "--cache-folder", "/d"])[..]));
        assert_eq!(workspace_args(&s(&["add", "workspace"])), None);
    }

    #[test]
    fn cache_arguments_are_passed_to_cache_dir() {
        assert_eq!(cache_args(&s(&["install", "--cache-folder", "/c", "--frozen-lockfile"])), s(&["--cache-folder", "/c"]));
        assert_eq!(cache_args(&s(&["--cache-folder=/c", "--no-default-rc", "--use-yarnrc", "r"])), s(&["--cache-folder=/c", "--no-default-rc", "--use-yarnrc", "r"]));
        assert_eq!(cache_args(&s(&["--preferred-cache-folder", "/p", "--offline"])), s(&["--preferred-cache-folder", "/p"]));
        assert!(cache_args(&s(&["--frozen-lockfile"])).is_empty());
    }

    /// Yarn 2+ is told from files: `.yarnrc.yml` up to the lockfile folder
    /// (not above it), `packageManager: yarn@2+`, a berry lockfile; nothing
    /// else (another package manager, Yarn 1) is.
    #[test]
    fn yarn2_projects_are_told_from_files() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("y2-files")).unwrap();
        let p = d.join("repo/pkg");
        std::fs::create_dir_all(&p).unwrap();
        let w = |f: &str, t: &str| std::fs::write(d.join(f), t).unwrap();
        w("repo/yarn.lock", "# yarn lockfile v1\n");
        w("repo/package.json", "{}");
        assert_eq!(yarn2_project(&p), None);
        w(".yarnrc.yml", "");
        assert_eq!(yarn2_project(&p), None, "a .yarnrc.yml above the lockfile folder");
        w("repo/pkg/package.json", r#"{"packageManager":"pnpm@10.0.0"}"#);
        assert_eq!(yarn2_project(&p), None, "another package manager: a gate's business");
        w("repo/pkg/package.json", r#"{"packageManager":"yarn@1.22.22"}"#);
        assert_eq!(yarn2_project(&p), None);
        w("repo/package.json", r#"{"packageManager":"yarn@4.5.0"}"#);
        assert!(yarn2_project(&p).unwrap().contains("yarn@4.5.0"), "the lockfile folder's packageManager");
        w("repo/package.json", "{}");
        w("repo/yarn.lock", "# comment\n\n__metadata:\n  version: 8\n");
        assert!(yarn2_project(&p).unwrap().contains("Yarn 2+ lockfile"));
        w("repo/yarn.lock", "# yarn lockfile v1\n");
        w("repo/.yarnrc.yml", "");
        assert!(yarn2_project(&p).unwrap().contains(".yarnrc.yml"));
        // No yarn.lock anywhere: only the working directory is looked at.
        let q = d.join("other/sub");
        std::fs::create_dir_all(&q).unwrap();
        w("other/.yarnrc.yml", "");
        assert_eq!(yarn2_project(&q), None);
        w("other/sub/.yarnrc.yml", "");
        assert!(yarn2_project(&q).is_some());
    }

    /// corepack's `dist/yarn.js` is recognized by its package's name; a
    /// `dist/yarn.js` of another package is not.
    #[test]
    fn corepack_launcher() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("y1-corepack")).unwrap();
        let js = d.join("corepack/dist/yarn.js");
        std::fs::create_dir_all(js.parent().unwrap()).unwrap();
        std::fs::write(&js, "").unwrap();
        std::fs::write(d.join("corepack/package.json"), r#"{"name":"corepack","version":"0.36.0"}"#).unwrap();
        assert!(is_corepack_launcher(&js));
        std::fs::write(d.join("corepack/package.json"), r#"{"name":"not-corepack"}"#).unwrap();
        assert!(!is_corepack_launcher(&js));
        let bin = d.join("yarn/bin/yarn.js");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        std::fs::write(d.join("yarn/package.json"), r#"{"name":"yarn","version":"1.22.22"}"#).unwrap();
        assert!(!is_corepack_launcher(&bin));
        assert_eq!(yarn1_package(&bin).unwrap(), "1.22.22");
        std::fs::write(d.join("yarn/package.json"), r#"{"name":"yarn","version":"4.5.0"}"#).unwrap();
        assert!(yarn1_package(&bin).unwrap_err().contains("not Yarn 1"));
    }

    /// The version floor (1.10.0): by number, not text; checked on the
    /// package matchapack finds itself.
    #[test]
    fn yarn_older_than_1_10_is_declined() {
        for (v, ok) in [("1.9.4", false), ("1.0.0", false), ("1.10.0", true), ("1.10.1", true), ("1.22.22", true), ("1.100.0", true), ("1.x", false), ("2.4.3", false)] {
            assert_eq!(yarn1_clones(v), ok, "{v}");
        }
        let d = std::fs::canonicalize(crate::store::tests::scratch("y1-floor")).unwrap();
        for v in ["1.9.4", "1.10.0", "1.22.22"] {
            let js = d.join(v).join("bin/yarn.js");
            std::fs::create_dir_all(js.parent().unwrap()).unwrap();
            std::fs::write(&js, "").unwrap();
            std::fs::write(d.join(v).join("package.json"), format!(r#"{{"name":"yarn","version":"{v}"}}"#)).unwrap();
            let got = yarn1_package(&js);
            if v == "1.9.4" {
                let e = got.unwrap_err();
                assert!(e.contains("yarn 1.9.4 is older than 1.10.0"), "{e}");
            } else {
                assert_eq!(got.as_deref(), Ok(v));
            }
        }
    }

    /// Homebrew's `bin/yarn` is recognized only as `write_env_script` writes
    /// it, naming its own keg's `libexec/bin/yarn.js`.
    #[test]
    fn homebrew_launcher_shape() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("y1-homebrew")).unwrap();
        let keg = d.join("Cellar/yarn/1.22.22");
        let js = keg.join("libexec/bin/yarn.js");
        std::fs::create_dir_all(js.parent().unwrap()).unwrap();
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::write(&js, "").unwrap();
        let other = d.join("Cellar/yarn/1.0.0/libexec/bin/yarn.js");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "").unwrap();
        let launcher = keg.join("bin/yarn");
        let with = |text: &str| {
            std::fs::write(&launcher, text).unwrap();
            homebrew_launcher(&launcher)
        };
        let (j, k) = (js.display(), d.display());
        assert_eq!(with(&format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{j}\"  \"$@\"\n")), Some((js.clone(), k.to_string())));
        for bad in [
            format!("#!/bin/sh\nPREFIX=\"{k}\" exec \"{j}\"  \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{j}\" \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{j}\"  \"$@\""),
            format!("#!/bin/bash\nexport X=1\nPREFIX=\"{k}\" exec \"{j}\"  \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{j}\"  \"$@\"\necho\n"),
            format!("#!/bin/bash\nOTHER=\"{k}\" exec \"{j}\"  \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"{k}\" X=\"1\" exec \"{j}\"  \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"$HOME\" exec \"{j}\"  \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"relative\" exec \"{j}\"  \"$@\"\n"),
            format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{}\"  \"$@\"\n", other.display()),
            format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{j}\" --x \"$@\"\n"),
        ] {
            assert_eq!(with(&bad), None, "{bad}");
        }
        // Only a `bin/yarn`.
        let elsewhere = keg.join("bin/yarnpkg-copy");
        std::fs::write(&elsewhere, format!("#!/bin/bash\nPREFIX=\"{k}\" exec \"{j}\"  \"$@\"\n")).unwrap();
        assert_eq!(homebrew_launcher(&elsewhere), None);
    }

    /// The Yarn behind each version manager's `yarn` shim (simulated:
    /// util::tests::fake_version_manager) is found by asking the shim, and
    /// node is started with the environment the shim gives Yarn: also when
    /// it sets a variable only for Yarn (PREFIX, as Homebrew's launcher
    /// does). One that adds arguments of its own to Yarn's, or starts a
    /// script beside Yarn's bin/yarn.js, is declined; so is a `yarn`
    /// launcher matchapack does not know, without being run.
    #[test]
    fn yarn_behind_version_manager_shims() {
        let Some(node) = util::tests::real_node() else { return eprintln!("SKIP: no node") };
        let d = std::fs::canonicalize(crate::store::tests::scratch("yarn-shims")).unwrap();
        for flavor in ["volta", "mise", "asdf"] {
            let t = d.join(flavor);
            let path = util::tests::path_with(&util::tests::fake_version_manager(&t, flavor, &node));
            let run = t.join("run");
            std::fs::create_dir_all(&run).unwrap();
            let run_dir = || Ok(run.clone());
            let y = find_yarn_on(&path, &t, &run_dir).unwrap();
            assert_eq!((y.js, y.version.as_str(), y.corepack, y.env.len()), (t.join("img/yarn/1.22.22/bin/yarn.js"), "1.22.22", false, 0), "{flavor}");
            let hb = t.join("homebrew-like");
            std::fs::create_dir_all(&hb).unwrap();
            std::fs::write(hb.join(".fakeyarnprefix"), "").unwrap();
            let y = find_yarn_on(&path, &hb, &run_dir).unwrap();
            let util::NodeStart::Launch(l) = &y.node else { panic!("{flavor}: not a Launch") };
            assert!(l.env.contains(&("PREFIX".into(), "/opt/fake-prefix".into())), "{flavor}: {:?}", l.env);
            assert_eq!(y.js, t.join("img/yarn/1.22.22/bin/yarn.js"), "{flavor}");
            // A known shim that starts another script beside Yarn's
            // bin/yarn.js: declined, not replaced by the yarn.js.
            let policy = t.join("policy");
            std::fs::create_dir_all(&policy).unwrap();
            std::fs::write(policy.join(".fakepolicy"), "").unwrap();
            let e = find_yarn_on(&path, &policy, &run_dir).unwrap_err();
            assert!(e.contains("/bin/policy.js: not Yarn 1's bin/yarn.js or corepack's launcher"), "{flavor}: {e}");
            let args = t.join("args");
            std::fs::create_dir_all(&args).unwrap();
            std::fs::write(args.join(".fakeargs"), "").unwrap();
            let e = find_yarn_on(&path, &args, &run_dir).unwrap_err();
            assert!(e.contains("passes the script arguments of its own (--version --ignore-scripts)"), "{flavor}: {e}");
            assert_eq!(std::fs::read_dir(&run).unwrap().count(), 0, "{flavor}: the probe's files are gone");
        }
        // Yarn 1's npm package on PATH (an npm-global install): no probe;
        // node without DYLD_* on macOS, as `#!/usr/bin/env node` gives it.
        let g = d.join("global");
        std::fs::create_dir_all(g.join("lib/node_modules/yarn/bin")).unwrap();
        std::fs::write(g.join("lib/node_modules/yarn/package.json"), r#"{"name":"yarn","version":"1.22.22"}"#).unwrap();
        std::fs::write(g.join("lib/node_modules/yarn/bin/yarn.js"), "#!/usr/bin/env node\n").unwrap();
        std::fs::create_dir_all(g.join("bin")).unwrap();
        std::os::unix::fs::symlink("../lib/node_modules/yarn/bin/yarn.js", g.join("bin/yarn")).unwrap();
        let no_probe = || -> std::result::Result<PathBuf, String> { panic!("probed Yarn's own package") };
        // A wrapper of Yarn that is no known launcher: declined, not run.
        let w = d.join("wrapper");
        let ran = d.join("wrapper-ran");
        write_exec_sh(&w.join("yarn"), &format!("#!/bin/sh\ntouch \"{}\"\ncase \"$1\" in install|\"\") set -- \"$@\" --ignore-scripts ;; esac\nexec node \"{}\" \"$@\"\n", ran.display(), g.join("lib/node_modules/yarn/bin/yarn.js").display()));
        let e = find_yarn_on(&util::tests::path_with(&w), &d, &no_probe).unwrap_err();
        assert!(e.contains(NOT_HANDLED) && e.contains("is a launcher matchapack does not follow"), "{e}");
        assert!(!ran.exists(), "the wrapper was run");
        let y = find_yarn_on(&util::tests::path_with(&g.join("bin")), &d, &no_probe).unwrap();
        assert!(matches!(&y.node, util::NodeStart::Path { node, drop_dyld } if node == Path::new("node") && *drop_dyld == cfg!(target_os = "macos")), "{:?}", y.node);
    }

    fn write_exec_sh(p: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn yarn_path_lines() {
        assert!(sets_yarn_path("yarn-path \".yarn/releases/yarn-1.22.22.js\"\n"));
        assert!(sets_yarn_path("# c\n  \"yarn-path\" \"x.js\"\n"));
        assert!(!sets_yarn_path("yarn-offline-mirror \"./m\"\n# yarn-path x\n"));
        assert!(!sets_yarn_path("cache-folder /c\n"));
    }

    #[test]
    fn facts_from_the_hello() {
        let f = Facts::from_json(&json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": true })).unwrap();
        assert_eq!(f.kept(0), (true, true));
        assert_eq!(f.kept(1), (true, false), "a non-empty copy is written after its times are set: mtime is now");
        let f20 = Facts::from_json(&json!({ "empty_atime": false, "empty_mtime": false, "atime": false, "mtime": false, "group": false })).unwrap();
        assert_eq!(f20.kept(0), (false, false));
        assert_eq!(Facts::from_json(&json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null })).unwrap().group, None);
        assert!(Facts::from_json(&json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": true })).is_none(), "a copy that keeps mtimes: no facts, no clones");
        assert!(Facts::from_json(&json!({ "atime": true })).is_none());
    }

    #[test]
    fn the_record_carries_what_a_replay_needs() {
        // The Yarn and node that ran, from the hello (scripts/verify-sample.sh --yarn).
        let st = Stats { hello: Some(json!({ "hello": true, "yarn": "1.22.22", "yarn_js": "/y/bin/yarn.js", "node": "v24.22.1", "node_path": "/n/bin/node", "first": 128 })), ..Default::default() };
        let j = st.to_json();
        assert_eq!((j["yarn_js"].clone(), j["node"].clone(), j["node_path"].clone()), (json!("/y/bin/yarn.js"), json!("v24.22.1"), json!("/n/bin/node")));
        assert_eq!(Stats::default().to_json()["node_path"], Value::Null);
        // The replay's arguments: no command word, no cache folder options
        // (verify-yarn1.sh gives both), every other word as given.
        assert_eq!(replay_args(&s(&["install", "--frozen-lockfile", "--cache-folder", "/c", "--ignore-scripts"])), s(&["--frozen-lockfile", "--ignore-scripts"]));
        assert_eq!(replay_args(&s(&["--cache-folder=/c", "--preferred-cache-folder", "/p", "--production", "install", "--check-files"])), s(&["--production", "install", "--check-files"]), "`install` after --production is that option's value, as Yarn reads it");
        assert_eq!(replay_args(&s(&["--mutex", "file:/tmp/m", "install"])), s(&["--mutex", "file:/tmp/m"]));
        assert_eq!(replay_args(&s(&[])), s(&[]));
        assert_eq!(replay(&s(&["install", "--frozen-lockfile"])), json!({ "kind": "install", "args": ["--frozen-lockfile"] }));
        assert_eq!(replay(&s(&["--frozen-lockfile"])), json!({ "kind": "install", "args": ["--frozen-lockfile"] }));
        // Another command: its whole command line, less the cache folder.
        assert_eq!(replay(&s(&["add", "x", "--cache-folder", "/c", "--dev"])), json!({ "kind": "command", "args": ["add", "x", "--dev"] }));
        assert_eq!(replay(&s(&["workspace", "w", "add", "--preferred-cache-folder=/p", "x"])), json!({ "kind": "command", "args": ["workspace", "w", "add", "x"] }));
    }

    struct Fx {
        dir: PathBuf,
        root: PathBuf,
        cache: PathBuf,
    }

    /// A project (`p`, with node_modules/a) and a Yarn cache (`cache/v6`,
    /// one entry), side by side in a scratch directory.
    fn fx(name: &str) -> Fx {
        let dir = std::fs::canonicalize(crate::store::tests::scratch(name)).unwrap();
        let root = dir.join("p");
        let cache = dir.join("cache/v6");
        std::fs::create_dir_all(root.join("node_modules/a/lib")).unwrap();
        std::fs::write(root.join("yarn.lock"), "").unwrap();
        let entry = cache.join("npm-a-1.0.0-abc-integrity/node_modules/a");
        std::fs::create_dir_all(entry.join("lib")).unwrap();
        std::fs::write(entry.join("index.js"), "x").unwrap();
        std::fs::write(entry.join("lib/empty.js"), "").unwrap();
        Fx { dir, root, cache }
    }

    fn server(f: &Fx) -> Server {
        let s = Server::new(f.root.clone()).unwrap();
        s.set_cache(Ok(f.cache.clone()));
        s
    }

    fn src(f: &Fx) -> PathBuf {
        f.cache.join("npm-a-1.0.0-abc-integrity/node_modules/a/index.js")
    }

    #[test]
    fn destinations_must_be_in_a_node_modules_of_the_project() {
        let f = fx("y1-dest");
        let s = server(&f);
        assert!(s.check_dest(&f.root.join("node_modules/a/index.js")).is_ok());
        assert!(s.check_dest(&f.root.join("node_modules/a/lib/x.js")).is_ok());
        // Outside the project, beside it, without node_modules, not plain.
        assert!(s.check_dest(&f.dir.join("elsewhere/node_modules/a/index.js")).is_err());
        std::fs::create_dir_all(f.root.join("src")).unwrap();
        let e = s.check_dest(&f.root.join("src/index.js")).unwrap_err();
        assert!(e.contains("not inside a node_modules"), "{e}");
        assert!(s.check_dest(&f.root.join("node_modules/a/../../x.js")).is_err());
        assert!(s.check_dest(Path::new("node_modules/a/index.js")).is_err());
        // A parent that is a symlink out of the project.
        let out = f.dir.join("outside/node_modules/q");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&out, f.root.join("node_modules/q")).unwrap();
        let e = s.check_dest(&f.root.join("node_modules/q/index.js")).unwrap_err();
        assert!(e.contains("resolves outside the project"), "{e}");
        // The verdict is kept per parent directory.
        assert!(s.dest_dirs.lock().unwrap().contains_key(&f.root.join("node_modules/q")));
    }

    #[test]
    fn sources_must_be_regular_files_inside_the_cache() {
        let f = fx("y1-src");
        let s = server(&f);
        let cache = std::fs::canonicalize(&f.cache).unwrap();
        assert!(s.check_src(&src(&f), &cache).is_ok());
        // Outside the cache.
        let other = f.dir.join("other.js");
        std::fs::write(&other, "x").unwrap();
        let e = s.check_src(&other, &cache).unwrap_err();
        assert!(e.contains("outside Yarn's cache"), "{e}");
        // A symlink as the source (into the cache, even).
        let link = src(&f).with_file_name("link.js");
        std::os::unix::fs::symlink(src(&f), &link).unwrap();
        let e = s.check_src(&link, &cache).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        // A directory; a parent that is a symlink out of the cache.
        assert!(s.check_src(&src(&f).with_file_name("lib"), &cache).is_err());
        std::os::unix::fs::symlink(&f.dir, f.cache.join("escape")).unwrap();
        let e = s.check_src(&f.cache.join("escape/other.js"), &cache).unwrap_err();
        assert!(e.contains("outside Yarn's cache"), "{e}");
        assert!(s.check_src(Path::new("relative.js"), &cache).is_err());
    }

    fn ask(s: &Server, src: &Path, dest: &Path) -> Value {
        let line = json!({ "id": 1, "src": src, "dest": dest }).to_string();
        serde_json::from_str(&s.handle_line(&line)[0]).unwrap()
    }

    fn hello(s: &Server, facts: Value) {
        assert!(s.record_message(&json!({ "hello": true, "first": 128, "facts": facts })));
    }

    #[test]
    fn requests_are_copied_until_the_hello_and_the_cache_are_known() {
        let f = fx("y1-pending");
        let mut s = Server::new(f.root.clone()).unwrap();
        s.cache_wait = Duration::from_millis(50);
        let dest = f.root.join("node_modules/a/index.js");
        assert_eq!(ask(&s, &src(&f), &dest)["reason"], "no-hello");
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["ok"].clone(), a["reason"].clone(), a["fatal"].clone()), (json!(false), json!("cache-dir-pending"), json!(false)));
        s.set_cache(Err("yarn cache dir exited with 1".into()));
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["reason"].clone(), a["fatal"].clone()), (json!("cache-unusable"), json!(true)));
        assert!(!dest.exists());
        let st = s.stats.lock().unwrap();
        assert_eq!(st.copied.get("cache-dir-pending"), Some(&1));
        assert_eq!(st.cloned, 0);
    }

    /// The wait for the cache folder is one deadline for the whole install:
    /// once it has passed, a request is answered at once.
    #[test]
    fn the_cache_wait_is_one_deadline_not_one_per_request() {
        let f = fx("y1-deadline");
        let mut s = Server::new(f.root.clone()).unwrap();
        s.cache_wait = Duration::from_millis(400);
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let dest = f.root.join("node_modules/a/index.js");
        let t = Instant::now();
        assert_eq!(ask(&s, &src(&f), &dest)["reason"], "cache-dir-pending");
        assert!(t.elapsed() >= Duration::from_millis(300), "the first request waits");
        let t = Instant::now();
        for _ in 0..5 {
            assert_eq!(ask(&s, &src(&f), &dest)["reason"], "cache-dir-pending");
        }
        assert!(t.elapsed() < Duration::from_millis(200), "later ones do not: {:?}", t.elapsed());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_request_waits_for_the_cache_folder() {
        let f = fx("y1-wait");
        let s = Server::new(f.root.clone()).unwrap();
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let dest = f.root.join("node_modules/a/index.js");
        let a = std::thread::scope(|sc| {
            let asked = sc.spawn(|| ask(&s, &src(&f), &dest));
            std::thread::sleep(Duration::from_millis(200));
            s.set_cache(Ok(f.cache.clone()));
            asked.join().unwrap()
        });
        assert_eq!(a["ok"], true, "{a}");
        assert!(dest.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_clone_gets_the_times_and_attributes_a_copy_would() {
        use std::os::unix::fs::MetadataExt;
        let f = fx("y1-clone");
        let s = server(&f);
        // Node >= 22's copy: empty files keep both times, others the atime.
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": true }));
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let set_old = |p: &Path| {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_times(std::fs::FileTimes::new().set_accessed(old).set_modified(old)).unwrap();
        };
        let empty_src = src(&f).with_file_name("lib/empty.js");
        set_old(&src(&f));
        set_old(&empty_src);
        let named = |p: &Path, n: &str| {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
            let k = std::ffi::CString::new(n).unwrap();
            assert_eq!(unsafe { libc::setxattr(c.as_ptr(), k.as_ptr(), b"1".as_ptr() as *const _, 1, 0, 0) }, 0);
        };
        named(&src(&f), "user.matchapack.test");
        let dest = f.root.join("node_modules/a/index.js");
        assert_eq!(ask(&s, &src(&f), &dest)["ok"], true);
        let m = std::fs::symlink_metadata(&dest).unwrap();
        assert_eq!(m.accessed().unwrap(), old, "atime kept");
        assert!(m.modified().unwrap() > old, "mtime now");
        assert_eq!(crate::diff::xattr_names(&dest).unwrap().iter().filter(|n| *n != "com.apple.provenance").count(), 0, "extra xattr removed");
        assert_eq!(m.gid(), std::fs::metadata(src(&f)).unwrap().gid());
        let edest = f.root.join("node_modules/a/lib/empty.js");
        assert_eq!(ask(&s, &empty_src, &edest)["ok"], true);
        let m = std::fs::symlink_metadata(&edest).unwrap();
        assert_eq!((m.accessed().unwrap(), m.modified().unwrap()), (old, old), "an empty clone keeps both, as Node's copy");
        assert!(sys::shares_blocks(&src(&f), &dest));
        let st = s.stats.lock().unwrap();
        assert_eq!((st.cloned, st.cloned_bytes), (2, 1));
        drop(st);
        // Node 20's copy: nothing kept.
        let f2 = fx("y1-clone20");
        let s2 = server(&f2);
        hello(&s2, json!({ "empty_atime": false, "empty_mtime": false, "atime": false, "mtime": false, "group": false }));
        set_old(&src(&f2).with_file_name("lib/empty.js"));
        let edest = f2.root.join("node_modules/a/lib/empty.js");
        assert_eq!(ask(&s2, &src(&f2).with_file_name("lib/empty.js"), &edest)["ok"], true);
        let m = std::fs::symlink_metadata(&edest).unwrap();
        assert!(m.accessed().unwrap() > old && m.modified().unwrap() > old);
    }

    /// clonefile(2) drops setuid/setgid; Node's copy keeps the source's mode.
    #[test]
    fn sources_with_special_mode_bits_are_left_to_node() {
        use std::os::unix::fs::PermissionsExt;
        let f = fx("y1-suid");
        let s = server(&f);
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        // The sticky bit is settable by the owner everywhere; setgid and
        // setuid take the same path (any bit of 0o7000).
        std::fs::set_permissions(src(&f), std::fs::Permissions::from_mode(0o1755)).unwrap();
        let dest = f.root.join("node_modules/a/index.js");
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["ok"].clone(), a["reason"].clone()), (json!(false), json!("src-special-mode")));
        assert!(!dest.exists());
        // Without the bit the same file is cloned (macOS: the engine's clone
        // exists there only).
        #[cfg(target_os = "macos")]
        {
            std::fs::set_permissions(src(&f), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(ask(&s, &src(&f), &dest)["ok"], true);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn failed_clones_and_fix_ups_leave_nothing_for_nodes_copy() {
        let f = fx("y1-fail");
        let s = server(&f);
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        // The destination exists: clonefile refuses (EEXIST), not fatal.
        let dest = f.root.join("node_modules/a/index.js");
        std::fs::write(&dest, "old").unwrap();
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["ok"].clone(), a["fatal"].clone()), (json!(false), json!(false)));
        assert_eq!(a["reason"], format!("clone-errno-{}", libc::EEXIST));
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        std::fs::remove_file(&dest).unwrap();
        // A fix-up that fails: the clone is removed.
        let mut s = s;
        s.test.fail_fixup_every = 1;
        let a = ask(&s, &src(&f), &dest);
        assert_eq!(a["reason"], "fixup-failed");
        assert!(!dest.exists(), "the clone was removed");
        // A cache on another volume: fatal.
        let s3 = Server::new(f.root.clone()).unwrap();
        s3.set_cache(Ok(PathBuf::from("/dev")));
        hello(&s3, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let a = ask(&s3, &src(&f), &dest);
        assert_eq!((a["reason"].clone(), a["fatal"].clone()), (json!("cache-unusable"), json!(true)));
        assert!(s3.stats.lock().unwrap().cache.as_deref().is_some_and(|c| c.contains("another volume")));
    }

    fn yarn_cache_dir(node: &util::NodeStart, yarn_js: &Path, cwd: &Path, args: &[String], deadline: Duration, cancel: &AtomicBool) -> std::result::Result<PathBuf, String> {
        start_yarn_cache_dir(node, yarn_js, &[], cwd, args)?.answer(deadline, cancel)
    }

    /// A `yarn cache dir` that does not answer is killed at its deadline, or
    /// as soon as the install has ended; either way its process is reaped.
    #[test]
    fn yarn_cache_dir_is_bounded_and_cancelled() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("skipped: no node");
            return;
        }
        let d = crate::store::tests::scratch("y1-cachedir");
        let slow = d.join("slow.js");
        std::fs::write(&slow, "require('fs').writeFileSync(process.argv[1] + '.pid', String(process.pid)); setTimeout(() => {}, 60000)\n").unwrap();
        let pid = || std::fs::read_to_string(d.join("slow.js.pid")).ok().and_then(|p| p.trim().parse::<i32>().ok());
        let gone = |p: i32| unsafe { libc::kill(p, 0) } != 0;
        let t = Instant::now();
        let e = yarn_cache_dir(&util::NodeStart::path("node"), &slow, &d, &[], Duration::from_millis(1500), &AtomicBool::new(false)).unwrap_err();
        assert!(e.contains("did not answer"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert!(pid().is_some_and(gone), "killed and reaped");
        let _ = std::fs::remove_file(d.join("slow.js.pid"));
        let cancel = AtomicBool::new(false);
        let t = Instant::now();
        let e = std::thread::scope(|s| {
            let h = s.spawn(|| yarn_cache_dir(&util::NodeStart::path("node"), &slow, &d, &[], Duration::from_secs(60), &cancel));
            std::thread::sleep(Duration::from_millis(1500));
            cancel.store(true, Ordering::Relaxed);
            h.join().unwrap().unwrap_err()
        });
        assert!(e.contains("install ended"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(pid().is_some_and(gone), "killed and reaped");
        // An answer, with a process left behind that inherited the pipe:
        // read without waiting for that process.
        let held = d.join("held.js");
        std::fs::write(&held, "require('child_process').spawn('sleep', ['30'], { stdio: ['ignore', 'inherit', 'inherit'], detached: true }).unref(); console.log('/held/cache/v6')\n").unwrap();
        let t = Instant::now();
        assert_eq!(yarn_cache_dir(&util::NodeStart::path("node"), &held, &d, &[], Duration::from_secs(30), &AtomicBool::new(false)).unwrap(), PathBuf::from("/held/cache/v6"));
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        // A real answer is read.
        let ok = d.join("ok.js");
        std::fs::write(&ok, "console.log('warning line'); console.log('/some/cache/v6')\n").unwrap();
        assert_eq!(yarn_cache_dir(&util::NodeStart::path("node"), &ok, &d, &[], Duration::from_secs(30), &AtomicBool::new(false)).unwrap(), PathBuf::from("/some/cache/v6"));
    }

    /// The end of an install does not wait for the pipes of a `yarn cache
    /// dir` it no longer needs: a process that holds them (here a `sleep`
    /// the child started) cannot hold the install's end back.
    #[test]
    fn a_cancelled_yarn_cache_dir_does_not_wait_for_its_pipes() {
        if std::process::Command::new("node").arg("-v").output().is_err() {
            eprintln!("skipped: no node");
            return;
        }
        let d = crate::store::tests::scratch("y1-cachedir-cancel");
        let held = d.join("held-slow.js");
        let script = "const c = require('child_process').spawn('sleep', ['30'], { stdio: ['ignore', 'inherit', 'inherit'], detached: true }); c.unref(); \
            require('fs').writeFileSync(process.argv[1] + '.pid', String(c.pid)); setTimeout(() => {}, 60000)\n";
        std::fs::write(&held, script).unwrap();
        let pid_file = d.join("held-slow.js.pid");
        let ask = start_yarn_cache_dir(&util::NodeStart::path("node"), &held, &[], &d, &[]).unwrap();
        let t = Instant::now();
        while !pid_file.exists() && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pid_file.exists(), "the child started its sleep");
        let t = Instant::now();
        let e = ask.answer(Duration::from_secs(60), &AtomicBool::new(true)).unwrap_err();
        let took = t.elapsed();
        if let Some(p) = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
        assert!(e.contains("install ended"), "{e}");
        assert!(took < Duration::from_millis(1500), "{took:?}");
        // The same when `yarn cache dir` has already ended and only its
        // pipe is held: what it printed is taken as it is.
        let done = d.join("held-done.js");
        let script = "const c = require('child_process').spawn('sleep', ['30'], { stdio: ['ignore', 'inherit', 'inherit'], detached: true }); c.unref(); \
            require('fs').writeFileSync(process.argv[1] + '.pid', String(c.pid)); console.log('/held/cache/v6')\n";
        std::fs::write(&done, script).unwrap();
        let mut ask = start_yarn_cache_dir(&util::NodeStart::path("node"), &done, &[], &d, &[]).unwrap();
        let t = Instant::now();
        while matches!(ask.child.try_wait(), Ok(None)) && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        let answer = ask.answer(Duration::from_secs(60), &AtomicBool::new(true));
        let took = t.elapsed();
        if let Some(p) = std::fs::read_to_string(d.join("held-done.js.pid")).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
        assert_eq!(answer, Ok(PathBuf::from("/held/cache/v6")));
        assert!(took < Duration::from_millis(1500), "{took:?}");
    }

    /// Under corepack `yarn cache dir` is started only once the hello has
    /// named the Yarn corepack chose (the install's spawn has finished by
    /// then): `chosen_yarn` does not return before it, returns that Yarn when
    /// it is Yarn 1's package, and gives up when the install ends first.
    #[test]
    fn under_corepack_the_cache_folder_is_asked_only_after_the_hello() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("y1-chosen")).unwrap();
        let js = d.join("v1/yarn/1.22.22/bin/yarn.js");
        std::fs::create_dir_all(js.parent().unwrap()).unwrap();
        std::fs::write(&js, "").unwrap();
        std::fs::write(d.join("v1/yarn/1.22.22/package.json"), r#"{"name":"yarn","version":"1.22.22"}"#).unwrap();
        let f = fx("y1-chosen-fx");
        let s = Server::new(f.root.clone()).unwrap();
        let ended = AtomicBool::new(false);
        let (got, early) = std::thread::scope(|sc| {
            let h = sc.spawn(|| s.chosen_yarn(&ended));
            std::thread::sleep(Duration::from_millis(300));
            let early = h.is_finished();
            assert!(s.record_message(&json!({ "hello": true, "first": 128, "yarn_js": js.to_str().unwrap(), "facts": {} })));
            (h.join().unwrap(), early)
        });
        assert!(!early, "returned before the hello");
        assert_eq!(got, Ok(js.clone()));
        // Not Yarn 1's package: refused.
        let s2 = Server::new(f.root.clone()).unwrap();
        std::fs::write(d.join("v1/yarn/1.22.22/package.json"), r#"{"name":"@yarnpkg/cli-dist","version":"4.5.0"}"#).unwrap();
        assert!(s2.record_message(&json!({ "hello": true, "first": 128, "yarn_js": js.to_str().unwrap(), "facts": {} })));
        assert!(s2.chosen_yarn(&ended).is_err());
        // The install ended without a hello (corepack failed, or a decline).
        let s3 = Server::new(f.root.clone()).unwrap();
        ended.store(true, Ordering::Relaxed);
        assert_eq!(s3.chosen_yarn(&ended).unwrap_err(), "the install ended first");
    }

    /// Once the node child is gone (`stop`), a worker waiting for Yarn's
    /// cache folder stops waiting: matchapack ends with the install, not at
    /// the cache folder's deadline.
    #[test]
    fn a_request_waiting_for_the_cache_folder_ends_when_the_server_stops() {
        let f = fx("y1-stop");
        let s = Server::new(f.root.clone()).unwrap();
        hello(&s, json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null }));
        let dest = f.root.join("node_modules/a/index.js");
        let t = Instant::now();
        let a = std::thread::scope(|sc| {
            let h = sc.spawn(|| ask(&s, &src(&f), &dest));
            std::thread::sleep(Duration::from_millis(300));
            s.stop();
            h.join().unwrap()
        });
        assert_eq!((a["ok"].clone(), a["reason"].clone()), (json!(false), json!("cache-dir-pending")));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(!dest.exists());
    }

    /// A workspace command's Yarn runs in its own Node, which measured its
    /// own facts: compared with the parent's, and on a mismatch its requests
    /// are Node's copies (fatal), with the reason recorded. Its facts never
    /// stand in for the parent's.
    #[test]
    fn a_workspace_childs_facts_must_be_the_parents() {
        let parent = json!({ "empty_atime": true, "empty_mtime": true, "atime": true, "mtime": false, "group": null });
        let child = |facts: Value| json!({ "hello": true, "first": 128, "facts": facts, "child": true, "cwd": "/x" });
        let f = fx("y1-child-facts");
        let dest = f.root.join("node_modules/a/index.js");

        // The same facts: nothing recorded, requests go on to be cloned.
        let s = server(&f);
        hello(&s, parent.clone());
        assert!(s.record_message(&child(parent.clone())));
        let a = ask(&s, &src(&f), &dest);
        assert_ne!(a["reason"], "child-facts-differ", "{a}");
        assert!(s.stats.lock().unwrap().child_facts_differ.is_none());
        let _ = std::fs::remove_file(&dest);

        // Other facts (its copy keeps an empty file's mtime, the parent's
        // does not): copy, fatal, the reason in the stats and the log record.
        let s = server(&f);
        hello(&s, parent.clone());
        let other = json!({ "empty_atime": true, "empty_mtime": false, "atime": true, "mtime": false, "group": null });
        assert!(s.record_message(&child(other)));
        let a = ask(&s, &src(&f), &dest);
        assert_eq!((a["ok"].clone(), a["reason"].clone(), a["fatal"].clone()), (json!(false), json!("child-facts-differ"), json!(true)));
        assert!(!dest.exists());
        let st = s.stats.lock().unwrap();
        assert_eq!(st.copied.get("child-facts-differ"), Some(&1));
        assert_eq!(st.cloned, 0);
        let why = st.to_json()["child_facts_differ"].as_str().unwrap_or("").to_string();
        assert!(why.contains("other facts") && why.contains("\"empty_mtime\":false"), "{why}");
        drop(st);

        // No facts in the child's hello: the same.
        let s = server(&f);
        hello(&s, parent.clone());
        assert!(s.record_message(&json!({ "hello": true, "child": true, "cwd": "/x" })));
        assert_eq!(ask(&s, &src(&f), &dest)["reason"], "child-facts-differ");

        // No facts from the parent: the child's do not take their place.
        let s = server(&f);
        assert!(s.record_message(&json!({ "hello": true, "first": 128 })));
        assert!(s.record_message(&child(parent.clone())));
        assert_eq!(ask(&s, &src(&f), &dest)["reason"], "no-hello");
    }

    #[test]
    fn link_step_messages_and_counts_are_recorded() {
        let f = fx("y1-msg");
        let s = server(&f);
        assert!(s.record_message(&json!({ "link_step": "start" })));
        assert!(s.record_message(&json!({ "link_step": "end", "counts": { "first-copies": 128, "not-eligible": 2, "x y\n": 1 } })));
        assert!(s.record_message(&json!({ "declined": "why" })));
        assert!(!s.record_message(&json!({ "id": 1 })));
        let st = s.stats.lock().unwrap();
        assert_eq!((st.link_steps, st.link_step_ends), (1, 1));
        assert_eq!(st.copied.get("first-copies"), Some(&128));
        assert_eq!(st.copied.get("xy"), Some(&1));
        assert_eq!(st.declined.as_deref(), Some("why"));
    }
}
