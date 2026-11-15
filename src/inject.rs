//! The inject engine, the only one since PR C2 (NOTES "Inject engine", "PR C2";
//! grimmerk/matchapack-private#33): npm runs its own `npm ci`, in a node child started
//! here, and only the per-package extraction is replaced by a clone from the
//! store. This process keeps what must be decided before anything starts
//! (npm version gate, umask), holds the store's shared lock for the whole
//! run, and serves the clone requests of the hook (src/inject.js) over a
//! socket (src/serve.rs). Where npm installs is npm's own answer: the helper
//! reports it when npm starts the command, and the server runs the gates
//! that depend on it (trial scope, a symlinked node_modules) and pins it
//! (grimmerk/matchapack-private#36; NOTES "The lazy hook and npm's install root").
//! Everything else is npm's: reading its config and arguments, validating the
//! lockfile, removing the old node_modules, scripts, bin links, the hidden
//! lockfile, the audit.

use crate::serve::{CheckPolicy, Server};
use crate::store::Store;
use crate::{clone, util, wrapper, InstallError, Summary};
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

/// MATCHAPACK_ENGINE: unset, empty or `inject` (the only engine since PR C2,
/// #33) is fine. `classic` names the engine PR C2 removed, and anything else
/// is no engine: an error, never a silent default. `matchapack install` stops
/// on it; `npm ci` through the shell function runs plain npm with it as the
/// reason (nothing has been changed, and the tree is npm's either way).
pub fn engine() -> std::result::Result<(), String> {
    match std::env::var("MATCHAPACK_ENGINE").as_deref() {
        Err(_) | Ok("") | Ok("inject") => Ok(()),
        Ok("classic") => Err(
            "MATCHAPACK_ENGINE=classic: the classic engine was removed in PR C2; unset MATCHAPACK_ENGINE (or set it to inject) for the inject engine".into(),
        ),
        Ok(other) => Err(format!("MATCHAPACK_ENGINE={other} is not an engine (inject is the only one)")),
    }
}

pub struct InjectOpts {
    /// The directory npm starts in. npm finds its install root from here
    /// (an ancestor in a workspace, or its --prefix).
    pub dir: PathBuf,
    /// The root npm reports must be inside the trial scope (the shell
    /// function's runs; `matchapack install` names its directory itself).
    pub trial_scope: bool,
    pub store: PathBuf,
    pub clone_mode: clone::Mode,
    /// npm's arguments, command first (`ci --no-audit`), passed on unchanged.
    pub npm_args: Vec<String>,
}

/// The fd number the node child finds the socket at.
pub(crate) const CHILD_FD: i32 = 3;

/// The helper's exit status when it stops before npm's command runs: before
/// npm starts, or when the server refuses the install root npm reported
/// (src/inject.js; the Yarn 1 hook, src/yarn1-hook.js, uses the same).
pub(crate) const HELPER_DECLINED: i32 = 90;

/// The checks that must hold before npm starts. A refusal changes nothing in
/// the project, so the caller can run plain npm. The checks that depend on
/// where npm installs run on npm's own root, when npm reports it
/// (serve.rs `accept_root`).
fn gates(o: &InjectOpts) -> Result<(PathBuf, CheckPolicy, String)> {
    if !o.dir.is_dir() {
        bail!("{} is not a directory", o.dir.display());
    }
    wrapper::inject_npm_version_gate().map_err(|e| anyhow!(e))?;
    // The pacote fingerprints the hook may be installed on; the hook checks
    // its npm's before npm starts (src/inject.js).
    let fingerprints = wrapper::inject_fingerprints().map_err(|e| anyhow!(e))?;
    // Clones carry the modes the store was extracted with under umask 022;
    // npm's files follow the process umask. (The hook checks it again per
    // package; here it also guards what this process extracts into the store.)
    let umask = wrapper::process_umask();
    if umask != 0o022 {
        bail!("umask is {umask:03o}, the store assumes 022");
    }
    let npm_dir = util::npm_lookup().map_err(|e| anyhow!(e))?;
    for m in ["lib/cli.js", "bin/npm-cli.js", "node_modules/pacote/package.json"] {
        if !npm_dir.join(m).exists() {
            bail!("npm installation {} has no {m}", npm_dir.display());
        }
    }
    let policy = CheckPolicy::from_env().map_err(|e| anyhow!(e))?;
    Ok((npm_dir, policy, fingerprints))
}

/// A hidden-lockfile key that names a package directory inside a
/// node_modules (`node_modules/a`, `packages/w/node_modules/@s/b`), as
/// opposed to the target of a link (a workspace or `file:` directory such as
/// `packages/w`, which npm lists too and never extracts).
fn is_package_key(key: &str) -> bool {
    let Some(i) = key.rfind("node_modules/") else { return false };
    if i > 0 && !key[..i].ends_with('/') {
        return false;
    }
    let name = &key[i + "node_modules/".len()..];
    match name.split_once('/') {
        None => !name.is_empty() && !name.starts_with('@'),
        Some((scope, rest)) => scope.starts_with('@') && !rest.is_empty() && !rest.contains('/'),
    }
}

/// A hidden lockfile's packages that npm extracts, inside the root: the
/// entries that sit in a node_modules, without links and bundled packages
/// (those come inside their bundler's tarball), and without the ones outside
/// the root (a key such as `../x/node_modules/a`, if npm ever writes one:
/// the hook never asks for a destination outside the root). Key → (version, integrity),
/// what npm's diff compares (arborist `lib/diff.js`: version, then
/// integrity; a changed `resolved` alone extracts nothing).
type Packages = BTreeMap<String, (Option<String>, Option<String>)>;

fn hidden_packages(text: &[u8]) -> Option<Packages> {
    let v: serde_json::Value = serde_json::from_slice(text).ok()?;
    let flag = |p: &serde_json::Value, k: &str| p.get(k).and_then(|x| x.as_bool()) == Some(true);
    let s = |p: &serde_json::Value, k: &str| p.get(k).and_then(|x| x.as_str()).map(String::from);
    Some(
        v.get("packages")?
            .as_object()?
            .iter()
            .filter(|(k, p)| is_package_key(k) && !k.starts_with("../") && !k.starts_with('/') && !flag(p, "link") && !flag(p, "inBundle"))
            .map(|(k, p)| (k.clone(), (s(p, "version"), s(p, "integrity"))))
            .collect(),
    )
}

/// The packages npm must have extracted in this run, so the hook should
/// have seen each (by its destination): of the packages in the hidden
/// lockfile npm wrote, those placed in this run, for every command. A
/// package counts as placed when its key is a real directory whose own
/// package.json was written after `since` (when npm reported its root,
/// before its command ran; None: no filter, tests): an extraction and a
/// clone (touched) both write it then. Not counted: a package npm left in
/// place, also when it gets a nested node_modules in this run (its
/// directory's time changes then, its package.json's does not), and a key
/// that is a symlink (npm 10 with --install-strategy=linked: hoisted paths
/// point into node_modules/.store, which the hook saw).
///
/// This replaced, in review round 4, a first filter by the hidden lockfile
/// before the run (a package new in it, or with another version or
/// integrity; the plan's U2): a package npm keeps that is new in the hidden
/// lockfile is not extracted, and a package npm restores at the same
/// version (its directory removed by hand) is, so that filter both raised
/// false alarms and missed packages (NOTES "Review round 4"). A lower
/// bound: a package the hook missed was extracted by npm, so its
/// package.json is new and it is counted; one npm or a script set back in
/// time is not. None: no hidden lockfile after the run.
fn expected_packages(root: &std::path::Path, since: Option<std::time::SystemTime>) -> Option<BTreeSet<String>> {
    let after = hidden_packages(&std::fs::read(root.join("node_modules/.package-lock.json")).ok()?)?;
    let written = |k: &String| match since {
        None => true,
        Some(t) => {
            std::fs::symlink_metadata(root.join(k)).is_ok_and(|m| m.file_type().is_dir())
                && std::fs::symlink_metadata(root.join(k).join("package.json")).and_then(|m| m.modified()).is_ok_and(|m| m >= t)
        }
    };
    Some(after.into_keys().filter(written).collect())
}

/// The store entries the hidden lockfile npm wrote names, and whether it
/// names the source (an integrity or a `resolved`) of every package npm
/// extracts: only then can it say which entries left the tree
/// (`Store::update_record`). None: no hidden lockfile, or not readable.
fn tree_entries(root: &std::path::Path) -> Option<(std::collections::HashSet<String>, bool)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("node_modules/.package-lock.json")).ok()?).ok()?;
    let flag = |p: &serde_json::Value, k: &str| p.get(k).and_then(|x| x.as_bool()) == Some(true);
    let mut keys = std::collections::HashSet::new();
    let mut complete = true;
    for (k, p) in v.get("packages")?.as_object()? {
        match p.get("integrity").and_then(|x| x.as_str()).and_then(|s| crate::store::Integrity::parse(s).ok()) {
            Some(i) => {
                keys.insert(i.key());
            }
            None if is_package_key(k) && !flag(p, "link") && !flag(p, "inBundle") && p.get("resolved").and_then(|x| x.as_str()).is_none() => complete = false,
            None => {}
        }
    }
    Some((keys, complete))
}

/// The error for reads of unexpected keys of the hook's result, or None.
/// `matchapack report` finds these runs by the words "of what matchapack's
/// hook returned".
fn trap_fired(unexpected: &std::collections::BTreeMap<String, usize>) -> Option<anyhow::Error> {
    if unexpected.is_empty() {
        return None;
    }
    let keys: Vec<String> = unexpected.iter().map(|(k, n)| format!("`{k}` ({n}×)")).collect();
    Some(anyhow!(
        "npm read {} of what matchapack's hook returned for a cloned package instead of pacote's extract: this npm uses the result, so what it installed may not be npm's own",
        keys.join(", ")
    ))
}

/// npm's root report of this process's run (the run's counts and npm's
/// settings at that moment), set by the server when it accepts the root: a
/// panic loses the run's stats, and the wrapper's advice after it still
/// needs the root, `save` and the workspace filter.
static ROOT_REPORT: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);

pub(crate) fn set_root_report(v: serde_json::Value) {
    *ROOT_REPORT.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
}

/// The root report of this process's run, if npm reported one (for the
/// advice after a panic).
pub(crate) fn root_report() -> Option<serde_json::Value> {
    ROOT_REPORT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) fn threads() -> usize {
    std::env::var("MATCHAPACK_INJECT_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
}

/// Start `cmd` with `theirs` (one end of a socket pair) as the child's fd 3,
/// and serve `ours` on a thread while it runs (`serve`). When the child has
/// exited: `stop` (drop what is still queued), shut the socket down (a
/// process the child started may still hold its end) and wait for `serve`.
/// The outer error: the child could not be started. Shared by the npm inject
/// engine and the Yarn 1 engine (src/yarn1.rs).
pub(crate) fn run_serving(
    cmd: &mut std::process::Command,
    ours: UnixStream,
    theirs: UnixStream,
    serve: impl FnOnce(UnixStream) + Send,
    stop: impl FnOnce(),
) -> std::io::Result<std::io::Result<std::process::ExitStatus>> {
    let theirs_fd = theirs.as_raw_fd();
    // The child's end of the socket becomes its fd 3. Only async-signal-safe
    // calls between fork and exec. dup2 clears close-on-exec on the new fd;
    // when the socket already is fd 3, the flag is cleared instead.
    unsafe {
        cmd.pre_exec(move || {
            if theirs_fd == CHILD_FD {
                let flags = libc::fcntl(theirs_fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(theirs_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(theirs_fd, CHILD_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Only the child holds its end now: when it exits, the server sees the
    // end of the stream.
    drop(theirs);
    let closer = ours.try_clone();
    Ok(std::thread::scope(|s| {
        let serving = s.spawn(move || serve(ours));
        let status = child.wait();
        // The child is gone: drop what is still queued and end the stream,
        // also when a process the child started still holds the socket.
        stop();
        if let Ok(c) = &closer {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        let _ = serving.join();
        status
    }))
}

pub fn run(o: &InjectOpts) -> std::result::Result<Summary, InstallError> {
    let t0 = Instant::now();
    let (npm_dir, policy, fingerprints) = gates(o).map_err(InstallError::Declined)?;
    let prepared = (|| -> Result<_> {
        // The shared lock: no entry is removed while npm may ask for it. The
        // cache path is unused: every request names the cache of the npm
        // that is running.
        let store = Store::new(o.store.clone(), crate::home().join(".npm/_cacache"))?;
        let helper = util::private_run_dir()?.join("inject.js");
        let _ = std::fs::remove_file(&helper);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&helper)?;
            f.write_all(include_str!("inject.js").as_bytes())?;
        }
        let (ours, theirs) = UnixStream::pair().context("creating the socket for the clone requests")?;
        Ok((store, helper, ours, theirs))
    })();
    let (store, helper, ours, theirs) = prepared.map_err(InstallError::Declined)?;
    let server = match Server::new(&store, None, o.clone_mode, None, policy) {
        Ok(s) => {
            s.set_trial_scope(o.trial_scope);
            s
        }
        Err(e) => {
            let _ = std::fs::remove_file(&helper);
            return Err(InstallError::Declined(e));
        }
    };

    // Not util::node_cmd: npm runs here as the user's `npm ci`, and the
    // scripts it starts must see the user's environment, without a
    // MATCHAPACK=0 of ours. (The helper removes MATCHAPACK_INJECT_FD.) Behind
    // a version manager's shim, the node and environment the shim gives npm
    // (util::NodeStart).
    let mut cmd = util::npm_node().command();
    cmd.arg(&helper)
        .arg(&npm_dir)
        .args(&o.npm_args)
        .current_dir(&o.dir)
        .env("MATCHAPACK_INJECT_FD", CHILD_FD.to_string())
        .env("MATCHAPACK_INJECT_FINGERPRINTS", &fingerprints);
    let t_npm = Instant::now();
    let serve = |sock| {
        let _ = server.serve(sock, threads());
    };
    let status = match run_serving(&mut cmd, ours, theirs, serve, || server.stop()) {
        Ok(status) => status,
        Err(e) => {
            let _ = std::fs::remove_file(&helper);
            return Err(InstallError::Declined(anyhow!("could not start node: {e}")));
        }
    };
    let npm_secs = t_npm.elapsed().as_secs_f64();
    let _ = std::fs::remove_file(&helper);
    let stats = std::mem::take(&mut *server.stats.lock().unwrap());
    // The pacote and tar versions from the hook's hello (what node loaded).
    let key = server.key();
    drop(server);

    let status = match status {
        Ok(s) => s,
        Err(e) => return Err(InstallError::Broke { error: anyhow!("waiting for npm: {e}"), inject: Some(stats.to_json()) }),
    };
    // Tests only (test-inject.sh): the same, after npm ran with its command,
    // so that the answer to a broken run of each command can be checked.
    // `panic`: a real panic, caught by the wrapper (`root_report`).
    match std::env::var("MATCHAPACK_TEST_BROKE").as_deref() {
        Ok("1") if stats.root.is_some() => {
            return Err(InstallError::Broke { error: anyhow!("waiting for npm: a lost child status (MATCHAPACK_TEST_BROKE)"), inject: Some(stats.to_json()) });
        }
        Ok("panic") if stats.root.is_some() => panic!("a panic after npm ran (MATCHAPACK_TEST_BROKE=panic)"),
        _ => {}
    }
    // The helper exits with HELPER_DECLINED when it stops before npm's
    // command runs, having told this process why: before npm starts (no
    // pacote file, a fingerprint not on the list), or because the server
    // refused the root npm reported. Nothing in the project has changed.
    // All three signs are required, so that npm, once its command ran, is
    // never run twice: the status, no accepted root, and a recorded reason
    // (or no hello: the helper stopped before it could say anything).
    if status.code() == Some(HELPER_DECLINED) && stats.root.is_none() && (stats.declined.is_some() || stats.hello.is_none()) {
        let why = stats.declined.clone().unwrap_or_else(|| format!("the inject helper could not start npm ({status})"));
        return Err(InstallError::Declined(anyhow!(why)));
    }
    let root = stats.root.clone();
    // From here on the project holds clones of these entries, whatever npm's
    // exit status. gc cannot run before this record is written: it needs the
    // store's exclusive lock, which `store` still holds shared. Without a
    // root (npm stopped before its command, or its root could not be read)
    // nothing was cloned.
    if let Some(root) = &root {
        // One rule for every command (store.rs `update_record`): also after
        // a failed run, which npm rolled back.
        let ci = stats.command.as_deref().unwrap_or("ci") == "ci";
        let tree = tree_entries(root);
        let shape = match &tree {
            Some((t, true)) => crate::store::Tree::Complete(t),
            Some((t, false)) => crate::store::Tree::Incomplete(t),
            None => crate::store::Tree::Unknown,
        };
        let recorded = store.update_record(root, &stats.used, shape, ci);
        if let Err(e) = recorded {
            crate::info!("could not record the project for `matchapack store gc`: {e:#}");
        }
    }
    util::record("npm", npm_secs);
    util::record("clone requests (first to last)", stats.wall_secs());
    for (k, (_, place, touch)) in &stats.kinds {
        util::record(&format!("{k}: place (thread sum)"), *place);
        util::record(&format!("{k}: touch (thread sum)"), *touch);
    }
    let kinds: Vec<String> = stats.kinds.iter().map(|(k, v)| format!("{k} {}", v.0)).collect();
    let falls: Vec<String> = stats.fallbacks.iter().map(|(k, n)| format!("{k} {n}")).collect();
    let asides: usize = stats.asides.values().sum();
    crate::info!(
        "inject: cloned {} ({}), extracted by npm {} ({}){}, {} new in the store, {} checked against pacote, {} paths regrouped; npm {:.2}s, clone requests {:.2}s",
        stats.cloned,
        if kinds.is_empty() { "-".to_string() } else { kinds.join(", ") },
        stats.fallback_total(),
        if falls.is_empty() { "-".to_string() } else { falls.join(", ") },
        // Extractions outside the root: pacote reading a manifest.
        if asides > 0 { format!(", {asides} outside the root left to pacote") } else { String::new() },
        stats.filled.len(),
        stats.checked,
        stats.paths_regrouped,
        npm_secs,
        stats.wall_secs()
    );
    for e in &stats.errors {
        crate::info!("inject: {e}");
    }
    // Not an error, but not what the user set matchapack up for: say it even
    // when the per-step lines are hidden.
    // The helper could not read npm's install root, or npm's pacote had not
    // the shape the hook needs: npm ran without clones. Correct, and said.
    if let Some(why) = &stats.no_root {
        eprintln!("matchapack: no clones from the store, npm extracted the packages itself (could not learn npm's install root: {why})");
    } else if stats.hook.as_deref() == Some("not-installed") {
        eprintln!(
            "matchapack: no clones from the store, npm extracted the packages itself (the hook was not installed: {})",
            stats.hook_reason.as_deref().unwrap_or("?")
        );
    }
    if stats.fallbacks.contains_key("clone-unsupported") {
        let why = stats.errors.iter().find(|e| e.starts_with("clone-unsupported")).map(String::as_str).unwrap_or("clone-unsupported");
        eprintln!("matchapack: no clones from the store here, npm extracted the packages itself ({why})");
    }
    // Entries that differed from pacote's extraction were replaced and the
    // packages placed from pacote's: this install is right. What it says
    // about the store depends on who made them (serve.rs).
    for (kind, (n, first)) in &stats.differs {
        match kind.as_str() {
            // Expected: matchapack's own extractor (the classic engine's,
            // removed in PR C2; its entries stay in older stores) differs
            // from pacote for some odd tarballs.
            "extractor" => crate::info!("inject: {n} store entries made by matchapack's extractor differed from npm's extraction and were replaced (first: {})", first[0]),
            "changed" => eprintln!(
                "matchapack: {n} store entries had changed since npm's pacote made them; replaced with npm's extraction (first: {})",
                first[0]
            ),
            // Every entry is compared once under each pacote/tar version before
            // it is cloned, so nothing that differs was placed unchecked.
            _ => eprintln!(
                "matchapack: npm's {key} extracts {n} package(s) differently from what another pacote/tar version put in the store \
                 (first: {}); replaced with npm's extraction",
                first[0]
            ),
        }
    }
    let mut inject = stats.to_json();
    inject["key"] = serde_json::json!(key);
    // npm read a key of what the hook's extract resolved to other than the
    // three pacote's own value has (src/inject.js `resultTrap`): this npm
    // uses something of the result that a clone may not give as pacote
    // would. The tree must be npm's, so this is a broken install, also when
    // npm failed (the hook may be why): through the shell function npm ci
    // runs again, plain; the install family stops and says how to rebuild
    // (wrapper.rs).
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    if let Some(error) = trap_fired(&stats.result_unexpected) {
        return Err(InstallError::Trap { error, code, inject });
    }
    if !status.success() {
        // npm reported its own failure (a script, the lockfile, the network);
        // `npm ci` would have failed the same way. Not retried.
        return Err(InstallError::Npm { code, inject });
    }
    // The hook is a patch of a method npm does not promise to keep calling.
    // If a future npm extracts some other way, installs stay correct and
    // nothing here fails: the only sign is npm having placed packages the
    // hook never saw. Said out loud and logged (`matchapack report` counts it).
    // When the hook was knowingly not installed, the line above said so.
    let hook_absent = stats.hook.as_deref() == Some("not-installed");
    let command = stats.command.as_deref().unwrap_or("ci");
    // A package the hook did not see counts only when npm made its directory
    // in this run (the directory's birth time, where the filesystem gives
    // one): npm's own extraction always goes into a directory it made in the
    // run, while a package npm left in place whose package.json a lifecycle
    // script rewrote (a fix-up script, patch-package) keeps its old
    // directory. A package the hook placed is counted as before (a directory
    // clone carries the store entry's birth time).
    let made_in_this_run = |r: &std::path::Path, k: &String| match (stats.root_at, std::fs::symlink_metadata(r.join(k)).and_then(|m| m.created())) {
        (Some(t), Ok(b)) => b >= t,
        _ => true,
    };
    let expected = root
        .as_deref()
        .and_then(|r| expected_packages(r, stats.root_at).map(|e| e.into_iter().filter(|k| stats.seen.contains(k) || made_in_this_run(r, k)).collect::<BTreeSet<String>>()));
    if let Some(expected) = expected {
        // Compared by destination, not by count: pacote's temporary
        // extractions and npm's extra ones cannot hide a package the hook
        // missed.
        let installed = expected.len();
        let unseen = expected.iter().filter(|k| !stats.seen.contains(*k)).count();
        inject["installed"] = serde_json::json!(installed);
        inject["unseen"] = serde_json::json!(unseen);
        if unseen > 0 && !hook_absent {
            if command == "ci" {
                eprintln!(
                    "matchapack: npm installed {installed} packages, but only {} went through matchapack's hook: \
                     this npm may extract packages another way (the install is npm's own and complete; nothing was cloned for the rest)",
                    installed - unseen
                );
            } else {
                eprintln!(
                    "matchapack: npm installed {installed} packages in this run, but {unseen} of them did not go through matchapack's hook: \
                     this npm may extract packages another way (the install is npm's own and complete; nothing was cloned for them)"
                );
            }
        }
    }
    let bytes_logical = stats.used.par_iter().map(|i| store.entry_size(i)).sum();
    let bytes_store_new = stats.filled.iter().map(|i| store.entry_size(i)).sum();
    let secs = t0.elapsed().as_secs_f64();
    crate::info!("placed {} entries in {secs:.2}s", stats.cloned);
    Ok(Summary {
        placed: stats.cloned,
        secs,
        store_new: stats.filled.len(),
        bytes_logical,
        bytes_store_new,
        inject: Some(inject),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packages_npm_must_have_extracted_in_this_run() {
        for k in ["node_modules/a", "node_modules/@s/b", "node_modules/a/node_modules/b", "packages/w/node_modules/@s/b", "node_modules/.store/a@1.0.0/node_modules/a"] {
            assert!(is_package_key(k), "{k}");
        }
        // The root, link targets (workspaces, file: directories), and keys
        // that only look alike.
        for k in ["", "packages/w", "vendor/lib", "node_modules/", "node_modules/@s", "node_modules/@s/", "node_modules/a/lib", "my_node_modules/a", "node_modules/@s/b/c"] {
            assert!(!is_package_key(k), "{k}");
        }
        let d = crate::store::tests::scratch("inject-installed");
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        assert_eq!(expected_packages(&d, None), None, "no hidden lockfile");
        let hidden = serde_json::json!({ "name": "p", "lockfileVersion": 3, "requires": true, "packages": {
            "node_modules/a": { "version": "1.0.0", "integrity": "sha512-a" },
            "node_modules/a/node_modules/b": { "version": "1.0.0", "inBundle": true },
            "node_modules/@s/w": { "resolved": "packages/w", "link": true },
            "packages/w": { "version": "1.0.0" },
            "packages/w/node_modules/c": { "version": "2.0.0", "integrity": "sha512-c" },
            "node_modules/r": { "version": "1.0.0", "integrity": "sha512-r" },
            "../out/node_modules/o": { "version": "1.0.0", "integrity": "sha512-o" },
        }});
        std::fs::write(d.join("node_modules/.package-lock.json"), hidden.to_string()).unwrap();
        let set = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<BTreeSet<String>>());
        assert_eq!(expected_packages(&d, None), set(&["node_modules/a", "node_modules/r", "packages/w/node_modules/c"]), "every package npm extracts, inside the root");
        // Only packages placed in this run: `node_modules/a` new (made after
        // `since`), `packages/w/node_modules/c` left as it was (older), one
        // whose directory is not there is not counted either.
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let pkg = |k: &str, old: bool| {
            std::fs::create_dir_all(d.join(k)).unwrap();
            std::fs::write(d.join(k).join("package.json"), "{}").unwrap();
            if old {
                let f = std::fs::File::options().write(true).open(d.join(k).join("package.json")).unwrap();
                f.set_modified(since - std::time::Duration::from_secs(3600)).unwrap();
            }
        };
        pkg("node_modules/a", false);
        pkg("packages/w/node_modules/c", true);
        // c is left in place and gets a nested node_modules in this run: its
        // directory's time is new, its package.json's is not.
        std::fs::create_dir_all(d.join("packages/w/node_modules/c/node_modules")).unwrap();
        assert_eq!(expected_packages(&d, Some(since)), set(&["node_modules/a"]), "c is in place, untouched");
        // A package npm restores at the same version (its directory had been
        // removed by hand): extracted, so counted, whatever the hidden
        // lockfile said before.
        pkg("node_modules/r", false);
        assert_eq!(expected_packages(&d, Some(since)), set(&["node_modules/a", "node_modules/r"]), "r restored");
        std::fs::remove_dir_all(d.join("node_modules/r")).unwrap();
        // A key that is a symlink (npm 10, --install-strategy=linked: hoisted
        // paths point into node_modules/.store, which the hook saw) is not
        // counted, whatever the times.
        std::fs::rename(d.join("node_modules/a"), d.join("a-target")).unwrap();
        std::os::unix::fs::symlink(d.join("a-target"), d.join("node_modules/a")).unwrap();
        assert_eq!(expected_packages(&d, Some(since)), set(&[]), "a symlink key");
        std::fs::remove_file(d.join("node_modules/a")).unwrap();
        assert_eq!(expected_packages(&d, Some(since)), set(&[]), "a directory that is not there");
    }

    #[test]
    fn the_hidden_lockfile_is_complete_when_it_names_every_packages_source() {
        let d = crate::store::tests::scratch("inject-tree-entries");
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        assert!(tree_entries(&d).is_none(), "no hidden lockfile");
        let sri = format!("sha512-{}", "A".repeat(86) + "==");
        let write = |p: serde_json::Value| std::fs::write(d.join("node_modules/.package-lock.json"), serde_json::json!({ "lockfileVersion": 3, "packages": p }).to_string()).unwrap();
        // Integrities, a git package with only `resolved`, a link, a bundled
        // package and a workspace directory: complete.
        write(serde_json::json!({
            "node_modules/a": { "version": "1.0.0", "integrity": sri },
            "node_modules/g": { "version": "1.0.0", "resolved": "git+ssh://x/g.git#abc" },
            "node_modules/w": { "resolved": "packages/w", "link": true },
            "node_modules/a/node_modules/b": { "version": "1.0.0", "inBundle": true },
            "packages/w": { "version": "1.0.0" },
        }));
        let (keys, complete) = tree_entries(&d).unwrap();
        assert!(complete);
        assert_eq!(keys.len(), 1);
        // A package npm read from disk without a lockfile: neither.
        write(serde_json::json!({
            "node_modules/a": { "version": "1.0.0", "integrity": sri },
            "node_modules/kept": { "version": "1.0.0" },
        }));
        assert_eq!(tree_entries(&d).map(|(k, c)| (k.len(), c)), Some((1, false)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unexpected_read_of_the_hooks_result_is_a_broken_install() {
        let mut m = std::collections::BTreeMap::new();
        assert!(trap_fired(&m).is_none());
        m.insert("files".to_string(), 3);
        let e = format!("{:#}", trap_fired(&m).unwrap());
        assert!(e.contains("npm read `files` (3×) of what matchapack's hook returned"), "{e}");
    }
}
