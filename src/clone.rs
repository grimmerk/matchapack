//! Copy-on-write cloning. Never falls back to a byte copy: a filesystem without
//! reflink support is an error, because a silent full copy would defeat the tool.

use anyhow::{bail, Context, Result};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// One `clonefile(2)` per package directory (macOS only).
    Dir,
    /// Walk the package; mkdir directories, clone each file.
    File,
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const CLONE_NOFOLLOW: u32 = 0x0001;

    pub fn clone(src: &Path, dst: &Path) -> io::Result<()> {
        let s = CString::new(src.as_os_str().as_bytes())?;
        let d = CString::new(dst.as_os_str().as_bytes())?;
        let r = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) };
        if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    pub const DIR_SUPPORTED: bool = true;
}

#[cfg(target_os = "linux")]
mod sys {
    // UNTESTED: FICLONE on Btrfs / XFS (reflink=1) / ZFS 2.2+ / bcachefs.
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const FICLONE: libc::c_ulong = 0x40049409;

    pub fn clone(src: &Path, dst: &Path) -> io::Result<()> {
        let s = std::fs::File::open(src)?;
        let d = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        let r = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) };
        if r != 0 {
            let e = io::Error::last_os_error();
            let _ = std::fs::remove_file(dst);
            return Err(e);
        }
        let perm = std::fs::metadata(src)?.permissions();
        std::fs::set_permissions(dst, perm)
    }

    pub const DIR_SUPPORTED: bool = false;
}

/// A failed clone: the message `explain` builds, plus the errno, so a caller
/// can tell "this filesystem pair cannot clone at all" from one bad package.
#[derive(Debug)]
pub struct CloneError {
    msg: String,
    pub errno: Option<i32>,
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for CloneError {}

#[cfg(test)]
impl CloneError {
    pub fn for_test(errno: i32) -> CloneError {
        CloneError { msg: format!("clone failed (errno {errno})"), errno: Some(errno) }
    }
}

/// True when `e` is a clone refused because the filesystem (or the pair of
/// filesystems) has no copy-on-write clone: every later clone would fail the
/// same way.
pub fn unsupported(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<CloneError>()).any(|c| {
        matches!(c.errno, Some(n) if n == libc::EXDEV || n == libc::ENOTSUP || n == libc::EOPNOTSUPP)
    })
}

fn explain(e: std::io::Error, src: &Path, dst: &Path) -> anyhow::Error {
    let hint = match e.raw_os_error() {
        Some(libc::EXDEV) => " (store and target are on different volumes: put the store on the same filesystem)",
        // ENOTSUP and EOPNOTSUPP are one value on Linux and two on macOS: a
        // guard instead of two patterns, so neither platform warns.
        Some(n) if n == libc::ENOTSUP || n == libc::EOPNOTSUPP => {
            " (filesystem has no copy-on-write clone support; refusing to fall back to a full copy)"
        }
        _ => "",
    };
    anyhow::Error::new(CloneError { msg: format!("clone {} -> {}: {e}{hint}", src.display(), dst.display()), errno: e.raw_os_error() })
}

fn touch_now(c: &std::ffi::CStr) -> Result<()> {
    let ts = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
    ];
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if r != 0 {
        bail!("utimensat {:?}: {}", c, std::io::Error::last_os_error());
    }
    Ok(())
}

/// What the kernel gives a file or directory created in a directory, which
/// is what npm's extraction gives every path of a package (node-tar runs
/// with `preserveOwner: false` and pacote with `noChmod`, so neither sets a
/// group or a directory's setgid bit itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewPaths {
    /// Its group.
    pub gid: u32,
    /// Linux: a new directory inherits the setgid bit of a setgid parent.
    pub setgid_dirs: bool,
}

/// `NewPaths` for paths created in `dir`:
/// - macOS (BSD semantics): the group of the directory; no setgid bit is
///   handed on;
/// - Linux: the group of the directory when it is setgid, and then new
///   directories are setgid too; else the creating process's effective group.
///   (Measured as a non-root user in Docker: `npm ci` in a plain directory
///   gave the user's group, in a setgid directory of another group that
///   group and `drwxr-sr-x` directories.)
///
/// Every path of a package is created inside directories npm (or tar)
/// created under the same rule, so it holds for the whole package.
/// Not covered: a Linux filesystem mounted with `grpid` / `bsdgroups`
/// (BSD semantics there).
pub fn new_paths(dir: &Path) -> std::io::Result<NewPaths> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dir)?;
    let setgid = m.mode() & (libc::S_ISGID as u32) != 0;
    if cfg!(target_os = "macos") {
        Ok(NewPaths { gid: m.gid(), setgid_dirs: false })
    } else if setgid {
        Ok(NewPaths { gid: m.gid(), setgid_dirs: true })
    } else {
        Ok(NewPaths { gid: unsafe { libc::getegid() }, setgid_dirs: false })
    }
}

/// Whether this process can give a file it owns the group `g` (lchown):
/// root, or `g` is its effective group or one of its supplementary groups.
/// The kernel gives a new file a group the user is not in (macOS: from its
/// directory; Linux: a setgid directory's), but the user cannot set one.
pub fn can_give_group(g: u32) -> bool {
    if unsafe { libc::geteuid() } == 0 || unsafe { libc::getegid() } == g {
        return true;
    }
    let mut v = vec![0 as libc::gid_t; 256];
    let n = unsafe { libc::getgroups(v.len() as libc::c_int, v.as_mut_ptr()) };
    v.truncate(n.max(0) as usize);
    v.iter().any(|x| *x as u32 == g)
}

/// What `settle_placed` did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// Paths walked (each touched when asked to).
    pub paths: usize,
    /// Paths whose group (or, for a directory, setgid bit) was changed.
    pub regrouped: usize,
}

/// Make the paths of `dst` that came from `src` (a store entry, or pacote's
/// extraction, just placed there) look as npm's extraction leaves them:
/// `dst` itself and `dst/<p>` for every path `p` under `src`, symlinks not
/// followed.
/// - `touch`: atime and mtime set to now (npm ci leaves every extracted path
///   with the install time; a clone carries the source's).
/// - `want`: the group (and setgid bit of directories) npm's extraction
///   would have given them (`new_paths` of the destination's parent). A
///   clone made directly at the destination already has it, but a placement
///   moved in from the store's `tmp/` keeps what it got there, the store's
///   (#41). Each path is `lstat`ed, and `lchown`ed (a directory: `chmod`ed)
///   only when it differs.
///
/// The walk is over `src`, which nothing changes, so paths other packages
/// are placing under `dst/node_modules` at the same moment are neither
/// touched nor read.
pub fn settle_placed(src: &Path, dst: &Path, touch: bool, want: Option<NewPaths>) -> Result<Settled> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mut s = Settled::default();
    for ent in walkdir::WalkDir::new(src).follow_links(false).follow_root_links(false) {
        let ent = ent?;
        let rel = ent.path().strip_prefix(src).unwrap();
        let out = if rel.as_os_str().is_empty() { dst.to_path_buf() } else { dst.join(rel) };
        let c = CString::new(out.as_os_str().as_bytes())?;
        if let Some(w) = want {
            if regroup(&c, w)? {
                s.regrouped += 1;
            }
        }
        if touch {
            touch_now(&c)?;
        }
        s.paths += 1;
    }
    Ok(s)
}

/// Give `c` (not followed if a symlink) the group of `w` unless it has it
/// already, and a directory the setgid bit when `w` hands it on. True when
/// something was changed.
fn regroup(c: &std::ffi::CStr, w: NewPaths) -> Result<bool> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(c.as_ptr(), &mut st) } != 0 {
        bail!("lstat {:?}: {}", c, std::io::Error::last_os_error());
    }
    let mut changed = false;
    if st.st_gid != w.gid {
        // The owner stays (-1); a group the user is not in fails with EPERM.
        if unsafe { libc::lchown(c.as_ptr(), u32::MAX as libc::uid_t, w.gid as libc::gid_t) } != 0 {
            bail!("lchown {:?} to group {}: {}", c, w.gid, std::io::Error::last_os_error());
        }
        changed = true;
    }
    let is_dir = (st.st_mode & libc::S_IFMT) == libc::S_IFDIR;
    if w.setgid_dirs && is_dir && st.st_mode & libc::S_ISGID == 0 {
        if unsafe { libc::chmod(c.as_ptr(), (st.st_mode & 0o7777) | libc::S_ISGID) } != 0 {
            bail!("chmod g+s {:?}: {}", c, std::io::Error::last_os_error());
        }
        changed = true;
    }
    Ok(changed)
}

/// What a placed file's modification time should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mtime {
    /// Keep the store's time (the moment the package was first extracted).
    Keep,
    /// Set every file and directory to the install time, as `npm ci` leaves them
    /// (pacote extracts with `noMtime`). Matters to mtime-keyed caches.
    Now,
}

/// Set atime/mtime of every entry under each of `dirs` (not following
/// symlinks) to the current time. Paths are collected per directory in
/// parallel, then touched in one flat parallel pass, so one huge package does
/// not serialize the tail.
pub fn touch_trees(dirs: &[std::path::PathBuf]) -> Result<usize> {
    use rayon::prelude::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    // Walk in parallel from each directory's children (and grandchildren for
    // scope directories), so one root does not serialize the walk.
    let mut starts: Vec<std::path::PathBuf> = vec![];
    let mut heads: Vec<CString> = vec![];
    for d in dirs {
        heads.push(CString::new(d.as_os_str().as_bytes())?);
        for e in std::fs::read_dir(d)? {
            let e = e?;
            let p = e.path();
            let ft = e.file_type()?;
            // A symlink here is a workspace or `file:` link: its target is
            // source code npm ci never touches. Only the link itself is touched
            // (lutimes), never what it points to.
            if ft.is_symlink() {
                heads.push(CString::new(p.as_os_str().as_bytes())?);
            } else if ft.is_dir() && e.file_name().to_string_lossy().starts_with('@') {
                heads.push(CString::new(p.as_os_str().as_bytes())?);
                for s in std::fs::read_dir(&p)? {
                    let s = s?;
                    if s.file_type()?.is_symlink() {
                        heads.push(CString::new(s.path().as_os_str().as_bytes())?);
                    } else {
                        starts.push(s.path());
                    }
                }
            } else {
                starts.push(p);
            }
        }
    }
    let mut lists: Vec<Vec<CString>> = starts
        .par_iter()
        .map(|d| -> Result<Vec<CString>> {
            let mut v = vec![];
            // follow_root_links(false): walkdir would otherwise follow a root that is a symlink.
            for ent in walkdir::WalkDir::new(d).follow_links(false).follow_root_links(false) {
                v.push(CString::new(ent?.path().as_os_str().as_bytes())?);
            }
            Ok(v)
        })
        .collect::<Result<_>>()?;
    lists.push(heads);
    let all: Vec<CString> = lists.into_iter().flatten().collect();
    all.par_iter().try_for_each(|c| touch_now(c))?;
    Ok(all.len())
}

/// Clone one regular file found under `store_entry` into `dir/node_modules` and delete the
/// copy: proves the filesystem pair supports reflinks BEFORE anything in the
/// project is removed. Errors carry the same EXDEV / ENOTSUP hints as placement.
pub fn probe(store_entry: &Path, dir: &Path) -> Result<()> {
    let Some(file) = walkdir::WalkDir::new(store_entry)
        .into_iter()
        .flatten()
        .find(|e| e.file_type().is_file())
        .map(|e| e.into_path())
    else {
        return Ok(());
    };
    // Inside node_modules (created if needed): a probe left behind by a killed
    // process must never be a stray file the repository could commit.
    let nm = dir.join("node_modules");
    std::fs::create_dir_all(&nm)?;
    let dst = nm.join(format!(".matchapack-probe-{}", std::process::id()));
    let _ = std::fs::remove_file(&dst);
    let r = sys::clone(&file, &dst).map_err(|e| explain(e, &file, &dst));
    let _ = std::fs::remove_file(&dst);
    r
}

pub fn clone_tree(src: &Path, dst: &Path, mode: Mode) -> Result<()> {
    if mode == Mode::Dir {
        if !sys::DIR_SUPPORTED {
            bail!("--clone-mode dir is only supported on macOS");
        }
        return sys::clone(src, dst).map_err(|e| explain(e, src, dst));
    }
    for ent in walkdir::WalkDir::new(src).follow_links(false) {
        let ent = ent?;
        let rel = ent.path().strip_prefix(src).unwrap();
        let out = dst.join(rel);
        let ft = ent.file_type();
        if ft.is_dir() {
            std::fs::create_dir(&out).with_context(|| format!("mkdir {}", out.display()))?;
        } else if ft.is_file() {
            sys::clone(ent.path(), &out).map_err(|e| explain(e, ent.path(), &out))?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(ent.path())?, &out)?;
        }
    }
    Ok(())
}
