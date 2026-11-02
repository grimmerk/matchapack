//! Small shared helpers: quiet-aware logging, the one way to start npm, and the
//! error type that marks a failed lifecycle script.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

/// Quiet mode (used when lockclone stands in for `npm ci`): per-step timings
/// and counts are hidden unless LOCKCLONE_VERBOSE is set.
pub fn set_quiet(q: bool) {
    QUIET.store(q && std::env::var_os("LOCKCLONE_VERBOSE").is_none(), Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if !$crate::util::quiet() {
            eprintln!("[lockclone] {}", format!($($arg)*));
        }
    };
}

/// Every npm that lockclone starts goes through here. `LOCKCLONE=0` tells any
/// npm wrapper (shell function, PATH shim) to pass straight through, so a
/// wrapper can never recurse into lockclone.
pub fn npm_cmd() -> Command {
    let mut c = Command::new("npm");
    c.env("LOCKCLONE", "0");
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
