mod clone;
mod diff;
mod lock;
mod npm;
mod plan;
mod platform;
mod resolve;
mod store;

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
        /// Store root (default $LOCKCLONE_STORE or ~/.cache/lockclone/store).
        #[arg(long)]
        store: Option<PathBuf>,
        /// npm content cache (default ~/.npm/_cacache).
        #[arg(long)]
        npm_cache: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "dir")]
        clone_mode: clone::Mode,
        /// Modification times of placed files.
        #[arg(long, value_enum, default_value = "keep")]
        mtime: clone::Mtime,
        /// Pass --ignore-scripts to npm rebuild.
        #[arg(long)]
        ignore_scripts: bool,
        /// Skip the npm rebuild step entirely (no scripts, no bin links).
        #[arg(long)]
        no_rebuild: bool,
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
    s.or_else(|| std::env::var_os("LOCKCLONE_STORE").map(PathBuf::from))
        .unwrap_or_else(|| home().join(".cache/lockclone/store"))
}

fn timed<T>(label: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let t = Instant::now();
    let r = f();
    eprintln!("[lockclone] {label}: {:.2}s", t.elapsed().as_secs_f64());
    r
}

fn npm_cache_add(root: &Path, urls: &[String]) -> Result<()> {
    for chunk in urls.chunks(200) {
        let st = std::process::Command::new("npm")
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

fn install(
    root: &Path,
    store: &Store,
    mode: clone::Mode,
    mtime: clone::Mtime,
    post: Option<&dyn PostInstall>,
    plan: &plan::Plan,
) -> Result<()> {
    // 1. Make sure every package is in the store.
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
        for r in res {
            let (m, x) = r?;
            if let Some(u) = m {
                missing.push(u);
            }
            extracted += x as usize;
        }
        Ok((missing, extracted))
    })?;
    eprintln!(
        "[lockclone] {} distinct packages: {} extracted now, {} not in npm cache",
        pkgs.len(),
        extracted,
        missing.len()
    );
    if !missing.is_empty() {
        timed("npm cache add", || npm_cache_add(root, &missing))?;
        let by_url: HashMap<&String, &Integrity> = pkgs.iter().map(|(i, u)| (u, i)).collect();
        for u in &missing {
            match store.ensure(by_url[u], u)? {
                Ensure::Missing => bail!("still not in npm cache after `npm cache add`: {u}"),
                _ => {}
            }
        }
    }

    // 2. Wipe what npm ci would wipe.
    timed("remove old node_modules", || {
        for r in &plan.owned_roots {
            let nm = root.join(r).join("node_modules");
            if nm.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&nm).with_context(|| format!("removing {}", nm.display()))?;
            }
        }
        Ok(())
    })?;

    // 3. Place packages, parents before children; one depth level at a time.
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
                        clone::clone_tree(&src, &dst, mode)?;
                        if mtime == clone::Mtime::Now {
                            clone::touch_tree(&dst)?;
                        }
                        Ok(())
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

    // 4. Scripts, bins, package-manager metadata.
    if let Some(p) = post {
        timed("post-install", || p.run(root, plan))?;
    }
    Ok(())
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
            let t0 = Instant::now();
            let root = std::fs::canonicalize(&dir)?;
            let (lf, mut plan) = timed("plan", || {
                let lf = lock::Lockfile::load(&root)?;
                npm::check_in_sync(&root, &lf)?;
                let plan = npm::build_plan(&lf)?;
                Ok((lf, plan))
            })?;
            eprintln!("[lockclone] {} lockfile entries skipped on this platform (LOCKCLONE_VERBOSE=1 lists them)", plan.skipped.len());
            if std::env::var_os("LOCKCLONE_VERBOSE").is_some() {
                for (p, why) in &plan.skipped {
                    eprintln!("[lockclone] skip {p}: {why}");
                }
            }
            let cacache = npm_cache.unwrap_or_else(|| home().join(".npm/_cacache"));
            let n = timed("resolve name@version entries", || resolve::resolve(&mut plan, &root, &cacache))?;
            if n > 0 {
                eprintln!("[lockclone] resolved {n} lockfile entries without `resolved` from packuments");
            }
            let store = Store::new(default_store(store), cacache)?;
            let post = npm::NpmRebuild { lock_raw: lf.raw.clone(), ignore_scripts, extra_args: vec![] };
            install(&root, &store, clone_mode, mtime, if no_rebuild { None } else { Some(&post) }, &plan)?;
            eprintln!(
                "[lockclone] placed {} entries in {:.2}s",
                plan.entries.len(),
                t0.elapsed().as_secs_f64()
            );
        }
        Cmd::Plan { dir } => {
            let root = std::fs::canonicalize(&dir)?;
            let lf = lock::Lockfile::load(&root)?;
            npm::check_in_sync(&root, &lf)?;
            let plan = npm::build_plan(&lf)?;
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
        Cmd::Diff { a, b, ignore, limit } => {
            let r = diff::diff(&a, &b, &ignore)?;
            r.print(limit);
            if !r.ok() {
                std::process::exit(1);
            }
        }
        Cmd::Store { store } => {
            let s = default_store(store);
            let n = walkdir::WalkDir::new(s.join(store::LAYOUT)).min_depth(3).max_depth(3).into_iter().count();
            println!("{} ({n} packages)", s.display());
        }
    }
    Ok(())
}
