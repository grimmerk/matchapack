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

/// Set atime/mtime to the current time on every path of `dst` that came from
/// `src` (a store entry just placed there): `dst` itself and `dst/<p>` for
/// every path `p` under `src`, symlinks not followed. The walk is over `src`,
/// which nothing changes, so paths other packages are placing under
/// `dst/node_modules` at the same moment are neither touched nor read.
pub fn touch_placed(src: &Path, dst: &Path) -> Result<usize> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mut n = 0;
    for ent in walkdir::WalkDir::new(src).follow_links(false).follow_root_links(false) {
        let ent = ent?;
        let rel = ent.path().strip_prefix(src).unwrap();
        let out = if rel.as_os_str().is_empty() { dst.to_path_buf() } else { dst.join(rel) };
        touch_now(&CString::new(out.as_os_str().as_bytes())?)?;
        n += 1;
    }
    Ok(n)
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
    let dst = nm.join(format!(".thinclone-probe-{}", std::process::id()));
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
