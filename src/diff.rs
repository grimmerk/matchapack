//! Verification harness: compare two project trees (an `npm ci` result and a
//! matchapack result). "Identical" means: same set of paths, and per path the
//! same type, permission bits, file content (blake3) and symlink target.
//! `node_modules/.package-lock.json` is compared as parsed JSON (and also
//! byte-for-byte, reported separately).
//!
//! `--meta` (`Meta`) adds, per path present in both trees: the modification
//! time to the nanosecond, uid, gid and the NAMES of the extended attributes,
//! and reports every class of difference separately. File times written
//! during the install legitimately differ between two runs; `MetaOpts`
//! explains the one rule that lets them through.
//!
//! `--normalize-roots` (`RootForms`) replaces each tree's own root path,
//! where it appears inside a relative path, with `<root>` before anything is
//! compared: an install script that names a file after its own directory
//! then gives the same path in every tree.

use anyhow::Result;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, PartialEq, Eq, Clone)]
enum Node {
    Dir { mode: u32 },
    File { mode: u32, hash: String, size: u64 },
    Link { target: PathBuf },
    Other,
}

/// What `--meta` compares besides the `Node`; read with lstat, so a symlink's
/// own values.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
struct Meta {
    uid: u32,
    gid: u32,
    mtime_ns: i128,
    xattrs: Vec<String>,
}

const HIDDEN: &str = "node_modules/.package-lock.json";
const YARN_INTEGRITY: &str = ".yarn-integrity";

/// The time window of one install run, in nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: i128,
    pub end: i128,
}

impl Run {
    /// `START:END`, nanoseconds since the epoch.
    pub fn parse(s: &str) -> Result<Run> {
        let (a, b) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("expected START:END (ns since the epoch), got {s:?}"))?;
        let r = Run { start: a.trim().parse()?, end: b.trim().parse()? };
        anyhow::ensure!(r.start <= r.end, "run window {s:?} ends before it starts");
        Ok(r)
    }
    fn contains(&self, t: i128) -> bool {
        self.start <= t && t <= self.end
    }
}

/// Options of `--meta`.
///
/// The mtime rule. Two mtimes pass when they are equal to the nanosecond.
/// Otherwise they pass only when BOTH sides were given a run window and both
/// times fall inside their own side's window AND fall in the same one of
/// two classes:
///   - "cache fill": the time equals, to the nanosecond, the mtime of a
///     regular file in that side's cache directory (`a_cache` / `b_cache`)
///     which is itself inside the run window: the file kept the time of a
///     cache entry that this run filled (Yarn sets every file of a fetched
///     entry to the fetch's start time, and its link step copies that time);
///   - "run": any other time inside the window: written during the run
///     (install-script output, `.yarn-integrity`, npm's hidden lockfile,
///     directories, or a copy whose time was not fixed).
/// A time outside its window is compared exactly. So a file that kept a
/// cache time on one side and the copy time on the other is reported even
/// when the cache was filled during the run (empty-cache case), and with a
/// warm cache (filled before the run) every cache time is outside the window
/// and compared exactly. What the rule cannot see: two in-run times of the
/// same class that should have been equal (for example, a file carrying
/// ANOTHER entry's cache-fill time).
#[derive(Debug, Clone, Default)]
pub struct MetaOpts {
    pub a_run: Option<Run>,
    pub b_run: Option<Run>,
    pub a_cache: Option<PathBuf>,
    pub b_cache: Option<PathBuf>,
}

/// Where one side's mtime falls under the rule of `MetaOpts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    Exact(i128),
    Run,
    CacheFill,
}

fn when(t: i128, run: Option<Run>, fills: &HashSet<i128>) -> When {
    match run {
        Some(r) if r.contains(t) => {
            if fills.contains(&t) {
                When::CacheFill
            } else {
                When::Run
            }
        }
        _ => When::Exact(t),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MtimeVerdict {
    Equal,
    BothRun,
    BothCacheFill,
    Differ,
}

fn mtime_verdict(a: i128, b: i128, sa: When, sb: When) -> MtimeVerdict {
    match (sa, sb) {
        _ if a == b => MtimeVerdict::Equal,
        (When::Run, When::Run) => MtimeVerdict::BothRun,
        (When::CacheFill, When::CacheFill) => MtimeVerdict::BothCacheFill,
        _ => MtimeVerdict::Differ,
    }
}

/// The mtimes of the regular files under `cache` that fall inside `run`:
/// the times this run's own cache fill wrote.
fn cache_fill_times(cache: Option<&Path>, run: Option<Run>) -> Result<HashSet<i128>> {
    let (Some(cache), Some(run)) = (cache, run) else { return Ok(HashSet::new()) };
    let mut out = HashSet::new();
    for ent in walkdir::WalkDir::new(cache).follow_links(false) {
        let ent = ent?;
        if ent.file_type().is_file() {
            let t = mtime_ns(&ent.path().symlink_metadata()?);
            if run.contains(t) {
                out.insert(t);
            }
        }
    }
    Ok(out)
}

fn mtime_ns(md: &std::fs::Metadata) -> i128 {
    md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128
}

/// The names of a path's extended attributes (of the symlink itself, not its
/// target), sorted. Values are not compared: on macOS every file carries
/// `com.apple.provenance`, whose value records which app created the file, so
/// a clone keeps the value of its source while a copy gets a new one, and
/// nothing in user space can set it.
pub(crate) fn xattr_names(p: &Path) -> std::io::Result<Vec<String>> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())?;
    let list = |buf: Option<&mut [u8]>| {
        let (ptr, len) = match buf {
            Some(b) => (b.as_mut_ptr() as *mut libc::c_char, b.len()),
            None => (std::ptr::null_mut(), 0),
        };
        #[cfg(target_os = "macos")]
        let n = unsafe { libc::listxattr(c.as_ptr(), ptr, len, libc::XATTR_NOFOLLOW) };
        #[cfg(not(target_os = "macos"))]
        let n = unsafe { libc::llistxattr(c.as_ptr(), ptr, len) };
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
    };
    names_from(list).map_err(|e| std::io::Error::new(e.kind(), format!("listing the extended attributes of {}: {e}", p.display())))
}

/// The xattr-name list protocol over `list` (None: ask for the size; Some:
/// read into the buffer): a filesystem without extended attributes
/// (ENOTSUP) has none; ERANGE (the list grew between the two calls) is
/// retried; every other error is returned.
fn names_from(mut list: impl FnMut(Option<&mut [u8]>) -> std::io::Result<usize>) -> std::io::Result<Vec<String>> {
    let unsupported = |e: &std::io::Error| e.raw_os_error() == Some(libc::ENOTSUP);
    for _ in 0..4 {
        let n = match list(None) {
            Ok(0) => return Ok(vec![]),
            Ok(n) => n,
            Err(e) if unsupported(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut buf = vec![0u8; n];
        let n = match list(Some(&mut buf)) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(libc::ERANGE) => continue,
            Err(e) if unsupported(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut names: Vec<String> =
            buf[..n].split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        names.sort();
        return Ok(names);
    }
    Err(std::io::Error::other("the extended attribute list kept changing"))
}

/// What `--normalize-roots` writes in place of a tree's own root.
pub const ROOT: &str = "<root>";

/// `--normalize-roots`: the forms of one tree's root that are replaced by
/// `ROOT` inside its relative paths, each a sequence of whole path
/// components, longest first.
///
/// Why: an install script can name a file after its own working directory.
/// node-gyp's make build writes `build/Release/.deps/<absolute path of the
/// package>/…/<target>.node.d`, so `pprof` in tree `plain1` gets
/// `…/.deps/<…>/plain1/node_modules/pprof/…` and in tree `engine`
/// `…/.deps/<…>/engine/node_modules/pprof/…`: two plain runs already differ
/// there, and an exact-path match of "what plain also varies on" can never
/// pair them with the engine's. With the root replaced, all three
/// trees have `…/.deps/<root>/node_modules/pprof/…`.
///
/// The forms: the root as given, made absolute, and its realpath when that
/// differs (the script sees the physical directory: on macOS `/tmp` is
/// `/private/tmp` and `$TMPDIR` lives under `/private/var`).
///
/// Over-matching: only the WHOLE root matches, as consecutive whole
/// components, never a part of a component and never the root's last
/// component alone. A tree called `A` with a package directory `A` is
/// therefore not touched; nor is a path naming only the root's parent
/// (directories such as `.deps/Users/me/work`, which are the same in every
/// tree that shares that parent, as the harnesses' trees do). A form of fewer
/// than two components (a tree at `/A`) is not used: its one name could be
/// any directory's. The measured case writes the full absolute path, so the
/// basename alone is not needed. If two paths of one tree become the same
/// path, `scan` fails instead of keeping one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootForms(Vec<Vec<String>>);

impl RootForms {
    fn of(root: &Path) -> Result<RootForms> {
        // None when the path has a `..` (absolute() keeps it; the realpath
        // form covers that root).
        let comps = |p: &Path| -> Option<Vec<String>> {
            let mut v = vec![];
            for c in p.components() {
                match c {
                    Component::Normal(s) => v.push(s.to_string_lossy().into_owned()),
                    Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
                    Component::ParentDir => return None,
                }
            }
            Some(v)
        };
        let abs = std::path::absolute(root).map_err(|e| anyhow::anyhow!("{}: {e}", root.display()))?;
        let real = std::fs::canonicalize(root).map_err(|e| anyhow::anyhow!("{}: {e}", root.display()))?;
        let mut forms: Vec<Vec<String>> = [comps(&abs), comps(&real)].into_iter().flatten().filter(|f| f.len() >= 2).collect();
        forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        forms.dedup();
        Ok(RootForms(forms))
    }

    /// `rel` (components separated by `/`) with every occurrence of a form
    /// replaced by `ROOT`.
    fn apply(&self, rel: &str) -> String {
        let parts: Vec<&str> = rel.split('/').collect();
        let mut out: Vec<&str> = Vec::with_capacity(parts.len());
        let mut i = 0;
        'next: while i < parts.len() {
            for f in &self.0 {
                if parts.len() - i >= f.len() && parts[i..i + f.len()].iter().zip(f).all(|(p, q)| *p == q) {
                    out.push(ROOT);
                    i += f.len();
                    continue 'next;
                }
            }
            out.push(parts[i]);
            i += 1;
        }
        out.join("/")
    }
}

fn scan(root: &Path, meta: bool, normalize_roots: bool) -> Result<BTreeMap<String, (Node, Option<Meta>)>> {
    let forms = if normalize_roots { Some(RootForms::of(root)?) } else { None };
    let mut paths = vec![];
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for ent in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let ent = ent?;
        let raw = ent.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
        let rel = match &forms {
            Some(f) => {
                let n = f.apply(&raw);
                if let Some(other) = seen.insert(n.clone(), raw.clone()) {
                    anyhow::bail!("{}: {other} and {raw} are the same path once the tree's root is written {ROOT}", root.display());
                }
                n
            }
            None => raw,
        };
        paths.push((rel, ent.path().to_path_buf(), ent.file_type()));
    }
    let nodes: Result<Vec<(String, (Node, Option<Meta>))>> = paths
        .into_par_iter()
        .map(|(rel, p, ft)| -> Result<(String, (Node, Option<Meta>))> {
            let n = if ft.is_symlink() {
                Node::Link { target: std::fs::read_link(&p).unwrap_or_default() }
            } else if ft.is_dir() {
                Node::Dir { mode: std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0) }
            } else if ft.is_file() {
                let md = std::fs::metadata(&p).unwrap();
                let data = std::fs::read(&p).unwrap_or_default();
                Node::File {
                    mode: md.permissions().mode() & 0o7777,
                    hash: blake3::hash(&data).to_hex().to_string(),
                    size: md.len(),
                }
            } else {
                Node::Other
            };
            let m = if meta {
                let md = std::fs::symlink_metadata(&p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
                Some(Meta { uid: md.uid(), gid: md.gid(), mtime_ns: mtime_ns(&md), xattrs: xattr_names(&p)? })
            } else {
                None
            };
            Ok((rel, (n, m)))
        })
        .collect();
    Ok(nodes?.into_iter().collect())
}

/// What `--meta` found, one list per class: (path, detail).
#[derive(Default)]
pub struct MetaReport {
    /// Paths present in both trees whose metadata was compared.
    pub compared: usize,
    /// Mtimes equal to the nanosecond.
    pub mtime_equal: usize,
    /// Classified, not differences (see `MetaOpts`).
    pub mtime_both_run: Vec<String>,
    pub mtime_both_cache_fill: Vec<String>,
    /// Differences.
    pub mtime: Vec<(String, String)>,
    pub uid: Vec<(String, String)>,
    pub gid: Vec<(String, String)>,
    pub xattr_names: Vec<(String, String)>,
    /// The base comparison's differences, by class (also in `Report::differ`).
    pub type_: Vec<String>,
    pub content: Vec<String>,
    pub file_mode: Vec<String>,
    pub dir_mode: Vec<String>,
    pub symlink_target: Vec<String>,
    /// Every `.yarn-integrity` present in either tree: byte-identical?
    pub yarn_integrity: Vec<(String, bool)>,
}

impl MetaReport {
    fn ok(&self) -> bool {
        self.mtime.is_empty() && self.uid.is_empty() && self.gid.is_empty() && self.xattr_names.is_empty()
    }
}

pub struct Report {
    pub only_a: Vec<String>,
    pub only_b: Vec<String>,
    pub differ: Vec<(String, String)>,
    pub hidden_bytes_equal: Option<bool>,
    pub files: usize,
    pub meta: Option<MetaReport>,
}

/// Compare tree `a` with tree `b`. `ignore` holds relative paths (with
/// `normalize_roots`, as reported: with `<root>`); `normalize_roots` is
/// `--normalize-roots` (`RootForms`).
pub fn diff_with(a: &Path, b: &Path, ignore: &[String], meta: Option<&MetaOpts>, normalize_roots: bool) -> Result<Report> {
    let want = meta.is_some();
    let (ma, mb) = rayon::join(|| scan(a, want, normalize_roots), || scan(b, want, normalize_roots));
    let (ma, mb) = (ma?, mb?);
    let ignored = |p: &str| ignore.iter().any(|i| p == i || p.starts_with(&format!("{i}/")));
    let mut r = Report { only_a: vec![], only_b: vec![], differ: vec![], hidden_bytes_equal: None, files: ma.len(), meta: None };
    let mut m = MetaReport::default();
    let opts = meta.cloned().unwrap_or_default();
    let (fa, fb) = if want {
        (cache_fill_times(opts.a_cache.as_deref(), opts.a_run)?, cache_fill_times(opts.b_cache.as_deref(), opts.b_run)?)
    } else {
        Default::default()
    };
    for (k, (va, xa)) in &ma {
        if ignored(k) {
            continue;
        }
        let Some((vb, xb)) = mb.get(k) else {
            r.only_a.push(k.clone());
            continue;
        };
        // (With --normalize-roots `k` is the reported path; one with a root
        // replaced has a `<root>` component, so a `k` equal to HIDDEN is
        // also the path on disk.)
        if k == HIDDEN && matches!((va, vb), (Node::File { .. }, Node::File { .. })) {
            let ja: serde_json::Value = serde_json::from_slice(&std::fs::read(a.join(k))?)?;
            let jb: serde_json::Value = serde_json::from_slice(&std::fs::read(b.join(k))?)?;
            r.hidden_bytes_equal = Some(same_content(va, vb));
            let mut why = vec![];
            if ja != jb {
                why.push("hidden lockfile JSON differs".to_string());
                m.content.push(k.clone());
            }
            if let (Node::File { mode: ma, .. }, Node::File { mode: mb, .. }) = (va, vb)
                && ma != mb
            {
                why.push(format!("hidden lockfile mode a={ma:o} b={mb:o}"));
                m.file_mode.push(k.clone());
            }
            if !why.is_empty() {
                r.differ.push((k.clone(), why.join("; ")));
            }
        } else if va != vb {
            r.differ.push((k.clone(), format!("a={va:?} b={vb:?}")));
            classify(k, va, vb, &mut m);
        }
        if let (Some(xa), Some(xb)) = (xa, xb) {
            let (sa, sb) = (when(xa.mtime_ns, opts.a_run, &fa), when(xb.mtime_ns, opts.b_run, &fb));
            compare_meta(k, xa, xb, sa, sb, &mut m);
            if Path::new(k).file_name().is_some_and(|n| n == YARN_INTEGRITY) {
                m.yarn_integrity.push((k.clone(), same_content(va, vb)));
            }
        }
    }
    for k in mb.keys() {
        if !ignored(k) && !ma.contains_key(k) {
            r.only_b.push(k.clone());
        }
    }
    if want {
        for k in r.only_a.iter().chain(&r.only_b) {
            if Path::new(k).file_name().is_some_and(|n| n == YARN_INTEGRITY) {
                m.yarn_integrity.push((k.clone(), false));
            }
        }
        r.meta = Some(m);
    }
    Ok(r)
}

/// Same bytes: both regular files with the same hash and size (the mode is
/// not part of it).
fn same_content(va: &Node, vb: &Node) -> bool {
    matches!((va, vb), (Node::File { hash: ha, size: sa, .. }, Node::File { hash: hb, size: sb, .. }) if ha == hb && sa == sb)
}

/// The `--meta` comparison of one path present in both trees.
fn compare_meta(k: &str, xa: &Meta, xb: &Meta, sa: When, sb: When, m: &mut MetaReport) {
    m.compared += 1;
    match mtime_verdict(xa.mtime_ns, xb.mtime_ns, sa, sb) {
        MtimeVerdict::Equal => m.mtime_equal += 1,
        MtimeVerdict::BothRun => m.mtime_both_run.push(k.into()),
        MtimeVerdict::BothCacheFill => m.mtime_both_cache_fill.push(k.into()),
        MtimeVerdict::Differ => m.mtime.push((k.into(), format!("a={} ({}) b={} ({})", xa.mtime_ns, label(sa), xb.mtime_ns, label(sb)))),
    }
    if xa.uid != xb.uid {
        m.uid.push((k.into(), format!("a={} b={}", xa.uid, xb.uid)));
    }
    if xa.gid != xb.gid {
        m.gid.push((k.into(), format!("a={} b={}", xa.gid, xb.gid)));
    }
    if xa.xattrs != xb.xattrs {
        m.xattr_names.push((k.into(), format!("a={:?} b={:?}", xa.xattrs, xb.xattrs)));
    }
}

fn label(w: When) -> &'static str {
    match w {
        When::Exact(_) => "outside its run",
        When::Run => "written during its run",
        When::CacheFill => "its run's cache fill",
    }
}

/// Sort one differing pair of the base comparison into its classes.
fn classify(k: &str, va: &Node, vb: &Node, m: &mut MetaReport) {
    match (va, vb) {
        (Node::File { mode: ma, hash: ha, size: sa }, Node::File { mode: mb, hash: hb, size: sb }) => {
            if ma != mb {
                m.file_mode.push(k.into());
            }
            if ha != hb || sa != sb {
                m.content.push(k.into());
            }
        }
        (Node::Dir { .. }, Node::Dir { .. }) => m.dir_mode.push(k.into()),
        (Node::Link { .. }, Node::Link { .. }) => m.symlink_target.push(k.into()),
        _ => m.type_.push(k.into()),
    }
}

impl Report {
    pub fn ok(&self) -> bool {
        self.only_a.is_empty() && self.only_b.is_empty() && self.differ.is_empty() && self.meta.as_ref().is_none_or(|m| m.ok())
    }

    pub fn print_paths(&self) {
        let mut seen = HashSet::new();
        let base = self.only_a.iter().chain(&self.only_b).chain(self.differ.iter().map(|(p, _)| p));
        let meta = self.meta.iter().flat_map(|m| m.mtime.iter().chain(&m.uid).chain(&m.gid).chain(&m.xattr_names).map(|(p, _)| p));
        for p in base.chain(meta) {
            if seen.insert(p) {
                println!("{p}");
            }
        }
    }

    /// Every difference as one `<class>\t<path>` line: `only-a`, `only-b`,
    /// and with --meta the classes of the report (type, content, file-mode,
    /// dir-mode, symlink-target, mtime, uid, gid, xattr-names); without it,
    /// `differ`. A path with several kinds of difference has several lines.
    pub fn print_classes(&self) {
        for p in &self.only_a {
            println!("only-a\t{p}");
        }
        for p in &self.only_b {
            println!("only-b\t{p}");
        }
        match &self.meta {
            None => {
                for (p, _) in &self.differ {
                    println!("differ\t{p}");
                }
            }
            Some(m) => {
                for (class, v) in [("type", &m.type_), ("content", &m.content), ("file-mode", &m.file_mode), ("dir-mode", &m.dir_mode), ("symlink-target", &m.symlink_target)] {
                    for p in v {
                        println!("{class}\t{p}");
                    }
                }
                for (class, v) in [("mtime", &m.mtime), ("uid", &m.uid), ("gid", &m.gid), ("xattr-names", &m.xattr_names)] {
                    for (p, _) in v {
                        println!("{class}\t{p}");
                    }
                }
            }
        }
    }

    pub fn print(&self, limit: usize) {
        println!("scanned {} paths in A", self.files);
        let show = |title: &str, v: &Vec<String>| {
            println!("{title}: {}", v.len());
            for x in v.iter().take(limit) {
                println!("  {x}");
            }
        };
        show("only in A", &self.only_a);
        show("only in B", &self.only_b);
        println!("differ: {}", self.differ.len());
        for (k, why) in self.differ.iter().take(limit) {
            println!("  {k}: {why}");
        }
        if let Some(eq) = self.hidden_bytes_equal {
            println!("hidden lockfile byte-identical: {eq}");
        }
        if let Some(m) = &self.meta {
            m.print(limit);
        }
        println!("RESULT: {}", if self.ok() { "IDENTICAL" } else { "DIFFERENT" });
    }
}

impl MetaReport {
    fn print(&self, limit: usize) {
        println!("meta: {} paths in both trees (mtime to the nanosecond, uid, gid, xattr names)", self.compared);
        println!(
            "meta: mtime equal {}; not differences: written during both runs {}, from each run's own cache fill {}",
            self.mtime_equal,
            self.mtime_both_run.len(),
            self.mtime_both_cache_fill.len()
        );
        for (title, v) in [("written during both runs", &self.mtime_both_run), ("from each run's own cache fill", &self.mtime_both_cache_fill)] {
            for x in v.iter().take(limit) {
                println!("  {title}: {x}");
            }
        }
        println!("meta differences by class:");
        let names = |title: &str, v: &Vec<String>| {
            println!("  {title}: {}", v.len());
            for x in v.iter().take(limit) {
                println!("    {x}");
            }
        };
        names("type", &self.type_);
        names("content", &self.content);
        names("file-mode", &self.file_mode);
        names("dir-mode", &self.dir_mode);
        names("symlink-target", &self.symlink_target);
        for (title, v) in [("mtime", &self.mtime), ("uid", &self.uid), ("gid", &self.gid), ("xattr-names", &self.xattr_names)] {
            println!("  {title}: {}", v.len());
            for (k, why) in v.iter().take(limit) {
                println!("    {k}: {why}");
            }
        }
        for (k, eq) in &self.yarn_integrity {
            println!("yarn integrity byte-identical ({k}): {eq}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::scratch;
    use std::fs;

    /// Set a path's mtime (and atime) to `ns` since the epoch, without following a symlink.
    fn set_mtime(p: &Path, ns: i128) {
        use std::os::unix::ffi::OsStrExt;
        let t = libc::timespec { tv_sec: (ns / 1_000_000_000) as _, tv_nsec: (ns % 1_000_000_000) as _ };
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }, 0);
    }

    fn set_xattr(p: &Path, name: &str) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        let n = std::ffi::CString::new(name).unwrap();
        #[cfg(target_os = "macos")]
        let r = unsafe { libc::setxattr(c.as_ptr(), n.as_ptr(), b"v".as_ptr() as *const _, 1, 0, 0) };
        #[cfg(not(target_os = "macos"))]
        let r = unsafe { libc::setxattr(c.as_ptr(), n.as_ptr(), b"v".as_ptr() as *const _, 1, 0) };
        r == 0
    }

    const OLD: i128 = 1_700_000_000_123_456_789;

    /// The comparison without --meta.
    fn diff(a: &Path, b: &Path, ignore: &[String]) -> Result<Report> {
        diff_with(a, b, ignore, None, false)
    }

    /// Two identical trees, A and B, with every time set to OLD: a package
    /// file, a directory, an install output and a `.yarn-integrity`.
    fn pair(name: &str) -> (PathBuf, PathBuf) {
        let d = scratch(name);
        for t in ["A", "B"] {
            let r = d.join(t);
            fs::create_dir_all(r.join("node_modules/p/lib")).unwrap();
            fs::write(r.join("node_modules/p/index.js"), "module.exports = 1\n").unwrap();
            fs::write(r.join("node_modules/p/lib/x.js"), "x\n").unwrap();
            fs::write(r.join("node_modules/.yarn-integrity"), "{}\n").unwrap();
            fs::write(r.join("package.json"), "{}\n").unwrap();
            for p in ["node_modules/p/index.js", "node_modules/p/lib/x.js", "node_modules/.yarn-integrity", "package.json", "node_modules/p/lib", "node_modules/p", "node_modules"] {
                set_mtime(&r.join(p), OLD);
            }
        }
        (d.join("A"), d.join("B"))
    }

    fn meta(a: &Path, b: &Path, o: &MetaOpts) -> Report {
        diff_with(a, b, &[], Some(o), false).unwrap()
    }

    #[test]
    fn identical_trees_pass_with_meta() {
        let (a, b) = pair("meta-same");
        let r = meta(&a, &b, &MetaOpts::default());
        let m = r.meta.as_ref().unwrap();
        assert!(r.ok(), "{:?} {:?}", m.mtime, m.xattr_names);
        assert_eq!(m.compared, 7);
        assert_eq!(m.mtime_equal, 7);
        assert_eq!(m.yarn_integrity, vec![("node_modules/.yarn-integrity".to_string(), true)]);
        // Without --meta nothing of this is computed.
        assert!(diff(&a, &b, &[]).unwrap().meta.is_none());
    }

    #[test]
    fn a_sub_millisecond_mtime_difference_is_reported() {
        let (a, b) = pair("meta-mtime");
        set_mtime(&b.join("node_modules/p/index.js"), OLD + 1_000); // 1 µs
        let r = meta(&a, &b, &MetaOpts::default());
        assert!(!r.ok());
        assert!(r.differ.is_empty(), "the base comparison does not see it");
        assert!(diff(&a, &b, &[]).unwrap().ok(), "and without --meta the trees pass");
        let m = r.meta.unwrap();
        assert_eq!(m.mtime.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["node_modules/p/index.js"]);
    }

    #[test]
    fn a_directory_mode_difference_is_reported_in_its_class() {
        let (a, b) = pair("meta-dirmode");
        fs::set_permissions(b.join("node_modules/p/lib"), fs::Permissions::from_mode(0o775)).unwrap();
        set_mtime(&b.join("node_modules/p/lib"), OLD);
        let r = meta(&a, &b, &MetaOpts::default());
        assert!(!r.ok());
        let m = r.meta.unwrap();
        assert_eq!(m.dir_mode, ["node_modules/p/lib"]);
        assert!(m.file_mode.is_empty() && m.content.is_empty() && m.mtime.is_empty());
    }

    #[test]
    fn an_extra_xattr_name_is_reported() {
        let (a, b) = pair("meta-xattr");
        if !set_xattr(&b.join("node_modules/p/index.js"), "user.matchapack.test") {
            eprintln!("skipped: this filesystem takes no extended attributes");
            return;
        }
        let r = meta(&a, &b, &MetaOpts::default());
        assert!(!r.ok());
        assert!(diff(&a, &b, &[]).unwrap().ok(), "without --meta the trees pass");
        let m = r.meta.unwrap();
        assert_eq!(m.xattr_names.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["node_modules/p/index.js"]);
    }

    #[test]
    fn a_yarn_integrity_byte_difference_is_reported() {
        let (a, b) = pair("meta-integrity");
        fs::write(b.join("node_modules/.yarn-integrity"), "{} \n").unwrap();
        set_mtime(&b.join("node_modules/.yarn-integrity"), OLD);
        let r = meta(&a, &b, &MetaOpts::default());
        assert!(!r.ok());
        let m = r.meta.as_ref().unwrap();
        assert_eq!(m.yarn_integrity, vec![("node_modules/.yarn-integrity".to_string(), false)]);
        assert_eq!(m.content, ["node_modules/.yarn-integrity"]);
        // The base comparison sees it too (it is not skipped or parsed).
        assert!(!diff(&a, &b, &[]).unwrap().ok());
    }

    /// A uid difference cannot be made without root, so the per-path
    /// comparison is checked on values; the gid also end to end.
    #[test]
    fn uid_and_gid_differences_are_reported() {
        let x = Meta { uid: 501, gid: 20, mtime_ns: OLD, xattrs: vec![] };
        let e = When::Exact(OLD);
        let mut m = MetaReport::default();
        compare_meta("p", &x, &Meta { uid: 502, ..x.clone() }, e, e, &mut m);
        compare_meta("q", &x, &Meta { gid: 0, ..x.clone() }, e, e, &mut m);
        compare_meta("r", &x, &x.clone(), e, e, &mut m);
        assert_eq!(m.uid.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["p"]);
        assert_eq!(m.gid.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["q"]);
        assert!(!m.ok());
        let (a, b) = pair("meta-owner");
        let k = "node_modules/p/index.js";
        // End to end, where the user is in a second group.
        let groups = unsafe {
            let mut g = vec![0 as libc::gid_t; 64];
            let n = libc::getgroups(g.len() as i32, g.as_mut_ptr());
            g.truncate(n.max(0) as usize);
            g
        };
        let mine = fs::metadata(b.join("node_modules/p/index.js")).unwrap().gid();
        let Some(other) = groups.iter().copied().find(|g| *g != mine) else {
            eprintln!("skipped the end-to-end gid case: the user is in one group only");
            return;
        };
        std::os::unix::fs::lchown(b.join(k), None, Some(other)).unwrap();
        let r = meta(&a, &b, &MetaOpts::default());
        assert!(!r.ok());
        assert!(diff(&a, &b, &[]).unwrap().ok(), "without --meta the trees pass");
        let m = r.meta.unwrap();
        assert_eq!(m.gid.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), [k]);
        assert!(m.uid.is_empty());
    }

    #[test]
    fn mtime_rule_classifies_only_same_class_in_run_times() {
        let w = Some(Run { start: 100, end: 200 });
        let fills: HashSet<i128> = [150].into();
        let none = HashSet::new();
        // Outside the window: exact.
        assert_eq!(when(50, w, &fills), When::Exact(50));
        assert_eq!(when(250, w, &fills), When::Exact(250));
        assert_eq!(when(150, None, &fills), When::Exact(150), "no window: exact");
        assert_eq!(when(150, w, &fills), When::CacheFill);
        assert_eq!(when(160, w, &fills), When::Run);
        let v = |a, b, fa: &HashSet<i128>, fb: &HashSet<i128>| mtime_verdict(a, b, when(a, w, fa), when(b, w, fb));
        assert_eq!(v(50, 50, &none, &none), MtimeVerdict::Equal);
        assert_eq!(v(50, 51, &none, &none), MtimeVerdict::Differ, "both before the run: exact");
        assert_eq!(v(120, 130, &none, &none), MtimeVerdict::BothRun);
        assert_eq!(v(150, 160, &fills, &[160].into()), MtimeVerdict::BothCacheFill);
        // The measured Yarn difference: one side kept the cache time, the
        // other the copy time, with a warm cache (cache time before the run) ...
        assert_eq!(v(50, 120, &none, &none), MtimeVerdict::Differ);
        // ... and with each run filling its own cache (both in the window).
        assert_eq!(v(150, 170, &fills, &[160].into()), MtimeVerdict::Differ);
        // A window on one side only: the other side is exact.
        assert_eq!(mtime_verdict(120, 130, when(120, w, &none), when(130, None, &none)), MtimeVerdict::Differ);
    }

    #[test]
    fn install_outputs_are_classified_and_cache_times_are_not_hidden() {
        let (a, b) = pair("meta-runs");
        let d = a.parent().unwrap().to_path_buf();
        let (ra, rb) = (Run { start: 1_000_000_000_000, end: 1_000_000_100_000 }, Run { start: 2_000_000_000_000, end: 2_000_000_100_000 });
        // Each run's cache, filled during the run.
        for (t, r) in [("cache-A", ra), ("cache-B", rb)] {
            fs::create_dir_all(d.join(t)).unwrap();
            fs::write(d.join(t).join("index.js"), "module.exports = 1\n").unwrap();
            set_mtime(&d.join(t).join("index.js"), r.start + 10);
        }
        // Written during each run: an install output, .yarn-integrity, the directories.
        for (root, r) in [(&a, ra), (&b, rb)] {
            fs::write(root.join("node_modules/p/built.txt"), "x").unwrap();
            for (p, dt) in [("node_modules/p/built.txt", 50), ("node_modules/.yarn-integrity", 60), ("node_modules/p/lib", 20), ("node_modules/p", 30), ("node_modules", 40)] {
                set_mtime(&root.join(p), r.start + dt);
            }
            // The package file kept its run's cache-fill time.
            set_mtime(&root.join("node_modules/p/index.js"), r.start + 10);
        }
        let o = MetaOpts { a_run: Some(ra), b_run: Some(rb), a_cache: Some(d.join("cache-A")), b_cache: Some(d.join("cache-B")) };
        let r = meta(&a, &b, &o);
        let m = r.meta.as_ref().unwrap();
        assert!(r.ok(), "{:?}", m.mtime);
        assert_eq!(m.mtime_both_cache_fill, ["node_modules/p/index.js"]);
        assert_eq!(m.mtime_both_run.len(), 5);
        // Without run windows, the same trees differ in all of those.
        let strict = meta(&a, &b, &MetaOpts::default());
        assert_eq!(strict.meta.unwrap().mtime.len(), 6);
        // B's package file has the copy time instead of the cache time:
        // reported, although both times are inside their runs.
        set_mtime(&b.join("node_modules/p/index.js"), rb.start + 70);
        let r = meta(&a, &b, &o);
        assert!(!r.ok());
        assert_eq!(r.meta.unwrap().mtime.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["node_modules/p/index.js"]);
        // Without the caches, the same case is classified "written during
        // both runs": the caches are what make the empty-cache case sound.
        let o2 = MetaOpts { a_cache: None, b_cache: None, ..o.clone() };
        assert!(meta(&a, &b, &o2).ok());
        // An old file time on one side against an in-run time on the other.
        set_mtime(&b.join("node_modules/p/index.js"), OLD);
        assert!(!meta(&a, &b, &o).ok());
    }

    #[test]
    fn yarn_integrity_bytes_ignore_the_mode() {
        let (a, b) = pair("meta-integrity-mode");
        fs::set_permissions(b.join("node_modules/.yarn-integrity"), fs::Permissions::from_mode(0o600)).unwrap();
        let r = meta(&a, &b, &MetaOpts::default());
        assert!(!r.ok());
        let m = r.meta.unwrap();
        assert_eq!(m.yarn_integrity, vec![("node_modules/.yarn-integrity".to_string(), true)], "same bytes");
        assert_eq!(m.file_mode, ["node_modules/.yarn-integrity"]);
        assert!(m.content.is_empty());
    }

    #[test]
    fn a_hidden_lockfile_mode_difference_is_reported() {
        let d = scratch("hidden-mode");
        for t in ["A", "B"] {
            fs::create_dir_all(d.join(t).join("node_modules")).unwrap();
            fs::write(d.join(t).join(HIDDEN), "{\"lockfileVersion\": 3}\n").unwrap();
        }
        let (a, b) = (d.join("A"), d.join("B"));
        assert!(diff(&a, &b, &[]).unwrap().ok());
        fs::set_permissions(b.join(HIDDEN), fs::Permissions::from_mode(0o600)).unwrap();
        let r = diff(&a, &b, &[]).unwrap();
        assert!(!r.ok(), "without --meta too");
        assert_eq!(r.differ.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), [HIDDEN]);
        assert_eq!(r.hidden_bytes_equal, Some(true), "the bytes are the same");
        let m = meta(&a, &b, &MetaOpts::default()).meta.unwrap();
        assert_eq!(m.file_mode, [HIDDEN]);
        assert!(m.content.is_empty());
        // Different JSON and mode: one entry, both reasons.
        fs::write(b.join(HIDDEN), "{\"lockfileVersion\": 2}\n").unwrap();
        let r = diff(&a, &b, &[]).unwrap();
        assert_eq!(r.differ.len(), 1);
        assert!(r.differ[0].1.contains("JSON") && r.differ[0].1.contains("mode"), "{}", r.differ[0].1);
    }

    #[test]
    fn xattr_list_errors_are_returned_not_swallowed() {
        use std::io::Error;
        let errno = |e: i32| move |_: Option<&mut [u8]>| -> std::io::Result<usize> { Err(Error::from_raw_os_error(e)) };
        assert_eq!(names_from(errno(libc::ENOTSUP)).unwrap(), Vec::<String>::new(), "no xattr support: none");
        assert_eq!(names_from(|_| Ok(0)).unwrap(), Vec::<String>::new());
        let e = names_from(errno(libc::EACCES)).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EACCES));
        // The size query succeeds, the read fails.
        let mut calls = 0;
        let e = names_from(|b| {
            calls += 1;
            match b {
                None => Ok(4),
                Some(_) => Err(Error::from_raw_os_error(libc::EIO)),
            }
        })
        .unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EIO));
        assert_eq!(calls, 2);
        // A list that keeps growing: retried, then an error.
        let e = names_from(|b| match b {
            None => Ok(4),
            Some(_) => Err(Error::from_raw_os_error(libc::ERANGE)),
        })
        .unwrap_err();
        assert!(e.to_string().contains("kept changing"), "{e}");
        // A read that works.
        let names = names_from(|b| match b {
            None => Ok(6),
            Some(buf) => {
                buf.copy_from_slice(b"b\0a\0\0\0");
                Ok(4)
            }
        })
        .unwrap();
        assert_eq!(names, ["a", "b"]);
        // End to end: a path that does not exist is an error, not "no attributes".
        let d = scratch("xattr-missing");
        assert!(xattr_names(&d.join("gone")).is_err());
    }

    /// `rel` under `root`, with the parent directories.
    fn put(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "x\n").unwrap();
    }

    /// A path without its leading `/`, as node-gyp writes it under `.deps/`.
    fn rel_of(p: &Path) -> String {
        p.to_string_lossy().trim_start_matches('/').to_string()
    }

    #[test]
    fn normalize_roots_makes_files_named_after_each_root_the_same() {
        // The trees are reached through a symlink, so the root as given and
        // its realpath differ on every platform. Each case: both trees' file
        // named after one form of their own root (a script writes what
        // getcwd gives, the realpath; the other form is also covered).
        for (case, real) in [("roots-real", true), ("roots-given", false)] {
            let d = scratch(case);
            std::os::unix::fs::symlink(&d, d.join("via")).unwrap();
            let (a, b) = (d.join("via/A"), d.join("via/B"));
            let form = |t: &Path| rel_of(&if real { fs::canonicalize(t.parent().unwrap()).unwrap().join(t.file_name().unwrap()) } else { t.to_path_buf() });
            let file = |t: &Path| format!("x/.deps/{}/node_modules/p/f.d", form(t));
            for t in [&a, &b] {
                put(t, &file(t));
            }
            assert_ne!(fs::canonicalize(&b).unwrap(), b, "the root as given is not its realpath");
            // Without the option: each tree's file only in that tree.
            let r = diff(&a, &b, &[]).unwrap();
            assert!(!r.ok());
            assert!(r.only_a.contains(&file(&a)), "{case}: {:?}", r.only_a);
            assert!(r.only_b.contains(&file(&b)), "{case}: {:?}", r.only_b);
            // With it: identical.
            let n = diff_with(&a, &b, &[], None, true).unwrap();
            assert!(n.ok(), "{case}: only A {:?} only B {:?}", n.only_a, n.only_b);
            assert!(diff_with(&a, &b, &[], Some(&MetaOpts::default()), true).unwrap().meta.unwrap().content.is_empty());
            // A content difference there is still reported, under the
            // <root> path, and --ignore takes that form.
            fs::write(b.join(file(&b)), "y\n").unwrap();
            let n = diff_with(&a, &b, &[], None, true).unwrap();
            assert_eq!(n.differ.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["x/.deps/<root>/node_modules/p/f.d"], "{case}");
            assert!(diff_with(&a, &b, &["x/.deps/<root>".into()], None, true).unwrap().ok());
        }
    }

    #[test]
    fn normalize_roots_does_not_match_the_basename_or_a_part() {
        let d = scratch("roots-over");
        let (a, b) = (d.join("A"), d.join("B"));
        // A package directory named like the tree, and the tree's parent
        // without the tree: neither is the root.
        put(&a, "node_modules/A/f");
        put(&b, "node_modules/B/f");
        let parent = rel_of(&fs::canonicalize(&d).unwrap());
        put(&a, &format!("y/{parent}/Ax/f"));
        put(&b, &format!("y/{parent}/Ax/f"));
        let r = diff_with(&a, &b, &[], None, true).unwrap();
        assert_eq!(r.only_a, ["node_modules/A", "node_modules/A/f"]);
        assert_eq!(r.only_b, ["node_modules/B", "node_modules/B/f"]);
        // A form of one component is not used.
        assert_eq!(RootForms::of(Path::new("/")).unwrap(), RootForms(vec![]));
        let f = RootForms(vec![vec!["u".into(), "w".into(), "A".into()]]);
        assert_eq!(f.apply("x/u/w/A/y/u/w/A"), "x/<root>/y/<root>", "every occurrence");
        assert_eq!(f.apply("x/u/w/Ab/y"), "x/u/w/Ab/y", "whole components only");
        assert_eq!(f.apply("u/w"), "u/w");
    }

    #[test]
    fn normalize_roots_refuses_two_paths_that_become_one() {
        let d = scratch("roots-clash");
        let a = d.join("A");
        put(&a, &format!("z/{}/f", rel_of(&fs::canonicalize(&d).unwrap().join("A"))));
        put(&a, "z/<root>/f");
        put(&d.join("B"), "z/<root>/f");
        let e = diff_with(&a, &d.join("B"), &[], None, true).err().expect("an error, not a result");
        assert!(e.to_string().contains("same path"), "{e}");
    }

    #[test]
    fn run_window_parses() {
        assert_eq!(Run::parse("1:2").unwrap(), Run { start: 1, end: 2 });
        assert!(Run::parse("2:1").is_err());
        assert!(Run::parse("12").is_err());
    }
}
