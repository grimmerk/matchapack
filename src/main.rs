mod clone;
mod diff;
mod lock;
mod npm;
mod plan;
mod platform;
mod resolve;
mod store;
mod trial;
mod util;
mod wrapper;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use plan::{Kind, PostInstall, Source};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;
use store::{Ensure, Integrity, Store};

#[derive(Parser)]
#[command(version, about = "Materialize node_modules from package-lock.json with copy-on-write clones")]
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
        /// Store root (default $THINCLONE_STORE or ~/.cache/thinclone/store).
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
    },
    /// Stand in for `npm <args>`: handle a bare `npm ci` from the store when it
    /// is safe to, otherwise run the real npm with the same arguments. Meant to
    /// be called by the shell wrapper (shell/thinclone.sh).
    Npm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the shell integration for an rc file (routes a bare `npm ci` through thinclone)
    ///
    /// zsh: ~/.zshrc, bash: ~/.bashrc, after the line that puts thinclone on
    /// PATH (otherwise the guard skips it silently):
    ///
    ///     command -v thinclone >/dev/null && eval "$(thinclone init)"
    ///
    /// fish: ~/.config/fish/config.fish:
    ///
    ///     type -q thinclone; and thinclone init | source
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
    /// Compare two project trees; exit 1 when they differ.
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
    },
    /// Summarise the trial logs: outcomes, reasons, time, space, verification samples.
    Report {
        /// Only the last N days.
        #[arg(long)]
        days: Option<f64>,
    },
    /// Print the store location and size in entries.
    Store {
        #[arg(long)]
        store: Option<PathBuf>,
    },
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn default_store(s: Option<PathBuf>) -> PathBuf {
    s.or_else(|| std::env::var_os("THINCLONE_STORE").map(PathBuf::from))
        .unwrap_or_else(|| home().join(".cache/thinclone/store"))
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
}

/// How an install ended when it did not succeed.
pub enum InstallError {
    /// Refused before anything in the project changed: npm can take over as is.
    Declined(anyhow::Error),
    /// A lifecycle script failed; `npm ci` would have failed the same way.
    Scripts(anyhow::Error),
    /// thinclone itself failed after node_modules was changed.
    Broke(anyhow::Error),
}

impl InstallError {
    pub fn into_inner(self) -> anyhow::Error {
        match self {
            InstallError::Declined(e) | InstallError::Scripts(e) | InstallError::Broke(e) => e,
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
    for m in ["node_modules/@npmcli/run-script", "node_modules/semver", "node_modules/@npmcli/arborist", "lib"] {
        if !npm_dir.join(m).is_dir() {
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
    info!("{} lockfile entries skipped on this platform (THINCLONE_VERBOSE=1 lists them)", plan.skipped.len());
    if std::env::var_os("THINCLONE_VERBOSE").is_some() {
        for (p, why) in &plan.skipped {
            eprintln!("[thinclone] skip {p}: {why}");
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
    timed("remove old node_modules", || {
        for r in &plan.owned_roots {
            let nm = root.join(r).join("node_modules");
            let Ok(entries) = std::fs::read_dir(&nm) else { continue };
            for ent in entries {
                let ent = ent?;
                let p = ent.path();
                let r = if ent.file_type()?.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                r.with_context(|| format!("removing {}", p.display()))?;
            }
        }
        Ok(())
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
                        let src = store.dir_for(&Integrity::parse(integrity)?);
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

const INIT_SHELLS: [&str; 3] = ["zsh", "bash", "fish"];

/// The shell `thinclone init` was started from: its parent process. In
/// `eval "$(thinclone init)"` that is the zsh or bash reading the rc file, in
/// `thinclone init | source` the fish. Not $SHELL, which names the login shell
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
    let rebuild = npm::NpmRebuild { lock_raw: lf.raw.clone(), ignore_scripts: o.ignore_scripts, extra_args: vec![] };
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
            Ok(Summary { placed: plan.entries.len(), secs, store_new: extracted_now.len(), bytes_logical, bytes_store_new })
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
        Cmd::Install { dir, store, npm_cache, clone_mode, mtime, ignore_scripts, no_rebuild } => {
            let o = InstallOpts {
                dir,
                store: default_store(store),
                cacache: npm_cache.unwrap_or_else(|| home().join(".npm/_cacache")),
                clone_mode: clone_mode.unwrap_or_else(default_clone_mode),
                mtime,
                ignore_scripts,
                no_rebuild,
            };
            run_install(&o).map_err(InstallError::into_inner)?;
        }
        Cmd::Npm { args } => {
            let code = wrapper::run(args);
            std::process::exit(code);
        }
        Cmd::Init { shell } => {
            let shell = match shell {
                Some(s) => s,
                None => parent_shell().map(String::from).with_context(|| {
                    format!(
                        "could not tell which shell runs this; name it: thinclone init <{}>\n  \
                         zsh (~/.zshrc) / bash (~/.bashrc): command -v thinclone >/dev/null && eval \"$(thinclone init zsh)\"\n  \
                         fish (~/.config/fish/config.fish): type -q thinclone; and thinclone init fish | source",
                        INIT_SHELLS.join("|")
                    )
                })?,
            };
            // The files in shell/ are the one definition; the binary carries
            // them so the rc line needs no checkout of this repository.
            print!("{}", if shell == "fish" { include_str!("../shell/thinclone.fish") } else { include_str!("../shell/thinclone.sh") });
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
        Cmd::Diff { a, b, ignore, limit, paths } => {
            let r = diff::diff(&a, &b, &ignore)?;
            if paths {
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
        Cmd::Store { store } => {
            let s = default_store(store);
            let n = walkdir::WalkDir::new(s.join(store::LAYOUT)).min_depth(3).max_depth(3).into_iter().count();
            println!("{} ({n} packages)", s.display());
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
