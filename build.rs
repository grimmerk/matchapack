// Embeds the git commit this binary was built from (THINCLONE_BUILD), so the
// trial logs can tell binaries apart: `<short sha>`, `<short sha>-dirty` when
// tracked files had uncommitted changes, or `unknown` outside a git checkout
// (for example a build from a crates.io package).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let build = match git(&["rev-parse", "--short=10", "HEAD"]) {
        Some(sha) if !sha.is_empty() => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            if dirty { format!("{sha}-dirty") } else { sha }
        }
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=THINCLONE_BUILD={build}");
    // Rebuild the value when HEAD moves or the index changes (a commit, a
    // checkout, staging); edits to tracked files also change the dirty flag
    // only once something is rebuilt, which every source edit causes anyway.
    for p in [".git/HEAD", ".git/index", ".git/refs/heads"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    println!("cargo:rerun-if-changed=src");
}
