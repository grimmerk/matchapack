mod clone;
mod diff;
mod gc;
mod inject;
mod lock;
mod npm;
mod plan;
mod platform;
mod resolve;
mod serve;
mod store;
mod trial;
mod util;
mod wrapper;
mod yarn1;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use plan::{Kind, PostInstall, Source};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;
use store::{Ensure, Integrity, Store};

#[derive(Parser)]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("MATCHAPACK_BUILD"), ")"), about = "Materialize node_modules from package-lock.json with copy-on-write clones")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Replace `npm ci`: wipe node_modules, clone every package from the store, run npm rebuild.
    Install {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Store root (default $MATCHAPACK_STORE or ~/.cache/matchapack/store).
        #[arg(long)]
        store: Option<PathBuf>,
        /// npm content cache (default ~/.npm/_cacache).
        #[arg(long)]
        npm_cache: Option<PathBuf>,
        /// Default: `dir` on macOS, `file` elsewhere (Linux has no directory reflink).
        #[arg(long, value_enum)]
        clone_mode: Option<clone::Mode>,
        /// Modification times of placed files: `now` (default) matches npm ci;
        /// `keep` is faster and keeps each content's store time (see NOTES.md).
        #[arg(long, value_enum, default_value = "now")]
        mtime: clone::Mtime,
        /// Pass --ignore-scripts to npm rebuild.
        #[arg(long)]
        ignore_scripts: bool,
        /// Skip the npm rebuild step entirely (no scripts, no bin links).
        #[arg(long)]
        no_rebuild: bool,
        /// With MATCHAPACK_ENGINE=inject: arguments for npm's own `npm ci`,
        /// after `--` (e.g. `matchapack install . -- --no-audit --omit=dev`).
        #[arg(last = true)]
        npm_args: Vec<String>,
    },
    /// Stand in for `npm <args>`: handle a bare `npm ci` from the store when it
    /// is safe to, otherwise run the real npm with the same arguments. Meant to
    /// be called by the shell wrapper (shell/matchapack.sh).
    Npm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stand in for `yarn <args>` (Yarn 1, macOS): a bare `yarn` or `yarn
    /// install` runs the user's Yarn with each file of its link step cloned
    /// from Yarn's cache; anything else, and every install matchapack
    /// declines, runs plain Yarn with the same arguments.
    Yarn {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the shell integration for an rc file (routes a bare `npm ci` through matchapack)
    ///
    /// zsh: ~/.zshrc, bash: ~/.bashrc, after the line that puts matchapack on
    /// PATH (otherwise the guard skips it silently):
    ///
    ///     command -v matchapack >/dev/null && eval "$(matchapack init)"
    ///
    /// fish: ~/.config/fish/config.fish:
    ///
    ///     type -q matchapack; and matchapack init | source
    ///
    /// Without SHELL, the shell that runs the command (the parent process) is
    /// used; name it to be explicit.
    #[command(verbatim_doc_comment)]
    Init {
        #[arg(value_parser = INIT_SHELLS)]
        shell: Option<String>,
    },
    /// Print the install plan (counts and every skipped entry with its reason); installs nothing.
    Plan {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Compare two project trees. Exit status: 0 identical, 1 they differ, 2 an error (nothing was decided).
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Relative paths to ignore (repeatable).
        #[arg(long)]
        ignore: Vec<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Print only the differing paths, one per line (for scripts).
        #[arg(long)]
        paths: bool,
        /// Print each difference as `<class>\t<path>` (for scripts; with
        /// --meta the classes of the report, otherwise only-a, only-b, differ).
        #[arg(long, conflicts_with = "paths")]
        classes: bool,
        /// Also compare, per path: mtime to the nanosecond, uid, gid and
        /// extended attribute names; report each class of difference.
        #[arg(long)]
        meta: bool,
        /// With --meta: A's install run, START:END in ns since the epoch. A
        /// time inside it may differ from B's when B's is inside B's run and
        /// of the same kind (see src/diff.rs, MetaOpts).
        #[arg(long, value_name = "START:END", requires = "meta", requires = "b_run")]
        a_run: Option<String>,
        /// With --meta: B's install run (as --a-run).
        #[arg(long, value_name = "START:END", requires = "meta", requires = "a_run")]
        b_run: Option<String>,
        /// With --a-run: the package manager cache A's run used; times its
        /// run wrote into it are told apart from other in-run times.
        #[arg(long, requires = "a_run")]
        a_cache: Option<PathBuf>,
        /// With --b-run: the cache B's run used.
        #[arg(long, requires = "b_run")]
        b_cache: Option<PathBuf>,
    },
    /// Summarise the trial logs: outcomes, reasons, time, space, verification samples.
    Report {
        /// Only the last N days.
        #[arg(long)]
        days: Option<f64>,
    },
    /// Print the store location and size; maintain it (gc, verify).
    Store {
        /// Store root (default $MATCHAPACK_STORE or ~/.cache/matchapack/store).
        #[arg(long, global = true)]
        store: Option<PathBuf>,
        #[command(subcommand)]
        action: Option<StoreCmd>,
    },
}

#[derive(Subcommand)]
enum StoreCmd {
    /// Remove the entries no existing project uses
    ///
    /// An entry is kept while a project installed from it still has its
    /// node_modules: every install records its directory and entries in
    /// <store>/projects/; projects installed before records existed are found
    /// through the trial log and their node_modules/.package-lock.json.
    /// Removing an entry never breaks an installed node_modules (clones are
    /// independent), but space comes back only for entries no node_modules
    /// still shares, and a later install re-extracts what it needs. Entries
    /// extracted in the last hour are kept. Refused while an install runs.
    #[command(verbatim_doc_comment)]
    Gc {
        /// Report what would be removed; remove nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare every entry with its tarball in the npm cache; exit 1 when one differs
    Verify {
        /// Remove the entries that differ (the next install re-extracts them).
        #[arg(long)]
        fix: bool,
        /// npm content cache (default ~/.npm/_cacache).
        #[arg(long)]
        npm_cache: Option<PathBuf>,
    },
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn default_store(s: Option<PathBuf>) -> PathBuf {
    s.or_else(|| std::env::var_os("MATCHAPACK_STORE").map(PathBuf::from))
        .unwrap_or_else(|| home().join(".cache/matchapack/store"))
}

fn default_clone_mode() -> clone::Mode {
    if cfg!(target_os = "macos") {
        clone::Mode::Dir
    } else {
        clone::Mode::File
    }
}

fn timed<T>(label: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let t = Instant::now();
    let r = f();
    util::record(label, t.elapsed().as_secs_f64());
    info!("{label}: {:.2}s", t.elapsed().as_secs_f64());
    r
}

fn npm_cache_add(root: &Path, urls: &[String]) -> Result<()> {
    for chunk in urls.chunks(200) {
        let st = util::npm_cmd()
            .arg("cache")
            .arg("add")
            .args(chunk)
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .status()
            .context("running npm cache add")?;
        if !st.success() {
            bail!("npm cache add failed ({st})");
        }
    }
    Ok(())
}

pub struct InstallOpts {
    pub dir: PathBuf,
    pub store: PathBuf,
    pub cacache: PathBuf,
    pub clone_mode: clone::Mode,
    pub mtime: clone::Mtime,
    pub ignore_scripts: bool,
    pub no_rebuild: bool,
    /// The arguments after `ci` in the user's `npm ci` (the wrapper accepts
    /// only passive flags), passed on to npm for the post-install steps.
    pub npm_args: Vec<String>,
}

pub struct Summary {
    pub placed: usize,
    pub secs: f64,
    /// Store entries extracted during this install.
    pub store_new: usize,
    /// File bytes this install placed as clones (what npm would have written).
    pub bytes_logical: u64,
    /// File bytes newly added to the store by this install.
    pub bytes_store_new: u64,
    /// The inject engine's counts (clones by placement, packages npm
    /// extracted itself and why), for the log; None for the classic engine.
    pub inject: Option<serde_json::Value>,
}

/// How an install ended when it did not succeed.
pub enum InstallError {
    /// Refused before anything in the project changed: npm can take over as is.
    Declined(anyhow::Error),
    /// A lifecycle script failed; `npm ci` would have failed the same way.
    Scripts(anyhow::Error),
    /// matchapack itself failed after node_modules was changed.
    Broke(anyhow::Error),
    /// Inject engine: npm ran and exited with this status, having reported
    /// the failure itself; `npm ci` would have failed the same way.
    Npm { code: i32, inject: serde_json::Value },
}

impl InstallError {
    pub fn into_inner(self) -> anyhow::Error {
        match self {
            InstallError::Declined(e) | InstallError::Scripts(e) | InstallError::Broke(e) => e,
            InstallError::Npm { code, .. } => anyhow::anyhow!("npm exited with status {code}"),
        }
    }
}

type Prepared = (PathBuf, lock::Lockfile, plan::Plan, Store, Vec<Integrity>);

/// Everything that can fail without touching the project: plan, packument
/// resolution, store fill (writes only the store and npm's cache), and the
/// reflink probe (writes only inside node_modules).
fn prepare(o: &InstallOpts) -> Result<Prepared> {
    let root = std::fs::canonicalize(&o.dir)?;
    // npm's own modules run the root scripts and judge semver ranges; check now,
    // not after node_modules has been wiped.
    // The version gate holds for every install, not only the `npm ci` wrapper:
    // the ideal tree and flags come from npm's internals (ideal-tree.js).
    wrapper::npm_version_gate().map_err(|e| anyhow::anyhow!(e))?;
    let npm_dir = util::npm_dir().context("cannot locate the npm installation on PATH")?;
    for m in ["node_modules/@npmcli/run-script", "node_modules/semver", "node_modules/@npmcli/arborist", "node_modules/@npmcli/config", "lib/npm.js", "lib/commands/rebuild.js"] {
        if !npm_dir.join(m).exists() {
            bail!("npm installation {} has no {m}", npm_dir.display());
        }
    }
    let (lf, mut plan) = timed("plan", || {
        let mut lf = lock::Lockfile::load(&root)?;
        let ideal = npm::check_in_sync(&root, &lf)?;
        if let Some(i) = &ideal {
            npm::apply_flags(&mut lf, i);
        }
        let plan = npm::build_plan(&lf, ideal.as_ref().map(|i| &i.paths))?;
        Ok((lf, plan))
    })?;
    info!("{} lockfile entries skipped on this platform (MATCHAPACK_VERBOSE=1 lists them)", plan.skipped.len());
    if std::env::var_os("MATCHAPACK_VERBOSE").is_some() {
        for (p, why) in &plan.skipped {
            eprintln!("[matchapack] skip {p}: {why}");
        }
    }
    let n = timed("resolve name@version entries", || resolve::resolve(&mut plan, &root, &o.cacache))?;
    if n > 0 {
        info!("resolved {n} lockfile entries without `resolved` from packuments");
    }
    let store = Store::new(o.store.clone(), o.cacache.clone())?;

    // Make sure every package is in the store.
    let pkgs: Vec<(Integrity, String)> = {
        let mut seen = HashMap::new();
        for e in &plan.entries {
            if let Kind::Package { integrity: Some(integrity), source: Source::Tarball { url } } = &e.kind {
                let i = Integrity::parse(integrity)?;
                seen.entry(i).or_insert_with(|| url.clone());
            }
        }
        seen.into_iter().collect()
    };
    let mut extracted_now: Vec<Integrity> = vec![];
    let (missing, extracted) = timed("store", || {
        let res: Vec<Result<(Option<String>, bool)>> = pkgs
            .par_iter()
            .map(|(i, url)| match store.ensure(i, url)? {
                Ensure::Present => Ok((None, false)),
                Ensure::Extracted => Ok((None, true)),
                Ensure::Missing => Ok((Some(url.clone()), false)),
            })
            .collect();
        let mut missing = vec![];
        let mut extracted = 0;
        for (r, (i, _)) in res.into_iter().zip(&pkgs) {
            let (m, x) = r?;
            if let Some(u) = m {
                missing.push(u);
            }
            if x {
                extracted += 1;
                extracted_now.push(i.clone());
            }
        }
        Ok((missing, extracted))
    })?;
    info!("{} distinct packages: {} extracted now, {} not in npm cache", pkgs.len(), extracted, missing.len());
    if !missing.is_empty() {
        timed("npm cache add", || npm_cache_add(&root, &missing))?;
        let by_url: HashMap<&String, &Integrity> = pkgs.iter().map(|(i, u)| (u, i)).collect();
        for u in &missing {
            match store.ensure(by_url[u], u)? {
                Ensure::Missing => bail!("still not in npm cache after `npm cache add`: {u}"),
                Ensure::Extracted => extracted_now.push(by_url[u].clone()),
                Ensure::Present => {}
            }
        }
    }

    // Checked as late as possible before the first write (the probe below),
    // after the slow steps (resolution, store fill), to keep the window in
    // which something could swap a directory for a symlink short.
    check_real_paths(&root, &plan)?;

    // Refuse before deleting anything if this filesystem cannot clone.
    if let Some((i, _)) = pkgs.first() {
        clone::probe(&store.dir_for(i), &root)?;
    }
    Ok((root, lf, plan, store, extracted_now))
}

/// The part that changes the project: wipe, clone, mtime, scripts.
fn place(
    root: &Path,
    store: &Store,
    mode: clone::Mode,
    mtime: clone::Mtime,
    post: Option<&dyn PostInstall>,
    plan: &plan::Plan,
) -> Result<()> {
    // Wipe what npm ci wipes, the way ci.js does: every entry INSIDE each
    // node_modules goes, the directory itself stays (it matters when nothing is
    // installed: npm leaves an empty node_modules, or none if there was none).
    // The entries are removed in parallel: on APFS the removal is the largest
    // fixed cost of a reinstall (NOTES "Removal and the fixed cost").
    timed("remove old node_modules", || {
        let mut old: Vec<PathBuf> = vec![];
        for r in &plan.owned_roots {
            let nm = root.join(r).join("node_modules");
            let Ok(entries) = std::fs::read_dir(&nm) else { continue };
            for ent in entries {
                old.push(ent?.path());
            }
        }
        remove_paths_par(old)
    })?;

    // Place packages, parents before children; one depth level at a time.
    timed("clone", || {
        let mut levels: BTreeMap<usize, Vec<&plan::PlanEntry>> = BTreeMap::new();
        for e in &plan.entries {
            levels.entry(lock::depth(&e.install_path)).or_default().push(e);
        }
        for (_, es) in levels {
            es.par_iter().try_for_each(|e| -> Result<()> {
                let dst = root.join(&e.install_path);
                std::fs::create_dir_all(dst.parent().unwrap())?;
                match &e.kind {
                    Kind::Package { integrity, .. } => {
                        let integrity = integrity.as_deref().context("unresolved package in plan")?;
                        let i = Integrity::parse(integrity)?;
                        let src = store.dir_for(&i);
                        // Shared, as the inject engine's clones: no
                        // replacement of the entry while it is cloned.
                        let _shared = store.entry_lock(&i, false)?;
                        clone::clone_tree(&src, &dst, mode)
                    }
                    Kind::Link { target } => {
                        let rel = relative(&root.join(target), dst.parent().unwrap());
                        std::os::unix::fs::symlink(rel, &dst)?;
                        Ok(())
                    }
                }
            })?;
        }
        Ok(())
    })?;

    // npm ci leaves every extracted file with the install time; a clone carries
    // the store entry's time. One walk per owned node_modules covers every
    // placed file exactly once; symlinks (workspace links) are not followed.
    if mtime == clone::Mtime::Now {
        let dirs: Vec<PathBuf> = plan
            .owned_roots
            .iter()
            .map(|r| root.join(r).join("node_modules"))
            .filter(|p| p.is_dir())
            .collect();
        let n = timed("touch (mtime now)", || clone::touch_trees(&dirs))?;
        info!("touched {n} paths");
    }

    // Scripts, bins, package-manager metadata.
    if let Some(p) = post {
        timed("post-install", || p.run(root, plan))?;
    }
    Ok(())
}

fn remove_path(p: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(p).with_context(|| format!("removing {}", p.display()))?;
    let r = if m.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    r.with_context(|| format!("removing {}", p.display()))
}

/// Remove paths in parallel, each as a whole: a directory (a scope directory
/// included) through remove_dir_all, which never follows a symlink inside the
/// tree even when one is swapped in during the removal; a symlink as a link.
/// Not split further: a path queued below a directory could be redirected if
/// that directory were replaced by a symlink in the meantime.
fn remove_paths_par(paths: Vec<PathBuf>) -> Result<()> {
    paths.par_iter().try_for_each(|p| remove_path(p))
}

/// Every directory matchapack will empty or write into must really be inside
/// the project. The lockfile paths are plain relative names (checked when the
/// plan is built), but a directory on the way can be a symlink: the project's
/// node_modules, a workspace directory, a `file:` dependency directory. Below
/// `<dir>/node_modules` everything is emptied and recreated by matchapack, so
/// checking each `<dir>` (its real path inside the project) and its
/// node_modules (not a symlink) covers every write. In-project link targets
/// are checked too even when nothing is placed under them: `npm rebuild`
/// links their bins and chmods the files they point at. Runs right before the
/// first write (the clone probe), so a refusal leaves the project untouched.
/// Not covered: another process swapping a directory for a symlink between
/// this check and the writes (npm ci has the same window).
fn check_real_paths(root: &Path, plan: &plan::Plan) -> Result<()> {
    let mut bases: Vec<&str> = plan.owned_roots.iter().map(|s| s.as_str()).collect();
    for e in &plan.entries {
        if let Kind::Link { target } = &e.kind {
            if crate::util::safe_relative(target) && !bases.contains(&target.as_str()) {
                bases.push(target.as_str());
            }
        }
    }
    for e in &plan.entries {
        let base = match e.install_path.find("node_modules/") {
            Some(0) => "",
            Some(i) => e.install_path[..i].trim_end_matches('/'),
            None => continue,
        };
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    for b in bases {
        let dir = root.join(b);
        // A directory that does not exist yet would be created under its
        // nearest existing ancestor: check that one.
        let mut existing = dir.as_path();
        while !existing.exists() {
            match existing.parent() {
                Some(p) => existing = p,
                None => break,
            }
        }
        let real = std::fs::canonicalize(existing).with_context(|| format!("resolving {}", existing.display()))?;
        if !real.starts_with(root) {
            bail!("{} resolves outside the project ({}), through a symlink", show(b), real.display());
        }
        let nm = dir.join("node_modules");
        if std::fs::symlink_metadata(&nm).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("{}/node_modules is a symlink; npm ci replaces it, matchapack does not", show(b));
        }
    }
    Ok(())
}

fn show(base: &str) -> &str {
    if base.is_empty() { "the project directory" } else { base }
}

const INIT_SHELLS: [&str; 3] = ["zsh", "bash", "fish"];

/// The shell `matchapack init` was started from: its parent process. In
/// `eval "$(matchapack init)"` that is the zsh or bash reading the rc file, in
/// `matchapack init | source` the fish. Not $SHELL, which names the login shell
/// even inside another one.
fn parent_shell() -> Option<&'static str> {
    let ppid = std::os::unix::process::parent_id();
    let out = std::process::Command::new("ps").args(["-o", "comm=", "-p", &ppid.to_string()]).output().ok()?;
    shell_from_comm(String::from_utf8_lossy(&out.stdout).trim())
}

/// `ps -o comm=` gives `zsh`, `/bin/zsh`, or `-zsh` for a login shell.
fn shell_from_comm(comm: &str) -> Option<&'static str> {
    let name = comm.rsplit('/').next().unwrap_or(comm).trim_start_matches('-');
    INIT_SHELLS.iter().copied().find(|s| *s == name)
}

pub fn run_install(o: &InstallOpts) -> std::result::Result<Summary, InstallError> {
    let t0 = Instant::now();
    let (root, lf, plan, store, extracted_now) = prepare(o).map_err(InstallError::Declined)?;
    // Before anything is placed: from here on the project holds clones of
    // these entries, even if the install fails part way.
    let used: Vec<Integrity> = plan
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Package { integrity: Some(i), .. } => Integrity::parse(i).ok(),
            _ => None,
        })
        .collect();
    if let Err(e) = store.record_project(&root, &used) {
        info!("could not record the project for `matchapack store gc`: {e:#}");
    }
    let rebuild = npm::NpmRebuild { lock_raw: lf.raw.clone(), ignore_scripts: o.ignore_scripts, npm_args: o.npm_args.clone() };
    let post: Option<&dyn PostInstall> = if o.no_rebuild { None } else { Some(&rebuild) };
    match place(&root, &store, o.clone_mode, o.mtime, post, &plan) {
        Ok(()) => {
            let secs = t0.elapsed().as_secs_f64();
            info!("placed {} entries in {secs:.2}s", plan.entries.len());
            let bytes_logical = plan
                .entries
                .iter()
                .filter_map(|e| match &e.kind {
                    Kind::Package { integrity: Some(i), .. } => Integrity::parse(i).ok(),
                    _ => None,
                })
                .map(|i| store.entry_size(&i))
                .sum();
            let bytes_store_new = extracted_now.iter().map(|i| store.entry_size(i)).sum();
            Ok(Summary { placed: plan.entries.len(), secs, store_new: extracted_now.len(), bytes_logical, bytes_store_new, inject: None })
        }
        Err(e) if e.downcast_ref::<util::ScriptFailure>().is_some() => Err(InstallError::Scripts(e)),
        Err(e) => Err(InstallError::Broke(e)),
    }
}

/// Relative path from directory `from` to `to` (both absolute, no symlinks resolved).
fn relative(to: &Path, from: &Path) -> PathBuf {
    let t: Vec<_> = to.components().collect();
    let f: Vec<_> = from.components().collect();
    let common = t.iter().zip(&f).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c);
    }
    out
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Install { dir, store, npm_cache, clone_mode, mtime, ignore_scripts, no_rebuild, npm_args: extra }
            if inject::engine().map_err(|e| anyhow::anyhow!(e))? == inject::Engine::Inject =>
        {
            // npm does the whole install; these belong to the classic engine.
            if no_rebuild {
                bail!("--no-rebuild is not available with MATCHAPACK_ENGINE=inject (npm ci runs as a whole)");
            }
            if mtime != clone::Mtime::Now {
                bail!("--mtime keep is not available with MATCHAPACK_ENGINE=inject");
            }
            if npm_cache.is_some() {
                info!("--npm-cache is ignored with MATCHAPACK_ENGINE=inject: npm's own cache setting decides");
            }
            let mut npm_args = vec!["ci".to_string()];
            if ignore_scripts {
                npm_args.push("--ignore-scripts".into());
            }
            npm_args.extend(extra);
            let o = inject::InjectOpts { dir, store: default_store(store), clone_mode: clone_mode.unwrap_or_else(default_clone_mode), npm_args };
            let r = inject::run(&o);
            util::remove_run_dir();
            match r {
                Ok(_) => {}
                // npm printed its own error; pass its status on.
                Err(InstallError::Npm { code, .. }) => std::process::exit(code),
                Err(e) => return Err(e.into_inner()),
            }
        }
        Cmd::Install { dir, store, npm_cache, clone_mode, mtime, ignore_scripts, no_rebuild, npm_args } => {
            if !npm_args.is_empty() {
                bail!("arguments for npm (after --) need MATCHAPACK_ENGINE=inject; the classic engine reads npm's settings from its config");
            }
            let o = InstallOpts {
                dir,
                store: default_store(store),
                cacache: npm_cache.unwrap_or_else(|| home().join(".npm/_cacache")),
                clone_mode: clone_mode.unwrap_or_else(default_clone_mode),
                mtime,
                ignore_scripts,
                no_rebuild,
                npm_args: if ignore_scripts { vec!["--ignore-scripts".into()] } else { vec![] },
            };
            let r = run_install(&o).map_err(InstallError::into_inner);
            util::remove_run_dir();
            r?;
        }
        Cmd::Npm { args } => {
            let code = wrapper::run(args);
            util::remove_run_dir();
            std::process::exit(code);
        }
        Cmd::Yarn { args } => {
            let code = yarn1::cli(args);
            std::process::exit(code);
        }
        Cmd::Init { shell } => {
            let shell = match shell {
                Some(s) => s,
                None => parent_shell().map(String::from).with_context(|| {
                    format!(
                        "could not tell which shell runs this; name it: matchapack init <{}>\n  \
                         zsh (~/.zshrc) / bash (~/.bashrc): command -v matchapack >/dev/null && eval \"$(matchapack init zsh)\"\n  \
                         fish (~/.config/fish/config.fish): type -q matchapack; and matchapack init fish | source",
                        INIT_SHELLS.join("|")
                    )
                })?,
            };
            // The files in shell/ are the one definition; the binary carries
            // them so the rc line needs no checkout of this repository.
            print!("{}", if shell == "fish" { include_str!("../shell/matchapack.fish") } else { include_str!("../shell/matchapack.sh") });
        }
        Cmd::Plan { dir } => {
            let root = std::fs::canonicalize(&dir)?;
            let mut lf = lock::Lockfile::load(&root)?;
            let ideal = npm::check_in_sync(&root, &lf)?;
            if let Some(i) = &ideal {
                npm::apply_flags(&mut lf, i);
            }
            let plan = npm::build_plan(&lf, ideal.as_ref().map(|i| &i.paths))?;
            let (mut pkgs, mut links, mut registry) = (0, 0, 0);
            for e in &plan.entries {
                match &e.kind {
                    Kind::Link { .. } => links += 1,
                    Kind::Package { source: Source::Registry { .. }, .. } => registry += 1,
                    Kind::Package { .. } => pkgs += 1,
                }
            }
            println!("entries {} ({pkgs} tarball, {registry} name@version, {links} links), skipped {}", plan.entries.len(), plan.skipped.len());
            for (p, why) in &plan.skipped {
                println!("skip {p}: {why}");
            }
        }
        Cmd::Diff { a, b, ignore, limit, paths, classes, meta, a_run, b_run, a_cache, b_cache } => {
            let opts = if meta {
                let parse = |s: Option<String>| {
                    s.as_deref().map(diff::Run::parse).transpose().unwrap_or_else(|e| {
                        eprintln!("matchapack diff: error: {e:#}");
                        std::process::exit(2)
                    })
                };
                Some(diff::MetaOpts {
                    a_run: parse(a_run),
                    b_run: parse(b_run),
                    a_cache,
                    b_cache,
                })
            } else {
                None
            };
            let r = match diff::diff_with(&a, &b, &ignore, opts.as_ref()) {
                Ok(r) => r,
                Err(e) => {
                    // Not 1: a script must never read an error as "the trees differ".
                    eprintln!("matchapack diff: error: {e:#}");
                    std::process::exit(2);
                }
            };
            if classes {
                r.print_classes();
            } else if paths {
                r.print_paths();
            } else {
                r.print(limit);
            }
            if !r.ok() {
                std::process::exit(1);
            }
        }
        Cmd::Report { days } => {
            let store = Store::new(default_store(None), home().join(".npm/_cacache"))?;
            trial::report(days, &store)?;
        }
        Cmd::Store { store, action } => {
            let s = default_store(store);
            let cacache = home().join(".npm/_cacache");
            match action {
                None => {
                    let n = walkdir::WalkDir::new(s.join(store::LAYOUT)).min_depth(3).max_depth(3).into_iter().count();
                    let p = std::fs::read_dir(s.join("projects")).map(|d| d.count()).unwrap_or(0);
                    println!("{} ({n} packages, {p} projects recorded)", s.display());
                }
                Some(StoreCmd::Gc { dry_run }) => {
                    let st = if dry_run { Store::new(s, cacache)? } else { Store::exclusive(s, cacache)? };
                    let free0 = store::free_bytes(&st.root);
                    let r = gc::gc(&st, &trial::log_path(), dry_run)?;
                    let free1 = store::free_bytes(&st.root);
                    let gib = |b: u64| format!("{:.2} GiB", b as f64 / 1024f64.powi(3));
                    println!(
                        "projects in use: {} ({} recorded, {} from the trial log); records of removed projects {}: {}",
                        r.projects.0 + r.projects.1,
                        r.projects.0,
                        r.projects.1,
                        if dry_run { "to drop" } else { "dropped" },
                        r.records_dropped
                    );
                    println!(
                        "{} {} of {} entries, {} of files{}",
                        if dry_run { "would remove" } else { "removed" },
                        r.removed,
                        r.entries,
                        gib(r.bytes_removed),
                        if r.recent > 0 { format!(" (kept {} unused entries extracted in the last hour)", r.recent) } else { String::new() }
                    );
                    // The file bytes above are not the space freed: a removed
                    // entry whose blocks a node_modules still shares (a clone
                    // of it) frees nothing. The volume's free space tells.
                    if r.removed > 0 {
                        if dry_run {
                            println!("space comes back only for entries no node_modules still shares blocks with; the real run reports the volume's free space before and after");
                        } else if let (Some(a), Some(b)) = (free0, free1) {
                            println!(
                                "free space on the store's volume: {} before, {} after ({:+.2} GiB; other activity on the volume counts too)",
                                gib(a),
                                gib(b),
                                (b as f64 - a as f64) / 1024f64.powi(3)
                            );
                        }
                    }
                }
                Some(StoreCmd::Verify { fix, npm_cache }) => {
                    let cacache = npm_cache.unwrap_or(cacache);
                    let st = if fix { Store::exclusive(s, cacache)? } else { Store::new(s, cacache)? };
                    let all = st.entries();
                    let checks: Vec<(Integrity, store::Check)> = all
                        .par_iter()
                        .map(|i| st.verify_entry(i).map(|c| (i.clone(), c)))
                        .collect::<Result<_>>()?;
                    let bad: Vec<&(Integrity, store::Check)> = checks.iter().filter(|(_, c)| matches!(c, store::Check::Differs(_))).collect();
                    let missing = checks.iter().filter(|(_, c)| *c == store::Check::NoTarball).count();
                    println!(
                        "{} entries: {} match their tarball, {} differ, {missing} not checked (tarball no longer in the npm cache)",
                        all.len(),
                        all.len() - bad.len() - missing,
                        bad.len()
                    );
                    for (i, c) in &bad {
                        if let store::Check::Differs(why) = c {
                            println!("  {}/{}…: {why}", i.algo, &i.hex[..16]);
                        }
                    }
                    if !bad.is_empty() {
                        if fix {
                            for (i, _) in &bad {
                                st.remove_entry(i)?;
                            }
                            println!("removed {} entries; the next install that needs one re-extracts it", bad.len());
                        } else {
                            println!("`matchapack store verify --fix` removes them (the next install re-extracts them)");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::shell_from_comm;

    #[test]
    fn shell_names_from_ps() {
        assert_eq!(shell_from_comm("zsh"), Some("zsh"));
        assert_eq!(shell_from_comm("/bin/zsh"), Some("zsh"));
        assert_eq!(shell_from_comm("-zsh"), Some("zsh"));
        assert_eq!(shell_from_comm("/opt/homebrew/bin/fish"), Some("fish"));
        assert_eq!(shell_from_comm("-bash"), Some("bash"));
        assert_eq!(shell_from_comm("sh"), None);
        assert_eq!(shell_from_comm("tmux"), None);
        assert_eq!(shell_from_comm(""), None);
    }
}
