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
    static NPM: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    NPM.get_or_init(|| {
        let path = std::env::var_os("PATH").ok_or("no PATH")?;
        let cwd = std::env::current_dir().map_err(|e| format!("the current directory: {e}"))?;
        find_npm(&path, &cwd, &|| private_run_dir().map_err(|e| format!("{e:#}")))
    })
    .clone()
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
fn find_npm(path: &OsStr, cwd: &Path, run_dir: &dyn Fn() -> Result<PathBuf, String>) -> Result<PathBuf, String> {
    let bin = on_path(path, "npm").ok_or("no npm on PATH")?;
    if let Some(dir) = npm_package_of(&bin) {
        return Ok(dir);
    }
    let main = probe_launcher(&bin, "npm", path, cwd, &run_dir()?)?;
    npm_package_of(&main).ok_or_else(|| {
        let what = if main.ends_with("dist/npm.js") { "corepack's npm launcher, which matchapack does not handle" } else { "not npm's bin/npm-cli.js" };
        format!("the npm on PATH ({}) starts {}: {what}", bin.display(), main.display())
    })
}

/// Preloaded into node through NODE_OPTIONS by `probe_launcher`: reports the
/// node, the script it was started on and the environment, and exits before
/// that script runs.
const PROBE_JS: &str = "'use strict';\n\
// matchapack: what a launcher on PATH starts (src/util.rs, probe_launcher).\n\
require('fs').writeSync(1, '\\nMATCHAPACK-PROBE ' + JSON.stringify({ main: process.argv[1] ?? null, node: process.execPath, env: process.env }) + '\\n');\n\
process.exit(0);\n";

/// Variables a launcher's own process sets for itself, not for the program
/// it starts: bash's (`_`, SHLVL, PWD, OLDPWD) and macOS's per-process text
/// encoding. Ignored when two launchers' environments are compared, as are
/// `DYLD_*`: macOS removes them whenever a protected binary is on the way
/// (`#!/usr/bin/env node`, a bash shim), so they depend on how node was
/// reached, not on what the launcher chose. (matchapack starts node itself,
/// so node keeps them even where plain npm, through `/usr/bin/env`, would
/// not; nvm's layout is the same. NOTES "Version managers".)
const OWN_VARS: [&str; 5] = ["_", "SHLVL", "PWD", "OLDPWD", "__CF_USER_TEXT_ENCODING"];

fn own_var(k: &str) -> bool {
    OWN_VARS.contains(&k) || k.starts_with("DYLD_")
}

/// What `launcher` (the `npm` or `yarn` on PATH, a version manager's shim)
/// starts in `cwd`, as `<launcher> --version` with a probe preloaded into
/// node (NODE_OPTIONS=--require): the script node is given, symlinks
/// resolved. A tool-independent answer: whatever the launcher reads to
/// choose a version (`.nvmrc`, `.node-version`, `.tool-versions`,
/// `mise.toml`, package.json `volta`) it reads here, in this directory.
///
/// matchapack then starts that script with the `node` on PATH, so the same
/// probe runs `node` (on its own, with no script) at the same time, and the
/// answer is taken only when both start the same node binary with the same
/// environment: what a shim sets (PATH, its own variables) the `node` shim
/// sets too. PATH is compared by the directories a lookup would visit
/// (existing ones, first occurrence): asdf's npm wrapper repeats some.
/// Anything else (a launcher that sets a variable only for Yarn, as
/// Homebrew's does, or picks another node) is not taken.
///
/// `--version`, never the user's arguments: a launcher that does not start
/// node with the probe (it ignores NODE_OPTIONS, or is not a node launcher)
/// then only prints a version. A bare `yarn` would install.
pub(crate) fn probe_launcher(launcher: &Path, name: &str, path: &OsStr, cwd: &Path, run_dir: &Path) -> Result<PathBuf, String> {
    let prefix = format!("the {name} on PATH ({})", launcher.display());
    let js = run_dir.join("launcher-probe.js");
    let js_str = js.to_str().filter(|s| !s.contains(['"', '\\', '\n'])).ok_or_else(|| format!("{prefix}: cannot name {} in NODE_OPTIONS", js.display()))?;
    let _ = std::fs::remove_file(&js);
    std::fs::write(&js, PROBE_JS).map_err(|e| format!("writing {}: {e}", js.display()))?;
    let node_options = match std::env::var("NODE_OPTIONS") {
        Ok(o) if !o.trim().is_empty() => format!("{o} --require \"{js_str}\""),
        _ => format!("--require \"{js_str}\""),
    };
    // Output to files, not pipes: no pipe for another thread's spawn to
    // inherit (NOTES "The cause (2026-11-13)"), and nothing to read until
    // the child has exited.
    let start = |what: &str, cmd: &mut Command| -> Result<(std::process::Child, [PathBuf; 2]), String> {
        let files = [run_dir.join(format!("launcher-probe-{what}.out")), run_dir.join(format!("launcher-probe-{what}.err"))];
        let create = |p: &PathBuf| std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()));
        let (out, err) = (create(&files[0])?, create(&files[1])?);
        let child = cmd
            .current_dir(cwd)
            .env("PATH", path)
            .env("NODE_OPTIONS", &node_options)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("starting {what}: {e}"))?;
        Ok((child, files))
    };
    // Both started from this thread, one after the other, then both waited
    // for.
    let t0 = std::time::Instant::now();
    let a = start(name, Command::new(launcher).arg("--version"));
    let b = start("node", Command::new("node").args(["-e", ""]));
    let finish = |r: Result<(std::process::Child, [PathBuf; 2]), String>| -> Result<(Option<Probe>, String), String> {
        let (mut child, files) = r?;
        let status = child.wait().map_err(|e| e.to_string());
        let [text, err] = files.each_ref().map(|f| {
            let t = std::fs::read_to_string(f).unwrap_or_default();
            let _ = std::fs::remove_file(f);
            t
        });
        let status = status?;
        let err = err.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string();
        let probe = text.lines().rev().find_map(|l| l.strip_prefix("MATCHAPACK-PROBE ")).and_then(|j| serde_json::from_str::<Probe>(j).ok());
        let said = if err.is_empty() { format!("{status}") } else { err };
        Ok((probe, said))
    };
    let (a, b) = (finish(a), finish(b));
    let _ = std::fs::remove_file(&js);
    record("launcher probe", t0.elapsed().as_secs_f64());
    let (a, said) = a.map_err(|e| format!("{prefix}: {e}"))?;
    let a = a.ok_or_else(|| format!("{prefix} is a launcher matchapack could not see start node ({said})"))?;
    let (b, said) = b.map_err(|e| format!("{prefix}: the node on PATH: {e}"))?;
    let b = b.ok_or_else(|| format!("{prefix}: the node on PATH did not start ({said})"))?;
    same_start(&prefix, &a, &b)
}

/// The script the launcher's node (`a`) was started on, when the `node` on
/// PATH (`b`) is the same node binary with the same environment.
fn same_start(prefix: &str, a: &Probe, b: &Probe) -> Result<PathBuf, String> {
    let main = a.main.as_deref().ok_or_else(|| format!("{prefix} starts node without a script"))?;
    let real = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    if real(&a.node) != real(&b.node) {
        return Err(format!("{prefix} starts node {}, the node on PATH starts {}", a.node, b.node));
    }
    if let Some(var) = env_differs(&a.env, &b.env) {
        return Err(format!("{prefix} sets {var} for node differently from the node on PATH"));
    }
    std::fs::canonicalize(main).map_err(|e| format!("{prefix} starts {main}: {e}"))
}

#[derive(serde::Deserialize)]
struct Probe {
    main: Option<String>,
    node: String,
    env: std::collections::BTreeMap<String, String>,
}

/// The first variable two launchers set differently for node (`own_var`s
/// aside; PATH by `path_lookup_dirs`), or None.
fn env_differs(a: &std::collections::BTreeMap<String, String>, b: &std::collections::BTreeMap<String, String>) -> Option<String> {
    a.keys().chain(b.keys()).filter(|k| !own_var(k)).find(|k| {
        let (x, y) = (a.get(*k), b.get(*k));
        if k.as_str() == "PATH" {
            x.map(|p| path_lookup_dirs(p.as_ref())) != y.map(|p| path_lookup_dirs(p.as_ref()))
        } else {
            x != y
        }
    }).cloned()
}

/// The directories a command lookup on `path` visits: existing ones, each
/// once, in order.
fn path_lookup_dirs(path: &OsStr) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    for d in std::env::split_paths(path).filter(|d| d.is_dir()) {
        if !seen.contains(&d) {
            seen.push(d);
        }
    }
    seen
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

    /// The real node, or None (the probe tests are skipped without it).
    pub(crate) fn real_node() -> Option<PathBuf> {
        let n = on_path(&std::env::var_os("PATH")?, "node")?;
        let ok = Command::new(&n).arg("-v").output().is_ok_and(|o| o.status.success());
        ok.then(|| std::fs::canonicalize(n).unwrap())
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
    /// Each sets a variable of its own and puts the chosen Node's `bin` on
    /// PATH. The Node is 24.0.0 (npm 11.0.0), or the one a `.fakepin` in the
    /// working directory names (22.0.0: npm 10.0.0); both are the real node.
    /// Yarn 1.22.22 is a standalone package. A `.fakeyarnprefix` makes the
    /// `yarn` shim set PREFIX, as Homebrew's launcher does (only for Yarn).
    /// Returns the shims directory.
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
case $name in
  node) exec "$d/img/node/$v/bin/node" "$@" ;;
  npm)
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

    /// The npm behind each version manager's shim is the one it would run in
    /// that directory: its default, a project's pinned Node, a project's
    /// pinned npm (volta).
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
            assert_eq!(found(&plain), Ok(t.join("img/node/24.0.0/lib/node_modules/npm")), "{flavor}");
            assert_eq!(found(&pinned), Ok(t.join("img/node/22.0.0/lib/node_modules/npm")), "{flavor}");
            assert_eq!(found(&npm_pinned), Ok(t.join("img/npm/10.9.2")), "{flavor}");
            // The probe's files are gone.
            assert_eq!(std::fs::read_dir(t.join("run")).unwrap().count(), 0, "{flavor}");
        }
    }

    /// An `npm` that resolves to npm's package is taken as it is: no probe.
    /// Launchers that are not shims of a node npm are declined, with why.
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
        assert_eq!(find_npm(&path_with(&nvm.join("bin")), &d, &no_probe), Ok(nvm.join("lib/node_modules/npm")));

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
    }

    fn probe(main: Option<&str>, node: &str, env: &[(&str, &str)]) -> Probe {
        Probe { main: main.map(String::from), node: node.into(), env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    /// The launcher's answer is taken only when the `node` on PATH starts
    /// the same node binary with the same environment.
    #[test]
    fn launcher_and_node_must_agree() {
        let d = std::fs::canonicalize(crate::store::tests::scratch("same-start")).unwrap();
        let (n1, n2, main) = (d.join("n1/node"), d.join("n2/node"), d.join("npm-cli.js"));
        for f in [&n1, &n2, &main] {
            write_exec(f, "");
        }
        let (n1s, n2s, mains) = (n1.to_str().unwrap(), n2.to_str().unwrap(), main.to_str().unwrap());
        let p = d.to_str().unwrap();
        let path = format!("{p}/n1:{p}/n1:{p}/missing:/usr/bin");
        let a = probe(Some(mains), n1s, &[("PATH", &path), ("_", "/shims/npm"), ("TOOL", "1"), ("SHLVL", "2"), ("DYLD_LIBRARY_PATH", "/l")]);
        let b = probe(None, n1s, &[("PATH", &format!("{p}/n1:/usr/bin")), ("_", "/shims/node"), ("TOOL", "1")]);
        assert_eq!(same_start("x", &a, &b), Ok(main.clone()));
        let other = probe(None, n2s, &[("PATH", &format!("{p}/n1:/usr/bin")), ("TOOL", "1")]);
        assert!(same_start("x", &a, &other).unwrap_err().contains("the node on PATH starts"));
        let extra = probe(None, n1s, &[("PATH", &format!("{p}/n1:/usr/bin")), ("TOOL", "1"), ("PREFIX", "/p")]);
        assert!(same_start("x", &a, &extra).unwrap_err().contains("sets PREFIX"));
        let reordered = probe(None, n1s, &[("PATH", &format!("/usr/bin:{p}/n1")), ("TOOL", "1")]);
        assert!(same_start("x", &a, &reordered).unwrap_err().contains("sets PATH"));
        let unset = probe(None, n1s, &[("PATH", &format!("{p}/n1:/usr/bin"))]);
        assert!(same_start("x", &a, &unset).unwrap_err().contains("sets TOOL"));
        assert!(same_start("x", &probe(None, n1s, &[]), &probe(None, n1s, &[])).unwrap_err().contains("without a script"));
    }
}
