// Embeds the git commit this binary was built from (MATCHAPACK_BUILD), so the
// trial logs can tell binaries apart: `<short sha>`, `<short sha>-dirty` when
// tracked files had uncommitted changes, or `unknown` outside a git checkout
// (for example a build from a crates.io package).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether this crate is the top of its own git checkout. A build from a
/// crates.io package (`cargo install matchapack`, or `cargo package`'s verify
/// step under target/package/) may sit inside some other repository, a home
/// directory kept in git for example; that repository's HEAD says nothing
/// about this binary.
fn own_checkout() -> bool {
    let canon = |p: &str| std::fs::canonicalize(p).ok();
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok();
    match (git(&["rev-parse", "--show-toplevel"]), manifest) {
        (Some(top), Some(dir)) => canon(&top).is_some() && canon(&top) == canon(&dir),
        _ => false,
    }
}

fn main() {
    let sha = if own_checkout() { git(&["rev-parse", "--short=10", "HEAD"]) } else { None };
    let build = match sha {
        Some(sha) if !sha.is_empty() => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            if dirty { format!("{sha}-dirty") } else { sha }
        }
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=MATCHAPACK_BUILD={build}");
    // Recompute when HEAD moves or the index changes (a commit, a checkout,
    // staging). Ask git where those files are: in a worktree `.git` is a file,
    // HEAD and index live in the worktree's own git dir, refs in the common
    // one. Source edits rebuild anyway, which refreshes the dirty flag.
    let git_dir = git(&["rev-parse", "--git-dir"]);
    let common_dir = git(&["rev-parse", "--git-common-dir"]);
    let mut watch = vec![];
    if let Some(d) = &git_dir {
        watch.push(format!("{d}/HEAD"));
        watch.push(format!("{d}/index"));
    }
    if let Some(d) = &common_dir {
        watch.push(format!("{d}/refs/heads"));
        watch.push(format!("{d}/packed-refs"));
    }
    for p in watch {
        if std::path::Path::new(&p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    println!("cargo:rerun-if-changed=src");
}
