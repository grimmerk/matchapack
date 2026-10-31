//! Package-manager-neutral install plan.
//!
//! A reader (today: npm's package-lock v2/v3 in `npm.rs`) turns its own inputs
//! into a `Plan`; the shared layers (store, clone placement) only see the plan.
//! Nothing here assumes install paths came from a lockfile: a future Yarn 1
//! reader would have to compute them (hoisting) or read them from an installed
//! tree, and hand over the same structure.

use std::path::Path;

#[derive(Debug, Clone)]
pub enum Source {
    /// A registry or remote tarball, identified by its URL (also the npm cache key).
    Tarball { url: String },
    /// Only name@version is known (npm lockfile entries without `resolved`):
    /// the registry's packument supplies the tarball URL and integrity.
    Registry { name: String, version: String },
}

#[derive(Debug, Clone)]
pub enum Kind {
    /// Content cloned from the store, keyed by `integrity` (filled in by
    /// `resolve` when the reader did not know it).
    Package { integrity: Option<String>, source: Source },
    /// A symlink (workspace or `file:` dependency) to `target`, relative to the project root.
    Link { target: String },
}

#[derive(Debug, Clone)]
pub struct PlanEntry {
    /// Install path relative to the project root, e.g. `node_modules/a/node_modules/b`.
    pub install_path: String,
    pub name: String,
    pub version: Option<String>,
    pub kind: Kind,
    pub has_install_script: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
    /// Directories whose `node_modules` the install owns (wiped before placing):
    /// the root and each workspace.
    pub owned_roots: Vec<String>,
    /// Entries left out on purpose (platform mismatch on an optional dependency, ...),
    /// reported for transparency.
    pub skipped: Vec<(String, String)>,
}

/// The step after files are in place: lifecycle scripts, bin links, and any
/// metadata the package manager expects (npm's hidden lockfile). Swappable per
/// package manager, since Yarn 1 has no `npm rebuild` equivalent.
pub trait PostInstall {
    fn run(&self, root: &Path, plan: &Plan) -> anyhow::Result<()>;
}
