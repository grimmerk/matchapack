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

use anyhow::Result;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
fn xattr_names(p: &Path) -> Vec<String> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return vec![] };
    #[cfg(target_os = "macos")]
    let list = |buf: *mut libc::c_char, len: usize| unsafe { libc::listxattr(c.as_ptr(), buf, len, libc::XATTR_NOFOLLOW) };
    #[cfg(not(target_os = "macos"))]
    let list = |buf: *mut libc::c_char, len: usize| unsafe { libc::llistxattr(c.as_ptr(), buf, len) };
    // The list can grow between the size query and the read: retry.
    for _ in 0..4 {
        let n = list(std::ptr::null_mut(), 0);
        if n <= 0 {
            return vec![];
        }
        let mut buf = vec![0u8; n as usize];
        let n = list(buf.as_mut_ptr() as *mut libc::c_char, buf.len());
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return vec![];
        }
        let mut names: Vec<String> =
            buf[..n as usize].split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        names.sort();
        return names;
    }
    vec!["<xattr list kept changing>".into()]
}

fn scan(root: &Path, meta: bool) -> Result<BTreeMap<String, (Node, Option<Meta>)>> {
    let mut paths = vec![];
    for ent in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let ent = ent?;
        let rel = ent.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
        paths.push((rel, ent.path().to_path_buf(), ent.file_type()));
    }
    let nodes: Vec<(String, (Node, Option<Meta>))> = paths
        .into_par_iter()
        .map(|(rel, p, ft)| {
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
                let md = std::fs::symlink_metadata(&p).ok();
                Some(md.map_or_else(Meta::default, |md| Meta {
                    uid: md.uid(),
                    gid: md.gid(),
                    mtime_ns: mtime_ns(&md),
                    xattrs: xattr_names(&p),
                }))
            } else {
                None
            };
            (rel, (n, m))
        })
        .collect();
    Ok(nodes.into_iter().collect())
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

pub fn diff_with(a: &Path, b: &Path, ignore: &[String], meta: Option<&MetaOpts>) -> Result<Report> {
    let want = meta.is_some();
    let (ma, mb) = rayon::join(|| scan(a, want), || scan(b, want));
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
        if k == HIDDEN {
            let ja: serde_json::Value = serde_json::from_slice(&std::fs::read(a.join(k))?)?;
            let jb: serde_json::Value = serde_json::from_slice(&std::fs::read(b.join(k))?)?;
            r.hidden_bytes_equal = Some(va == vb);
            if ja != jb {
                r.differ.push((k.clone(), "hidden lockfile JSON differs".into()));
                m.content.push(k.clone());
            }
        } else if va != vb {
            r.differ.push((k.clone(), format!("a={va:?} b={vb:?}")));
            classify(k, va, vb, &mut m);
        }
        if let (Some(xa), Some(xb)) = (xa, xb) {
            let (sa, sb) = (when(xa.mtime_ns, opts.a_run, &fa), when(xb.mtime_ns, opts.b_run, &fb));
            compare_meta(k, xa, xb, sa, sb, &mut m);
            if Path::new(k).file_name().is_some_and(|n| n == YARN_INTEGRITY) {
                m.yarn_integrity.push((k.clone(), va == vb));
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
        diff_with(a, b, ignore, None)
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
        diff_with(a, b, &[], Some(o)).unwrap()
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
    fn run_window_parses() {
        assert_eq!(Run::parse("1:2").unwrap(), Run { start: 1, end: 2 });
        assert!(Run::parse("2:1").is_err());
        assert!(Run::parse("12").is_err());
    }
}
