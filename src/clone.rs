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

fn explain(e: std::io::Error, src: &Path, dst: &Path) -> anyhow::Error {
    let hint = match e.raw_os_error() {
        Some(libc::EXDEV) => " (store and target are on different volumes: put the store on the same filesystem)",
        Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) => " (filesystem has no copy-on-write clone support; refusing to fall back to a full copy)",
        _ => "",
    };
    anyhow::anyhow!("clone {} -> {}: {e}{hint}", src.display(), dst.display())
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

/// Set atime/mtime of every entry under `dir` (not following symlinks) to now.
pub fn touch_tree(dir: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let ts = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
    ];
    for ent in walkdir::WalkDir::new(dir).follow_links(false).contents_first(true) {
        let ent = ent?;
        let c = CString::new(ent.path().as_os_str().as_bytes())?;
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        if r != 0 {
            bail!("utimensat {}: {}", ent.path().display(), std::io::Error::last_os_error());
        }
    }
    Ok(())
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
