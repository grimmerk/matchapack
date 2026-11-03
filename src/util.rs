//! Small shared helpers: quiet-aware logging, the one way to start npm, and the
//! error type that marks a failed lifecycle script.

use std::process::Command;
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

/// Quiet mode (used when thinclone stands in for `npm ci`): per-step timings
/// and counts are hidden unless THINCLONE_VERBOSE is set.
pub fn set_quiet(q: bool) {
    QUIET.store(q && std::env::var_os("THINCLONE_VERBOSE").is_none(), Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if !$crate::util::quiet() {
            eprintln!("[thinclone] {}", format!($($arg)*));
        }
    };
}

/// Every npm that thinclone starts goes through here. `THINCLONE=0` tells any
/// npm wrapper (shell function, PATH shim) to pass straight through, so a
/// wrapper can never recurse into thinclone.
pub fn npm_cmd() -> Command {
    let mut c = Command::new("npm");
    c.env("THINCLONE", "0");
    c
}

/// A dependency or root lifecycle script failed. `npm ci` would have failed the
/// same way, so this is reported, never retried with npm.
#[derive(Debug)]
pub struct ScriptFailure(pub String);

impl std::fmt::Display for ScriptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScriptFailure {}

/// The npm installation the `npm` on PATH runs: bin/npm resolves to
/// <dir>/bin/npm-cli.js. Used for npm's own modules (@npmcli/run-script,
/// semver) and its version, so they always match the npm that runs, whatever
/// manages it (nvm, Homebrew, Volta). `npm root -g` can point elsewhere.
pub fn npm_dir() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let bin = std::env::split_paths(&path).map(|d| d.join("npm")).find(|p| is_executable(p))?;
    let cli = std::fs::canonicalize(&bin).ok()?;
    let dir = cli.parent()?.parent()?.to_path_buf();
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("package.json")).ok()?).ok()?;
    (v.get("name").and_then(|n| n.as_str()) == Some("npm")).then_some(dir)
}

pub fn npm_version() -> Option<String> {
    let dir = npm_dir()?;
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("package.json")).ok()?).ok()?;
    v.get("version").and_then(|x| x.as_str()).map(String::from)
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// A path from the lockfile is used under the project root only if it is
/// relative and made of plain names: no `..`, no root, no `.`. Path::join with
/// an absolute path would REPLACE the root, so this guards every delete, clone
/// and symlink thinclone does inside the project.
pub fn safe_relative(p: &str) -> bool {
    !p.is_empty() && std::path::Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_)))
}
