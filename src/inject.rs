//! The inject engine (`THINCLONE_ENGINE=inject`; NOTES "Inject engine",
//! grimmerk/thinclone#33): npm runs its own `npm ci`, in a node child started
//! here, and only the per-package extraction is replaced by a clone from the
//! store. This process keeps what must be decided before anything starts
//! (npm version gate, umask, where the project's node_modules really is),
//! holds the store's shared lock for the whole run, and serves the clone
//! requests of the hook (src/inject.js) over a socket (src/serve.rs).
//! Everything else is npm's: reading its config and arguments, validating the
//! lockfile, removing the old node_modules, scripts, bin links, the hidden
//! lockfile, the audit.

use crate::serve::Server;
use crate::store::Store;
use crate::{clone, util, wrapper, InstallError, Summary};
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::time::Instant;

/// Which engine installs. The classic engine (thinclone orders every step of
/// `npm ci` itself) stays the default until the inject engine replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Classic,
    Inject,
}

/// From THINCLONE_ENGINE: unset, empty or `classic` → Classic; `inject`.
/// Anything else is an error, never a silent default.
pub fn engine() -> std::result::Result<Engine, String> {
    match std::env::var("THINCLONE_ENGINE").as_deref() {
        Err(_) | Ok("") | Ok("classic") => Ok(Engine::Classic),
        Ok("inject") => Ok(Engine::Inject),
        Ok(other) => Err(format!("THINCLONE_ENGINE={other} is not an engine (classic, inject)")),
    }
}

pub struct InjectOpts {
    pub dir: PathBuf,
    pub store: PathBuf,
    pub clone_mode: clone::Mode,
    /// npm's arguments, command first (`ci --no-audit`), passed on unchanged.
    pub npm_args: Vec<String>,
}

/// The fd number the node child finds the socket at.
const CHILD_FD: i32 = 3;

/// The checks that must hold before npm starts. A refusal changes nothing in
/// the project, so the caller can run plain npm.
fn gates(o: &InjectOpts) -> Result<(PathBuf, PathBuf)> {
    let root = std::fs::canonicalize(&o.dir).with_context(|| format!("resolving {}", o.dir.display()))?;
    wrapper::npm_version_gate().map_err(|e| anyhow!(e))?;
    // Clones carry the modes the store was extracted with under umask 022;
    // npm's files follow the process umask. (The hook checks it again per
    // package; here it also guards what this process extracts into the store.)
    let umask = wrapper::process_umask();
    if umask != 0o022 {
        bail!("umask is {umask:03o}, the store assumes 022");
    }
    // npm ci replaces a node_modules that is a symlink; every clone into it
    // would be refused one by one (its real path is outside the project).
    if std::fs::symlink_metadata(root.join("node_modules")).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("the project directory's node_modules is a symlink; npm ci replaces it, thinclone does not");
    }
    let npm_dir = util::npm_dir().context("cannot locate the npm installation on PATH")?;
    for m in ["lib/cli.js", "bin/npm-cli.js", "node_modules/pacote/package.json"] {
        if !npm_dir.join(m).exists() {
            bail!("npm installation {} has no {m}", npm_dir.display());
        }
    }
    Ok((root, npm_dir))
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

/// How many packages npm left installed that it had to extract: the entries
/// of the hidden lockfile it wrote that sit in a node_modules, without links
/// and bundled packages (those come inside their bundler's tarball). Every
/// one of them went through pacote's extract, so the hook should have seen
/// at least as many.
fn installed_packages(root: &std::path::Path) -> Option<usize> {
    let text = std::fs::read(root.join("node_modules/.package-lock.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&text).ok()?;
    let flag = |p: &serde_json::Value, k: &str| p.get(k).and_then(|x| x.as_bool()) == Some(true);
    Some(v.get("packages")?.as_object()?.iter().filter(|(k, p)| is_package_key(k) && !flag(p, "link") && !flag(p, "inBundle")).count())
}

fn threads() -> usize {
    std::env::var("THINCLONE_INJECT_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
}

pub fn run(o: &InjectOpts) -> std::result::Result<Summary, InstallError> {
    let t0 = Instant::now();
    let (root, npm_dir) = gates(o).map_err(InstallError::Declined)?;
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
    let server = match Server::new(&store, root.clone(), o.clone_mode) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&helper);
            return Err(InstallError::Declined(e));
        }
    };

    // Not util::node_cmd: npm runs here as the user's `npm ci`, and the
    // scripts it starts must see the user's environment, without a
    // THINCLONE=0 of ours. (The helper removes THINCLONE_INJECT_FD.)
    let mut cmd = std::process::Command::new("node");
    cmd.arg(&helper).arg(&npm_dir).args(&o.npm_args).current_dir(&root).env("THINCLONE_INJECT_FD", CHILD_FD.to_string());
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
    let t_npm = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&helper);
            return Err(InstallError::Declined(anyhow!("could not start node: {e}")));
        }
    };
    // Only the child holds its end now: when it exits, the server sees the
    // end of the stream.
    drop(theirs);
    let closer = ours.try_clone();
    let status = std::thread::scope(|s| {
        let srv = &server;
        let serving = s.spawn(move || srv.serve(ours, threads()));
        let status = child.wait();
        // The child is gone: drop what is still queued and end the stream,
        // also when a process the child started still holds the socket.
        server.stop();
        if let Ok(c) = &closer {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        let _ = serving.join();
        status
    });
    let npm_secs = t_npm.elapsed().as_secs_f64();
    let _ = std::fs::remove_file(&helper);
    let stats = std::mem::take(&mut *server.stats.lock().unwrap());
    drop(server);

    let status = match status {
        Ok(s) => s,
        Err(e) => return Err(InstallError::Broke(anyhow!("waiting for npm: {e}"))),
    };
    if stats.hello.is_none() {
        // The helper stopped before npm started (it says why on stderr):
        // nothing in the project has changed.
        return Err(InstallError::Declined(anyhow!("the inject helper could not start npm ({status})")));
    }
    // From here on the project holds clones of these entries, whatever npm's
    // exit status. gc cannot run before this record is written: it needs the
    // store's exclusive lock, which `store` still holds shared.
    if let Err(e) = store.record_project(&root, &stats.used) {
        crate::info!("could not record the project for `thinclone store gc`: {e:#}");
    }
    util::record("npm", npm_secs);
    util::record("clone requests (first to last)", stats.wall_secs());
    for (k, (_, place, touch)) in &stats.kinds {
        util::record(&format!("{k}: place (thread sum)"), *place);
        util::record(&format!("{k}: touch (thread sum)"), *touch);
    }
    let kinds: Vec<String> = stats.kinds.iter().map(|(k, v)| format!("{k} {}", v.0)).collect();
    let falls: Vec<String> = stats.fallbacks.iter().map(|(k, n)| format!("{k} {n}")).collect();
    crate::info!(
        "inject: cloned {} ({}), extracted by npm {} ({}), {} new in the store; npm {:.2}s, clone requests {:.2}s",
        stats.cloned,
        if kinds.is_empty() { "-".to_string() } else { kinds.join(", ") },
        stats.fallback_total(),
        if falls.is_empty() { "-".to_string() } else { falls.join(", ") },
        stats.filled.len(),
        npm_secs,
        stats.wall_secs()
    );
    for e in &stats.errors {
        crate::info!("inject: {e}");
    }
    // Not an error, but not what the user set thinclone up for: say it even
    // when the per-step lines are hidden.
    if stats.fallbacks.contains_key("clone-unsupported") {
        let why = stats.errors.iter().find(|e| e.starts_with("clone-unsupported")).map(String::as_str).unwrap_or("clone-unsupported");
        eprintln!("thinclone: no clones from the store here, npm extracted the packages itself ({why})");
    }
    let mut inject = stats.to_json();
    if !status.success() {
        // npm reported its own failure (a script, the lockfile, the network);
        // `npm ci` would have failed the same way. Not retried.
        let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        return Err(InstallError::Npm { code, inject });
    }
    // The hook is a patch of a method npm does not promise to keep calling.
    // If a future npm extracts some other way, installs stay correct and
    // nothing here fails: the only sign is npm having placed packages the
    // hook never saw. Said out loud and logged (`thinclone report` counts it).
    if let Some(installed) = installed_packages(&root) {
        let seen = stats.cloned + stats.fallback_total();
        let unseen = installed.saturating_sub(seen);
        inject["installed"] = serde_json::json!(installed);
        inject["unseen"] = serde_json::json!(unseen);
        if unseen > 0 {
            eprintln!(
                "thinclone: npm installed {installed} packages, but only {seen} went through thinclone's hook: \
                 this npm may extract packages another way (the install is npm's own and complete; nothing was cloned for the rest)"
            );
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
    fn installed_packages_counts_what_npm_extracted() {
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
        assert_eq!(installed_packages(&d), None, "no hidden lockfile");
        let hidden = serde_json::json!({ "name": "p", "lockfileVersion": 3, "requires": true, "packages": {
            "node_modules/a": { "version": "1.0.0" },
            "node_modules/a/node_modules/b": { "version": "1.0.0", "inBundle": true },
            "node_modules/@s/w": { "resolved": "packages/w", "link": true },
            "packages/w": { "version": "1.0.0" },
            "packages/w/node_modules/c": { "version": "2.0.0" },
        }});
        std::fs::write(d.join("node_modules/.package-lock.json"), hidden.to_string()).unwrap();
        assert_eq!(installed_packages(&d), Some(2), "a and packages/w/node_modules/c");
    }
}
