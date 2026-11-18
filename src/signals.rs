//! Stopping matchapack stops what it started (grimmerk/matchapack-private#64).
//!
//! npm (src/inject.rs) and Yarn (src/yarn1.rs) run in a node child that
//! stays in matchapack's process group, so a signal sent to the whole group
//! (Ctrl-C in a terminal, `kill -- -<pgid>`, `timeout`, most CI
//! cancellations) reaches it as it reaches plain npm. A signal sent to
//! matchapack's pid alone (an IDE's task runner, a supervisor, `kill <pid>`,
//! SIGKILL, a crash) does not; the node side stops then: its hook (src/inject.js,
//! src/yarn1-hook.js) signals its own process when the socket to matchapack
//! closes, or when matchapack tells it to stop, so that npm's own signal
//! handling runs (its reify rollback) as if npm had been signalled. Nothing
//! is forwarded from here: the hook adds no signal when its process has
//! already received one of the three (a group signal), so npm never gets a
//! second one on top of its own.
//!
//! This module is matchapack's half. While the child runs, SIGTERM, SIGHUP
//! and SIGINT are caught instead of ending this process at once. The first
//! one is passed to the hook as a line on the socket, `{"stop":"SIGTERM"}`
//! (`relay_stop`, run beside the server's workers, which keep answering
//! until the child exits); matchapack waits for the child, logs the run as
//! interrupted (the store's lock released, temporary files removed) and ends
//! as the child ended, by the same signal when it died of one (`finish`). A
//! second signal while it waits is passed on too, as the last line
//! (`"last":true`), and then ends matchapack at once, by that signal; the
//! child, already stopping, finishes on its own. Not the same signal again
//! within `SAME_STOP_US`: that is one stop sent twice (GNU `timeout` signals
//! its command, then the command's whole group, matchapack included,
//! microseconds apart). SIGKILL cannot be caught: the socket closes with no
//! line; the hook then uses SIGTERM, or SIGKILL when it had already been
//! asked to stop (a supervisor's SIGTERM, then SIGKILL).
//!
//! An inherited "ignore" (`nohup`, a background command of a shell without
//! job control) is overridden, as Node overrides it for plain npm.
//!
//! The handler is async-signal-safe: atomics, write, sigaction, raise, and
//! errno saved and restored.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// The first of the three signals received since `arm` and when it came, in
/// one word: CLOCK_MONOTONIC in µs << 8 | the signal (0: none). One atomic,
/// so that a copy of the same signal handled on another thread meanwhile
/// never sees the signal without its time.
static FIRST: AtomicU64 = AtomicU64::new(0);
/// The same signal again within this time is the same stop (module doc).
const SAME_STOP_US: u64 = 500_000;
/// The pipe from the handler to `relay_stop` (-1: not armed). The handler
/// writes the signal's number (| LAST for a signal after the first, which
/// ends this process once passed on), `end_relay` a 0.
static NOTICE_R: AtomicI32 = AtomicI32::new(-1);
static NOTICE_W: AtomicI32 = AtomicI32::new(-1);
const LAST: u8 = 0x80;
/// `relay_stop` is reading the pipe: a LAST signal is ended by it, after its
/// line; otherwise by the handler at once.
static RELAYING: AtomicBool = AtomicBool::new(false);
/// The signal that ends this process once its line is written (0: none):
/// set by the handler, ended by `relay_stop`, or by the alarm when that is
/// late (`on_alarm`).
static DIE_SIG: AtomicI32 = AtomicI32::new(0);
/// How long the last line may take before the process ends without it.
const LAST_LINE_SECS: libc::c_uint = 1;

pub const CAUGHT: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

#[cfg(target_os = "macos")]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
#[cfg(not(target_os = "macos"))]
fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

/// `sig`'s default action from now on (a handler may call it).
fn default_action(sig: libc::c_int) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// CLOCK_MONOTONIC in µs (clock_gettime is async-signal-safe).
fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

/// One byte to `relay_stop` (async-signal-safe).
fn notice(b: u8) {
    let fd = NOTICE_W.load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

/// End this process by `sig` now (async-signal-safe): its default action,
/// unblocked on this thread, raised.
fn die(sig: libc::c_int) {
    default_action(sig);
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(sig);
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let saved = unsafe { *errno_ptr() };
    let mine = (now_us() << 8) | sig as u64;
    match FIRST.compare_exchange(0, mine, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => notice(sig as u8),
        Err(first) => {
            // An end already pending (a second signal whose line is not out
            // yet): any further one ends this process now, before the
            // same-signal rule below could fold it into the first.
            if DIE_SIG.load(Ordering::SeqCst) != 0 {
                die(sig);
            }
            let same = (first & 0xff) as libc::c_int == sig && (mine >> 8).saturating_sub(first >> 8) < SAME_STOP_US;
            if !same {
                // A second one while the child stops: passed on as well (the
                // hook signals npm with it, as plain npm would have got it),
                // then this process ends by it at once: by `relay_stop` once
                // the line is written, or here when nothing relays. Written
                // first, checked after: `relay_stop` stops relaying first and
                // reads what is left after.
                // Bounded: a third signal, or the alarm, ends it anyway (the
                // relay may wait for the server's writer lock, or for a hook
                // that does not read); the hook then sees a close after a
                // stop, which ends npm too (SIGKILL).
                notice(sig as u8 | LAST);
                if !RELAYING.load(Ordering::SeqCst) || DIE_SIG.swap(sig, Ordering::SeqCst) != 0 {
                    die(sig);
                }
                unsafe {
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = on_alarm as *const () as libc::sighandler_t;
                    libc::sigemptyset(&mut sa.sa_mask);
                    libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());
                    libc::alarm(LAST_LINE_SECS);
                }
            }
        }
    }
    unsafe { *errno_ptr() = saved };
}

/// The last line was not written in time: end by the signal that asked for it.
extern "C" fn on_alarm(_: libc::c_int) {
    let sig = DIE_SIG.load(Ordering::SeqCst);
    die(if sig != 0 { sig } else { libc::SIGTERM });
}

/// The handlers of one child's run (`arm`); `disarm` when the child is gone.
pub struct Armed {
    old: Vec<(libc::c_int, libc::sigaction)>,
}

/// Catch the three signals from now on. Called before the child is started,
/// so that none falls between the start and the handlers: one that comes
/// before the server runs is passed on as soon as it does (`relay_stop`).
pub fn arm() -> Armed {
    let mut fds = [-1 as libc::c_int; 2];
    let piped = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0;
    arm_with(piped.then_some(fds))
}

/// `arm` with the pipe to `relay_stop`, or without one (`pipe` failed: no
/// descriptor left). Without it nothing is caught: a signal ends this
/// process at once, as before #64, and the hook stops npm when the socket
/// closes; a handler with nowhere to say the signal would leave npm running
/// while matchapack waited for it.
fn arm_with(fds: Option<[libc::c_int; 2]>) -> Armed {
    FIRST.store(0, Ordering::SeqCst);
    DIE_SIG.store(0, Ordering::SeqCst);
    let Some(fds) = fds else {
        return Armed { old: Vec::new() };
    };
    {
        for fd in fds {
            // Close-on-exec (no process this one starts inherits them); the
            // handler's write never blocks.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        unsafe {
            let fl = libc::fcntl(fds[1], libc::F_GETFL);
            libc::fcntl(fds[1], libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        NOTICE_R.store(fds[0], Ordering::SeqCst);
        NOTICE_W.store(fds[1], Ordering::SeqCst);
    }
    Armed { old: install() }
}

/// The handler for the three; the dispositions it replaces.
fn install() -> Vec<(libc::c_int, libc::sigaction)> {
    CAUGHT
        .iter()
        .map(|&sig| unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            // Restarted: the serving threads' reads and writes are not to see
            // EINTR because of this. One handler at a time.
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            for s in CAUGHT {
                libc::sigaddset(&mut sa.sa_mask, s);
            }
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, &sa, &mut old);
            (sig, old)
        })
        .collect()
}

/// In the child, between fork and exec (async-signal-safe): the three
/// signals' default actions, so that one arriving before the exec ends the
/// child as it would end a plain npm, instead of running this process's
/// handler in the child's copy.
pub fn child_defaults() {
    for sig in CAUGHT {
        default_action(sig);
    }
}

impl Armed {
    /// After the child's run (it has exited and the server has returned).
    /// Without a signal the earlier handlers come back (a signal now ends
    /// this process at once, as before #64). After one, the handler stays: a
    /// second signal ends the process at once while the run is logged.
    /// The three are blocked on this thread meanwhile (the serving threads
    /// are gone: none can run the handler), so that none falls between the
    /// check and the restore; one that came on another thread all the same
    /// puts the handler back. A signal held meanwhile is delivered when they
    /// are unblocked, with the disposition then in place.
    pub fn disarm(self) {
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut before: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
            for s in CAUGHT {
                libc::sigaddset(&mut mask, s);
            }
            libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut before);
        }
        if FIRST.load(Ordering::SeqCst) == 0 {
            for (sig, old) in &self.old {
                unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
            }
            if FIRST.load(Ordering::SeqCst) != 0 && !self.old.is_empty() {
                install();
            }
        }
        for fd in [NOTICE_W.swap(-1, Ordering::SeqCst), NOTICE_R.swap(-1, Ordering::SeqCst)] {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &before, std::ptr::null_mut()) };
    }
}

/// Run beside the server (src/serve.rs `serve_lines`) while the child runs:
/// passes every signal on to the hook as a line, `{"stop":"SIGTERM"}`, the
/// one that ends this process with `"last":true` (then ends it by that
/// signal), written with `write` (under the server's writer lock; `true`:
/// the last line, to be written within a bound or not at all). Returns at
/// once when not armed, and when `end_relay` is called.
pub fn relay_stop(write: impl Fn(&[u8], bool)) {
    let fd = NOTICE_R.load(Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    RELAYING.store(true, Ordering::SeqCst);
    let pass_on = |b: u8| {
        let sig = (b & !LAST) as libc::c_int;
        let last = b & LAST != 0;
        write(format!("{{\"stop\":\"{}\"{}}}\n", name(sig), if last { ",\"last\":true" } else { "" }).as_bytes(), last);
        if last {
            die(sig);
        }
    };
    loop {
        let mut b = 0u8;
        let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
        if n == 1 && b != 0 {
            pass_on(b);
            continue;
        }
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break;
    }
    // A last signal whose handler saw this still relaying: ended here.
    RELAYING.store(false, Ordering::SeqCst);
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    loop {
        let mut b = 0u8;
        if unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) } != 1 {
            break;
        }
        if b & LAST != 0 {
            die((b & !LAST) as libc::c_int);
        }
    }
}

/// The child has exited (src/inject.rs `run_serving`): `relay_stop`
/// returns.
pub fn end_relay() {
    let fd = NOTICE_W.load(Ordering::SeqCst);
    if fd >= 0 {
        let b = 0u8;
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

/// The signal that stopped this run, if one came (since the last `arm`).
pub fn received() -> Option<i32> {
    let s = (FIRST.load(Ordering::SeqCst) & 0xff) as i32;
    (s != 0).then_some(s)
}

pub fn name(sig: i32) -> &'static str {
    match sig {
        libc::SIGTERM => "SIGTERM",
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        _ => "a signal",
    }
}

/// The interrupted run's line on stderr, written without a panic (after
/// SIGHUP the terminal may be gone). `code`: the child's status.
pub fn say(sig: i32, what: &str, code: Option<i32>) {
    use std::io::Write;
    let how = match code {
        Some(c) if c > 128 && CAUGHT.contains(&(c - 128)) => format!("{what} ended by {}", name(c - 128)),
        Some(c) => format!("{what} exited with status {c}"),
        None => format!("{what} has exited"),
    };
    let _ = writeln!(std::io::stderr(), "matchapack: stopped by {}; {how}", name(sig));
}

/// End this process with `code`. When a signal stopped the run and the
/// child died of one of the three (`code` = 128 + it), by that signal
/// itself, so that a parent shell or script sees a process killed by it, as
/// it would see plain npm or Yarn (bash stops a script on a child killed by
/// SIGINT, not on one that exited 130).
pub fn finish(code: i32) -> ! {
    if received().is_some() && code > 128 && CAUGHT.contains(&(code - 128)) {
        use std::io::Write;
        let sig = code - 128;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        default_action(sig);
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(sig);
        }
    }
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler_of(sig: libc::c_int) -> libc::sighandler_t {
        unsafe {
            let mut cur: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut cur);
            cur.sa_sigaction
        }
    }

    /// Without the pipe (`pipe` failed), nothing is caught: the handlers stay
    /// as they were (a signal ends the process, as before #64).
    #[test]
    fn without_the_pipe_nothing_is_caught() {
        let before: Vec<_> = CAUGHT.iter().map(|&s| handler_of(s)).collect();
        let armed = arm_with(None);
        let during: Vec<_> = CAUGHT.iter().map(|&s| handler_of(s)).collect();
        armed.disarm();
        assert_eq!(before, during);
        assert!(during.iter().all(|h| *h != on_signal as *const () as libc::sighandler_t));
    }
}
