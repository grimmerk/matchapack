//! Small shared helpers: quiet-aware logging, phase times, where npm is
//! installed, matchapack's run and settings directories.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static PHASES: std::sync::Mutex<Vec<(String, f64)>> = std::sync::Mutex::new(Vec::new());

/// Record how long a step took; the wrapper writes them into log.jsonl.
pub fn record(label: &str, secs: f64) {
    if let Ok(mut p) = PHASES.lock() {
        p.push((label.to_string(), (secs * 100.0).round() / 100.0));
    }
}

/// Steps recorded so far, as a JSON object (label → seconds; repeats summed).
pub fn phases() -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Ok(p) = PHASES.lock() {
        for (k, v) in p.iter() {
            let prev = m.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
            m.insert(k.clone(), serde_json::json!(((prev + v) * 100.0).round() / 100.0));
        }
    }
    serde_json::Value::Object(m)
}

/// Quiet mode (used when matchapack stands in for `npm ci`): per-step timings
/// and counts are hidden unless MATCHAPACK_VERBOSE is set.
pub fn set_quiet(q: bool) {
    QUIET.store(q && std::env::var_os("MATCHAPACK_VERBOSE").is_none(), Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if !$crate::util::quiet() {
            eprintln!("[matchapack] {}", format!($($arg)*));
        }
    };
}

/// The npm installation the `npm` on PATH runs, in this directory: the
/// directory of npm's package, whose `bin/npm-cli.js` it starts. Used for
/// npm's own modules (pacote) and its version, so they always match the npm
/// that runs. `npm root -g` can point elsewhere. Looked up once per process
/// (`find_npm`); None when it cannot be told for certain.
pub fn npm_dir() -> Option<PathBuf> {
    npm_lookup().ok()
}

/// `npm_dir`, or why there is none (for the decline message).
pub fn npm_lookup() -> Result<PathBuf, String> {
    npm_found().map(|(dir, _)| dir)
}

/// How to start node for that npm (`NodeStart`); `node` on PATH when there
/// is no npm.
pub fn npm_node() -> NodeStart {
    npm_found().map(|(_, node)| node).unwrap_or_default()
}

fn npm_found() -> Result<(PathBuf, NodeStart), String> {
    static NPM: std::sync::OnceLock<Result<(PathBuf, NodeStart), String>> = std::sync::OnceLock::new();
    NPM.get_or_init(|| {
        let path = std::env::var_os("PATH").ok_or("no PATH")?;
        let cwd = std::env::current_dir().map_err(|e| format!("the current directory: {e}"))?;
        find_npm(&path, &cwd, &|| private_run_dir().map_err(|e| format!("{e:#}")))
    })
    .clone()
}

/// How matchapack starts node for npm or Yarn: the `node` on PATH with
/// matchapack's own environment (npm's or Yarn's files are on PATH, as nvm
/// puts them), or exactly the node a launcher on PATH starts, with its
/// argv[0] and its whole environment (`Launch`, from `probe_launcher`).
#[derive(Debug, Clone)]
pub enum NodeStart {
    Path(PathBuf),
    Launch(Launch),
}

impl Default for NodeStart {
    fn default() -> Self {
        NodeStart::Path(PathBuf::from("node"))
    }
}

impl NodeStart {
    #[cfg(test)]
    pub fn path(p: impl Into<PathBuf>) -> Self {
        NodeStart::Path(p.into())
    }

    /// A Command for this node; the caller's `env` calls apply on top.
    pub fn command(&self) -> Command {
        match self {
            NodeStart::Path(p) => Command::new(p),
            NodeStart::Launch(l) => {
                use std::os::unix::process::CommandExt;
                let mut c = Command::new(&l.node);
                c.arg0(&l.argv0).env_clear().envs(l.env.iter().map(|(k, v)| (k, v)));
                c
            }
        }
    }
}

/// The node a launcher started, as it started it: the binary
/// (`process.execPath`), argv[0] and the environment, with the user's own
/// NODE_OPTIONS in place of the probe's.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub node: PathBuf,
    pub argv0: String,
    pub env: Vec<(String, String)>,
}

pub fn npm_version() -> Result<String, String> {
    let dir = npm_lookup()?;
    package_json(&dir).and_then(|v| v["version"].as_str().map(String::from)).ok_or_else(|| format!("no version in {}/package.json", dir.display()))
}

fn package_json(dir: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(dir.join("package.json")).ok()?).ok()
}

/// The directory of npm's package when `cli` (symlinks resolved) is a file
/// in its `bin` (`<dir>/bin/npm-cli.js`).
fn npm_package_of(cli: &Path) -> Option<PathBuf> {
    let cli = std::fs::canonicalize(cli).ok()?;
    let dir = cli.parent()?.parent()?.to_path_buf();
    (package_json(&dir)?["name"] == "npm").then_some(dir)
}

/// The first executable `name` on `path`.
pub fn on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path).map(|d| d.join(name)).find(|p| is_executable(p))
}

/// The npm the first `npm` on `path` runs in `cwd`. Usually that file is a
/// symlink to npm's `bin/npm-cli.js` (nvm, fnm, Homebrew's node, the
/// official installer, mise with PATH activation). Otherwise it is a
/// launcher (a version manager's shim: volta, asdf, mise in shim mode), and
/// `probe_launcher` asks it what it starts.
fn find_npm(path: &OsStr, cwd: &Path, run_dir: &dyn Fn() -> Result<PathBuf, String>) -> Result<(PathBuf, NodeStart), String> {
    let bin = on_path(path, "npm").ok_or("no npm on PATH")?;
    if let Some(dir) = npm_package_of(&bin) {
        return Ok((dir, NodeStart::default()));
    }
    let (main, launch) = probe_launcher(&bin, "npm", path, cwd, &run_dir()?)?;
    let dir = npm_package_of(&main).ok_or_else(|| {
        let what = if main.ends_with("dist/npm.js") { "corepack's npm launcher, which matchapack does not handle" } else { "not npm's bin/npm-cli.js" };
        format!("the npm on PATH ({}) starts {}: {what}", bin.display(), main.display())
    })?;
    Ok((dir, NodeStart::Launch(launch)))
}

/// Preloaded into node through NODE_OPTIONS by `probe_launcher`: reports how
/// node was started (binary, argv[0], options, working directory,
/// environment, script) and exits before that script runs.
const PROBE_JS: &str = "'use strict';\n\
// matchapack: what a launcher on PATH starts (src/util.rs, probe_launcher).\n\
require('fs').writeSync(1, '\\nMATCHAPACK-PROBE ' + JSON.stringify({ main: process.argv[1] ?? null, node: process.execPath, argv0: process.argv0, exec_argv: process.execArgv, cwd: process.cwd(), env: process.env }) + '\\n');\n\
process.exit(0);\n";

/// How long a launcher may take to start node (`probe_launcher`). The first
/// run after a version manager installed a Node took 0.7 s here; one that
/// downloads a Node first takes longer, and is then plain npm or Yarn once
/// (which downloads it).
pub(crate) const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// What `launcher` (the `npm` or `yarn` on PATH, a version manager's shim)
/// starts in `cwd`: the script node is given (symlinks resolved) and how
/// node was started (`Launch`). Found by running `<launcher> --version` with
/// a probe preloaded into node (NODE_OPTIONS=--require). A tool-independent
/// answer: whatever the launcher reads to choose a version (`.nvmrc`,
/// `.node-version`, `.tool-versions`, `mise.toml`, package.json `volta`) it
/// reads here, in this directory.
///
/// matchapack then starts that script with exactly that node binary,
/// argv[0] and environment (`NodeStart::Launch`), so npm or Yarn, and the
/// scripts they run, see what they see under the launcher: its PATH, its
/// own variables, and none of the `DYLD_*` variables macOS removes on the
/// way (`#!/usr/bin/env node`, a bash shim). What matchapack cannot pass on
/// is declined: node options on the command line (`execArgv`: the install's
/// node gets none), another working directory, a launcher that changes
/// NODE_OPTIONS, and an environment JSON cannot carry exactly.
///
/// `--version`, never the user's arguments: a launcher that does not start
/// node with the probe (it ignores NODE_OPTIONS, or is not a node launcher)
/// then only prints a version. A bare `yarn` would install. The launcher
/// runs in its own process group, killed at PROBE_DEADLINE.
pub(crate) fn probe_launcher(launcher: &Path, name: &str, path: &OsStr, cwd: &Path, run_dir: &Path) -> Result<(PathBuf, Launch), String> {
    probe_launcher_within(launcher, name, path, cwd, run_dir, PROBE_DEADLINE)
}

fn probe_launcher_within(launcher: &Path, name: &str, path: &OsStr, cwd: &Path, run_dir: &Path, within: std::time::Duration) -> Result<(PathBuf, Launch), String> {
    use std::os::unix::process::CommandExt;
    let prefix = format!("the {name} on PATH ({})", launcher.display());
    let user_options = match std::env::var_os("NODE_OPTIONS") {
        None => None,
        Some(o) => Some(o.into_string().map_err(|_| format!("{prefix}: NODE_OPTIONS is not UTF-8"))?),
    };
    let js = run_dir.join("launcher-probe.js");
    let js_str = js.to_str().filter(|s| !s.contains(['"', '\\', '\n'])).ok_or_else(|| format!("{prefix}: cannot name {} in NODE_OPTIONS", js.display()))?;
    let node_options = match user_options.as_deref() {
        Some(o) if !o.trim().is_empty() => format!("{o} --require \"{js_str}\""),
        _ => format!("--require \"{js_str}\""),
    };
    // Output to files, not pipes: no pipe for another thread's spawn to
    // inherit (NOTES "The cause (2026-11-13)"), and nothing to read until
    // the launcher has exited.
    let files = [js.clone(), run_dir.join("launcher-probe.out"), run_dir.join("launcher-probe.err")];
    let remove = || {
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
    };
    remove();
    let t0 = std::time::Instant::now();
    let ran = (|| -> Result<(String, String), String> {
        std::fs::write(&js, PROBE_JS).map_err(|e| format!("writing {}: {e}", js.display()))?;
        let create = |p: &PathBuf| std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()));
        let mut child = Command::new(launcher)
            .arg("--version")
            .current_dir(cwd)
            .env("PATH", path)
            .env("NODE_OPTIONS", &node_options)
            .stdin(Stdio::null())
            .stdout(create(&files[1])?)
            .stderr(create(&files[2])?)
            .process_group(0)
            .spawn()
            .map_err(|e| format!("starting it: {e}"))?;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if t0.elapsed() < within => std::thread::sleep(std::time::Duration::from_millis(5)),
                Ok(None) => {
                    // The whole group: a shim's node may be a grandchild.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    let _ = child.wait();
                    return Err(format!("did not start node within {:.0} s", within.as_secs_f64()));
                }
                Err(e) => return Err(format!("waiting for it: {e}")),
            }
        };
        let read = |p: &PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        let err = read(&files[2]).lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string();
        Ok((read(&files[1]), if err.is_empty() { format!("{status}") } else { err }))
    })();
    remove();
    record("launcher probe", t0.elapsed().as_secs_f64());
    let (out, said) = ran.map_err(|e| format!("{prefix}: {e}"))?;
    let probe = out.lines().rev().find_map(|l| l.strip_prefix("MATCHAPACK-PROBE ")).and_then(|j| serde_json::from_str::<Probe>(j).ok());
    let probe = probe.ok_or_else(|| format!("{prefix} is a launcher matchapack could not see start node ({said})"))?;
    exact_launch(&prefix, probe, cwd, &node_options, user_options.as_deref())
}

#[derive(serde::Deserialize)]
struct Probe {
    main: Option<String>,
    node: String,
    argv0: String,
    exec_argv: Vec<String>,
    cwd: String,
    env: std::collections::BTreeMap<String, String>,
}

/// The script and the `Launch` that starts node exactly as the launcher did,
/// or why matchapack cannot.
fn exact_launch(prefix: &str, p: Probe, cwd: &Path, node_options: &str, user_options: Option<&str>) -> Result<(PathBuf, Launch), String> {
    let main = p.main.as_deref().ok_or_else(|| format!("{prefix} starts node without a script"))?;
    if !p.exec_argv.is_empty() {
        return Err(format!("{prefix} starts node with options ({}), matchapack would start it without", p.exec_argv.join(" ")));
    }
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if real(Path::new(&p.cwd)) != real(cwd) {
        return Err(format!("{prefix} starts node in {}, not in {}", p.cwd, cwd.display()));
    }
    if p.env.get("NODE_OPTIONS").map(String::as_str) != Some(node_options) {
        return Err(format!("{prefix} changes NODE_OPTIONS"));
    }
    // process.env is text: a variable that was not UTF-8 arrives changed.
    if let Some((k, _)) = p.env.iter().find(|(k, v)| k.contains('\u{fffd}') || v.contains('\u{fffd}')) {
        return Err(format!("{prefix}: the variable {k} cannot be passed on exactly"));
    }
    let mut env: Vec<(String, String)> = p.env.into_iter().filter(|(k, _)| k != "NODE_OPTIONS").collect();
    if let Some(o) = user_options {
        env.push(("NODE_OPTIONS".into(), o.into()));
    }
    let main = std::fs::canonicalize(main).map_err(|e| format!("{prefix} starts {main}: {e}"))?;
    Ok((main, Launch { node: PathBuf::from(p.node), argv0: p.argv0, env }))
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

static RUN_DIR: std::sync::OnceLock<crate::store::TmpDir> = std::sync::OnceLock::new();

/// A directory for this run's helper files: `<store>/tmp/<pid>-<nanos>-run`,
/// created fresh with mode 0700, once per process, and held (TmpDir) until
/// the process exits. Nobody else can create, replace or symlink files in it,
/// even when the store itself is shared. Store::new removes it once no
/// process holds it and it is an hour old.
pub fn private_run_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(d) = RUN_DIR.get() {
        return Ok(d.path.clone());
    }
    let tmp = crate::default_store(None).join("tmp");
    let d = crate::store::TmpDir::create(tmp.join(format!("{}-run", crate::store::process_tag())), 0o700)?;
    Ok(RUN_DIR.get_or_init(|| d).path.clone())
}

/// Remove this run's private directory if it was made and is empty (each
/// helper removes its own files). Left behind on an early error or a crash,
/// it goes with Store::new's cleanup.
pub fn remove_run_dir() {
    if let Some(d) = RUN_DIR.get() {
        let _ = std::fs::remove_dir(&d.path);
    }
}

/// matchapack's settings directory: $MATCHAPACK_CONFIG_DIR, else
/// ~/.config/matchapack (trial scope `paths`, `npm-verified`).
pub fn config_dir() -> std::path::PathBuf {
    std::env::var_os("MATCHAPACK_CONFIG_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| crate::home().join(".config/matchapack"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// The real node binary, as the working `node` reports it (the `node` on
    /// PATH may itself be a shim: asdf's is a bash script), or None (the
    /// probe tests are skipped without it).
    pub(crate) fn real_node() -> Option<PathBuf> {
        let o = Command::new("node").args(["-p", "process.execPath"]).output().ok()?;
        let p = PathBuf::from(String::from_utf8(o.stdout).ok()?.trim());
        (o.status.success() && p.is_absolute()).then(|| std::fs::canonicalize(p).unwrap())
    }

    fn write_exec(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A package whose `bin/<file>` must never run: the probe stops node
    /// before it.
    fn package(dir: &Path, name: &str, version: &str, file: &str) {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("package.json"), format!(r#"{{"name":"{name}","version":"{version}"}}"#)).unwrap();
        write_exec(&dir.join("bin").join(file), &format!("#!/usr/bin/env node\nthrow new Error('{name} ran: the probe should have stopped node first');\n"));
    }

    /// A simulated version manager under `d`, the shape of `flavor`'s shims
    /// (the real tools: NOTES "Version managers"):
    /// - `volta`: `shims/{node,npm,yarn}` are symlinks to one binary that
    ///   dispatches on its name; an npm can be pinned apart from node (its
    ///   own image, `.fakenpm` here);
    /// - `mise`: the same, the binary named `mise`; it starts npm as
    ///   `node <npm>/bin/npm-cli.js`;
    /// - `asdf`: `shims/*` are bash scripts running `asdf exec <name>`; npm
    ///   goes through the nodejs plugin's wrapper, which puts directories on
    ///   PATH again (one of them missing).
    ///
    /// Each sets a variable of its own, puts the chosen Node's `bin` on
    /// PATH and removes HOME (so a variable the launcher removed shows). The Node is 24.0.0 (npm 11.0.0), or the one a `.fakepin` in the
    /// working directory names (22.0.0: npm 10.0.0); both are the real node.
    /// Yarn 1.22.22 is a standalone package. A `.fakeyarnprefix` makes the
    /// `yarn` shim set PREFIX, as Homebrew's launcher does (only for Yarn);
    /// `.fakeflags` makes the `npm` shim start node with an option, `.fakecd`
    /// makes it change directory first. Returns the shims directory.
    pub(crate) fn fake_version_manager(d: &Path, flavor: &str, node: &Path) -> PathBuf {
        let img = d.join("img");
        for (v, npm) in [("24.0.0", "11.0.0"), ("22.0.0", "10.0.0")] {
            let n = img.join("node").join(v);
            std::fs::create_dir_all(n.join("bin")).unwrap();
            symlink(node, n.join("bin/node")).unwrap();
            package(&n.join("lib/node_modules/npm"), "npm", npm, "npm-cli.js");
            symlink("../lib/node_modules/npm/bin/npm-cli.js", n.join("bin/npm")).unwrap();
        }
        package(&img.join("npm/10.9.2"), "npm", "10.9.2", "npm-cli.js");
        package(&img.join("yarn/1.22.22"), "yarn", "1.22.22", "yarn.js");
        let r = d.display();
        let script = format!(
            r#"#!/bin/bash
d="{r}"; flavor={flavor}
name=${{0##*/}}
if [ $flavor = asdf ]; then name=$2; shift 2; fi
v=24.0.0; [ -f .fakepin ] && v=$(cat .fakepin)
case $flavor in volta) export _FAKE_VOLTA_RECURSION=1 ;; mise) export __FAKE_MISE_SHIM=1 ;; asdf) export ASDF_INSTALL_VERSION=$v ;; esac
front="$d/img/yarn/1.22.22/bin:$d/img/node/$v/bin"
export PATH="$front:$PATH"
unset HOME
case $name in
  node) exec "$d/img/node/$v/bin/node" "$@" ;;
  npm)
    [ -f .fakeflags ] && exec "$d/img/node/$v/bin/node" --max-old-space-size=4096 "$d/img/node/$v/lib/node_modules/npm/bin/npm-cli.js" "$@"
    [ -f .fakecd ] && cd "$(cat .fakecd)"
    [ -f .fakenpm ] && exec "$d/img/node/$v/bin/node" "$d/img/npm/$(cat .fakenpm)/bin/npm-cli.js" "$@"
    [ $flavor = mise ] && exec "$d/img/node/$v/bin/node" "$d/img/node/$v/lib/node_modules/npm/bin/npm-cli.js" "$@"
    [ $flavor = asdf ] && export PATH="$front:$d/no-such-dir:$PATH"
    exec "$d/img/node/$v/bin/npm" "$@" ;;
  yarn)
    [ -f .fakeyarnprefix ] && export PREFIX=/opt/fake-prefix
    exec "$d/img/yarn/1.22.22/bin/yarn.js" "$@" ;;
esac
echo "fake $flavor: no $name" >&2; exit 127
"#
        );
        let shims = d.join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        let bin = d.join(match flavor {
            "volta" => "volta-shim",
            "mise" => "bin/mise",
            _ => "bin/asdf",
        });
        write_exec(&bin, &script);
        for name in ["node", "npm", "yarn"] {
            if flavor == "asdf" {
                write_exec(&shims.join(name), &format!("#!/usr/bin/env bash\n# asdf-plugin: nodejs 24.0.0\nexec \"{}\" exec \"{name}\" \"$@\"\n", bin.display()));
            } else {
                symlink(&bin, shims.join(name)).unwrap();
            }
        }
        shims
    }

    /// PATH with `first` before the system's directories (no other node).
    pub(crate) fn path_with(first: &Path) -> std::ffi::OsString {
        std::env::join_paths([first, Path::new("/usr/bin"), Path::new("/bin")]).unwrap()
    }

    fn probe_dir(d: &Path) -> impl Fn() -> Result<PathBuf, String> + '_ {
        move || {
            let r = d.join("run");
            std::fs::create_dir_all(&r).unwrap();
            Ok(r)
        }
    }

    /// The variables, argv[0] and binary a `NodeStart` gives node.
    fn started_as(n: &NodeStart, cwd: &Path) -> (std::collections::BTreeMap<String, String>, String, PathBuf) {
        let o = n.command().current_dir(cwd).args(["-e", "process.stdout.write(JSON.stringify([process.env, process.argv0, process.execPath]))"]).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let (env, argv0, node): (std::collections::BTreeMap<String, String>, String, String) = serde_json::from_slice(&o.stdout).unwrap();
        (env, argv0, PathBuf::from(node))
    }

    /// The npm behind each version manager's shim is the one it would run in
    /// that directory: its default, a project's pinned Node, a project's
    /// pinned npm (volta); and node is started exactly as the shim starts it
    /// for npm: binary, argv[0], every variable (the shim's own, its PATH),
    /// no more (none of this process's: env_clear), nothing macOS removed on
    /// the way (cargo's DYLD_FALLBACK_LIBRARY_PATH, behind a bash shim).
    #[test]
    fn npm_behind_version_manager_shims() {
        let Some(node) = real_node() else { return eprintln!("SKIP: no node") };
        let d = std::fs::canonicalize(crate::store::tests::scratch("npm-shims")).unwrap();
        for flavor in ["volta", "mise", "asdf"] {
            let t = d.join(flavor);
            let path = path_with(&fake_version_manager(&t, flavor, &node));
            let (plain, pinned, npm_pinned) = (t.join("plain"), t.join("pinned"), t.join("npm-pinned"));
            for p in [&plain, &pinned, &npm_pinned] {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::write(pinned.join(".fakepin"), "22.0.0").unwrap();
            std::fs::write(npm_pinned.join(".fakenpm"), "10.9.2").unwrap();
            let found = |cwd: &Path| find_npm(&path, cwd, &probe_dir(&t));
            let dir = |cwd: &Path| found(cwd).map(|(d, _)| d);
            assert_eq!(dir(&plain), Ok(t.join("img/node/24.0.0/lib/node_modules/npm")), "{flavor}");
            assert_eq!(dir(&pinned), Ok(t.join("img/node/22.0.0/lib/node_modules/npm")), "{flavor}");
            assert_eq!(dir(&npm_pinned), Ok(t.join("img/npm/10.9.2")), "{flavor}");
            // The probe's files are gone.
            assert_eq!(std::fs::read_dir(t.join("run")).unwrap().count(), 0, "{flavor}");

            let (_, start) = found(&pinned).unwrap();
            let NodeStart::Launch(l) = &start else { panic!("{flavor}: not a Launch") };
            assert_eq!(std::fs::canonicalize(&l.node).unwrap(), node, "{flavor}");
            let (env, argv0, exec) = started_as(&start, &pinned);
            assert_eq!(std::fs::canonicalize(exec).unwrap(), node, "{flavor}");
            assert_eq!(argv0, l.argv0, "{flavor}");
            let want: std::collections::BTreeMap<String, String> = l.env.iter().cloned().collect();
            assert_eq!(env, want, "{flavor}: node sees exactly the launcher's environment");
            let own = match flavor {
                "volta" => "_FAKE_VOLTA_RECURSION",
                "mise" => "__FAKE_MISE_SHIM",
                _ => "ASDF_INSTALL_VERSION",
            };
            assert!(env.contains_key(own), "{flavor}: the shim's own variable");
            assert!(env["PATH"].starts_with(&format!("{}/img/yarn/1.22.22/bin:{}/img/node/22.0.0/bin:", t.display(), t.display())), "{flavor}: the shim's PATH");
            assert!(!env.contains_key("HOME"), "{flavor}: a variable the launcher removed is not passed on");
            assert!(!env.contains_key("NODE_OPTIONS") || std::env::var_os("NODE_OPTIONS").is_some(), "{flavor}: not the probe's NODE_OPTIONS");
            if std::env::var_os("DYLD_FALLBACK_LIBRARY_PATH").is_some() {
                assert!(!env.contains_key("DYLD_FALLBACK_LIBRARY_PATH"), "{flavor}: macOS removed it behind the bash shim, so node does not get it");
            }
        }
    }

    /// An `npm` that resolves to npm's package is taken as it is: no probe,
    /// the `node` on PATH. Launchers that are not shims of a node npm, and
    /// shims matchapack cannot follow exactly (a node option, another
    /// directory), are declined, with why.
    #[test]
    fn npm_launchers_not_resolved() {
        let Some(node) = real_node() else { return eprintln!("SKIP: no node") };
        let d = std::fs::canonicalize(crate::store::tests::scratch("npm-launchers")).unwrap();
        let no_probe = || -> Result<PathBuf, String> { panic!("probed an npm that resolves to its package") };
        // nvm's layout.
        let nvm = d.join("nvm");
        package(&nvm.join("lib/node_modules/npm"), "npm", "11.0.0", "npm-cli.js");
        std::fs::create_dir_all(nvm.join("bin")).unwrap();
        symlink("../lib/node_modules/npm/bin/npm-cli.js", nvm.join("bin/npm")).unwrap();
        let (dir, start) = find_npm(&path_with(&nvm.join("bin")), &d, &no_probe).unwrap();
        assert_eq!(dir, nvm.join("lib/node_modules/npm"));
        assert!(matches!(&start, NodeStart::Path(p) if p == Path::new("node")), "{start:?}");

        // Not a node launcher at all: it is run with --version only (a
        // bare `yarn` would install), and declined.
        let sh = d.join("sh");
        let log = d.join("sh-args");
        for name in ["npm", "yarn"] {
            write_exec(&sh.join(name), &format!("#!/bin/sh\necho \"{name} $*\" >> \"{}\"\necho 1.0.0\n", log.display()));
        }
        symlink(&node, sh.join("node")).unwrap();
        let e = find_npm(&path_with(&sh), &d, &probe_dir(&d)).unwrap_err();
        assert!(e.contains("could not see start node"), "{e}");
        let e = probe_launcher(&sh.join("yarn"), "yarn", &path_with(&sh), &d, &probe_dir(&d)().unwrap()).unwrap_err();
        assert!(e.contains("could not see start node"), "{e}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "npm --version\nyarn --version\n");

        // corepack's npm launcher (`corepack enable npm`).
        let cp = d.join("cp");
        package(&cp.join("lib/node_modules/corepack"), "corepack", "0.34.0", "corepack.js");
        write_exec(&cp.join("lib/node_modules/corepack/dist/npm.js"), "#!/usr/bin/env node\nthrow new Error('corepack ran');\n");
        std::fs::create_dir_all(cp.join("bin")).unwrap();
        symlink("../lib/node_modules/corepack/dist/npm.js", cp.join("bin/npm")).unwrap();
        symlink(&node, cp.join("bin/node")).unwrap();
        let e = find_npm(&path_with(&cp.join("bin")), &d, &probe_dir(&d)).unwrap_err();
        assert!(e.contains("corepack's npm launcher"), "{e}");

        // A shim that starts node with an option, or in another directory.
        let t = d.join("volta");
        let path = path_with(&fake_version_manager(&t, "volta", &node));
        let (flags, cd) = (t.join("flags"), t.join("cd"));
        for p in [&flags, &cd] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(flags.join(".fakeflags"), "").unwrap();
        std::fs::write(cd.join(".fakecd"), t.display().to_string()).unwrap();
        let e = find_npm(&path, &flags, &probe_dir(&t)).unwrap_err();
        assert!(e.contains("starts node with options (--max-old-space-size=4096)"), "{e}");
        let e = find_npm(&path, &cd, &probe_dir(&t)).unwrap_err();
        assert!(e.contains(&format!("starts node in {}, not in {}", t.display(), cd.display())), "{e}");
    }

    /// A launcher that does not start node in time is stopped, with the
    /// processes it started, and declined; its files are removed.
    #[test]
    fn a_stalled_launcher_is_stopped() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("stalled")).unwrap();
        let pid = d.join("pid");
        write_exec(&d.join("bin/npm"), &format!("#!/bin/sh\nsleep 30 &\necho $! > \"{}\"\nwait\n", pid.display()));
        let run = d.join("run");
        std::fs::create_dir_all(&run).unwrap();
        let t0 = std::time::Instant::now();
        let e = probe_launcher_within(&d.join("bin/npm"), "npm", &path_with(&d.join("bin")), &d, &run, std::time::Duration::from_millis(500)).unwrap_err();
        assert!(e.contains("did not start node within 0 s") || e.contains("did not start node within 1 s"), "{e}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "{:?}", t0.elapsed());
        assert_eq!(std::fs::read_dir(&run).unwrap().count(), 0, "the probe's files are gone");
        // The launcher's own child (`sleep`) was killed too.
        let child: i32 = std::fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
        let gone = (0..200).any(|_| {
            let alive = unsafe { libc::kill(child, 0) } == 0;
            if alive {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            !alive
        });
        assert!(gone, "sleep {child} still runs");
    }

    fn probe(main: Option<&str>, exec_argv: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Probe {
        Probe {
            main: main.map(String::from),
            node: "/n/node".into(),
            argv0: "node".into(),
            exec_argv: exec_argv.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.display().to_string(),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    /// What the launcher's node had is passed on whole, with the user's own
    /// NODE_OPTIONS (or none) in place of the probe's; what cannot be
    /// passed on is refused.
    #[test]
    fn launch_is_exact_or_refused() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("exact-launch")).unwrap();
        let main = d.join("npm-cli.js");
        write_exec(&main, "");
        let m = main.to_str().unwrap();
        let ours = "--require \"/run/launcher-probe.js\"";
        let env = [("PATH", "/a:/a::/b"), ("TOOL", "1"), ("NODE_OPTIONS", ours), ("_", "/shims/npm")];
        let (got, l) = exact_launch("x", probe(Some(m), &[], &d, &env), &d, ours, None).unwrap();
        assert_eq!(got, main);
        assert_eq!(l, Launch { node: "/n/node".into(), argv0: "node".into(), env: vec![("PATH".into(), "/a:/a::/b".into()), ("TOOL".into(), "1".into()), ("_".into(), "/shims/npm".into())] });
        let mine = format!("--max-old-space-size=100 {ours}");
        let (_, l) = exact_launch("x", probe(Some(m), &[], &d, &[("NODE_OPTIONS", &mine)]), &d, &mine, Some("--max-old-space-size=100")).unwrap();
        assert_eq!(l.env, vec![("NODE_OPTIONS".into(), "--max-old-space-size=100".into())]);
        let refused = |p: Probe| exact_launch("x", p, &d, ours, None).unwrap_err();
        assert!(refused(probe(Some(m), &["--preserve-symlinks"], &d, &[("NODE_OPTIONS", ours)])).contains("with options (--preserve-symlinks)"));
        assert!(refused(probe(Some(m), &[], Path::new("/"), &[("NODE_OPTIONS", ours)])).contains("starts node in /, not in"));
        assert!(refused(probe(Some(m), &[], &d, &[("NODE_OPTIONS", "--other")])).contains("changes NODE_OPTIONS"));
        assert!(refused(probe(Some(m), &[], &d, &[])).contains("changes NODE_OPTIONS"));
        assert!(refused(probe(Some(m), &[], &d, &[("NODE_OPTIONS", ours), ("X", "a\u{fffd}b")])).contains("the variable X cannot be passed on exactly"));
        assert!(refused(probe(None, &[], &d, &[("NODE_OPTIONS", ours)])).contains("without a script"));
    }
}
