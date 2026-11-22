#!/usr/bin/env python3
"""The benchmark behind the README's disk, time and CPU numbers (issue grimmerk/matchapack-private#7).
Started by scripts/bench.sh, which keeps the Mac awake; see that file for
the usage. Plain npm / Yarn 1 against matchapack on public projects pinned
to a commit, in interleaved rounds, with every run recorded.

Everything it writes is under the work directory (--work):
  src/<project>        the project's source at its pinned commit (git)
  runs/<project>/...   the worktrees the installs run in (removed after use)
  stores/<project>/... matchapack stores (empty, filled by another npm, warm)
  npm-cache/           npm's cache, shared by both sides, primed first
  npm-alt/             another npm, installed from the registry into a
                       scratch prefix (fills the "first use" store)
  yarn-mirror/         Yarn 1's offline mirror: the tarballs, primed first
  yarn-cache/          Yarn 1's warm cache, shared by both sides
  mp/                  matchapack's config and log directories
  env/                 empty npmrc / yarnrc files (the user's are not read)
  logs/                one log per install, and bench.out
  results/             runs.jsonl (one line per timed run), projects.jsonl,
                       meta.jsonl (machine, versions, top CPU users),
                       sentinels.jsonl, summary.md
  .matchapack-bench    the marker: this directory belongs to the benchmark
The benchmark starts only in a new or empty directory (and marks it) or in
a marked one; --summary-only and --cleanup need the marker too. --cleanup
removes everything but logs/ and results/.
"""
import argparse, datetime, fcntl, json, os, re, resource, shutil, signal, statistics, struct, subprocess, sys, time

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(HERE, "target", "release", "matchapack")
GIB = 1024 ** 3
MIB = 1024 ** 2

# The projects. Public, pinned to a commit. "dir" is the npm or Yarn project
# inside the repository. Sizes ("est_gib": node_modules of one plain
# install, the store or cache, the sources) are for the disk check before a
# run, measured in the smoke run of 2026-11-21; the real sizes are recorded
# per run.
PROJECTS = {
    "small": {
        "kind": "npm",
        "repo": "this repository (fixtures/small)",
        "dir": ".",
        "about": "fixtures/small of this repository: express, esbuild, chokidar, lodash, debug, chalk, semver",
        "est_gib": 1.0,
    },
    "medium": {
        "kind": "npm",
        "repo": "zwave-js/zwave-js-ui",
        "commit": "7af2ab653895641744179323c7ee1c5707fd4363",
        "dir": ".",
        "about": "one package, 1,686 lockfile entries; the project of the Linux CI job (linux-large.yml)",
        "est_gib": 5.0,
    },
    "large": {
        "kind": "npm",
        "repo": "apache/superset",
        "commit": "90cf89255c213c9acdafc0a00d458eff989a9470",
        "dir": "superset-frontend",
        "about": "an npm workspaces monorepo, 2,921 lockfile entries",
        "est_gib": 14.0,
    },
    "yarn": {
        "kind": "yarn",
        "repo": "cornerstonejs/cornerstone3D",
        # A Yarn 1 commit (the project moved to pnpm on 2026-06-08; from
        # 2026-04 on its yarn.lock no longer matched package.json, which
        # --frozen-lockfile refuses). This commit is a small real bump:
        # axios 1.12.0 -> 1.13.5 in `resolutions` (package.json 1 line,
        # yarn.lock 17+/12-); the incremental case installs its parent,
        # then this commit.
        "commit": "d0e0e37510e27a2f7070475fedf7a1fc7cff204a",
        "dir": ".",
        "about": "a Yarn 1 workspaces monorepo (10 workspace globs)",
        "est_gib": 10.0,
    },
}
NPM_CASES = ["npm-empty-store", "npm-first-use", "npm-fresh-worktree", "npm-reinstall"]
YARN_CASES = ["yarn-empty-cache", "yarn-fresh-worktree", "yarn-installed", "yarn-incremental"]
CASE_TEXT = {
    "npm-empty-store": "empty store (npm's cache warm)",
    "npm-first-use": "first use per npm version (store filled by npm {fill_npm})",
    "npm-fresh-worktree": "a fresh worktree, warm store (the main use case)",
    "npm-reinstall": "reinstall over an existing node_modules",
    "yarn-empty-cache": "empty Yarn cache (tarballs in the offline mirror)",
    "yarn-fresh-worktree": "a fresh worktree, warm cache (the main use case)",
    "yarn-installed": "already installed (integrity check, nothing to do)",
    "yarn-incremental": "incremental install after a lockfile change",
}
# Only the priming install of each project may download (into the scratch
# npm cache or Yarn's offline mirror). Every later install, timed or setup,
# is offline: a cache or mirror miss fails it instead of timing a download.
NPM_PRIME_ARGS = ["ci", "--no-audit", "--no-fund", "--prefer-offline"]
NPM_ARGS = ["ci", "--no-audit", "--no-fund", "--offline"]
# --no-default-rc: Yarn reads no .yarnrc or .npmrc of its own finding (the
# user's ~/.yarnrc could name a yarn-path or move the install); the offline
# mirror comes from the environment (yarn_env), the cache from the command.
YARN_PRIME_ARGS = ["install", "--frozen-lockfile", "--ignore-scripts", "--non-interactive", "--no-default-rc"]
YARN_ARGS = YARN_PRIME_ARGS + ["--offline"]
MARKER = ".matchapack-bench"
# A resumed benchmark must measure with what it started with: these
# settings and toolchain facts are compared with the first record of the
# bench id, and a difference needs a new --bench-id.
RESUME_SETTINGS = ("rounds", "disk_rounds", "load_max", "load_wait", "settle", "timeout", "fill_npm", "smoke")
# The implementation is named by content, not by commit: a dirty checkout
# says nothing about which edits it holds, so the SHA-256 of bench.py and
# bench.sh, and of the matchapack binary, are compared (a change committed
# or not refuses the resume; a commit that changes neither does not).
RESUME_TOOLCHAIN = ("model", "cpu", "macos", "filesystem", "node", "npm", "yarn", "matchapack", "matchapack_sha256",
                    "bench_files_sha256")

CURRENT = {"pgid": None}


OUT = {"file": None}


def say(*a):
    """To stdout and to logs/bench.out; a closed terminal or pipe does not
    stop the run."""
    line = " ".join([time.strftime("%H:%M:%S")] + [str(x) for x in a])
    if OUT["file"]:
        OUT["file"].write(line + "\n")
        OUT["file"].flush()
    try:
        print(line, flush=True)
    except (OSError, ValueError):
        pass


def die(msg, code=1):
    say("ERROR:", msg)
    sys.exit(code)


# --- the work directory and what may be removed in it -------------------

class Work:
    def __init__(self, root):
        self.root = root

    def p(self, *parts):
        return os.path.join(self.root, *parts)

    def inside(self, path):
        """A path strictly inside the work directory, as given and resolved."""
        if not path or not os.path.isabs(path):
            return False
        real = os.path.realpath(path)
        return path.startswith(self.root + os.sep) and real.startswith(self.root + os.sep) and real != self.root

    def remove(self, path):
        """Removes a tree of the work directory, never anything outside it."""
        if not self.inside(path):
            die(f"refusing to remove {path!r}: not inside the work directory {self.root}")
        if not os.path.lexists(path):
            return
        r = subprocess.run(["/bin/rm", "-rf", "--", path], stdin=subprocess.DEVNULL)
        if r.returncode != 0 or os.path.lexists(path):
            subprocess.run(["/bin/chmod", "-R", "u+w", "--", path], stdin=subprocess.DEVNULL)
            subprocess.run(["/bin/rm", "-rf", "--", path], stdin=subprocess.DEVNULL)
        if os.path.lexists(path):
            die(f"could not remove {path}")


def work_state(root):
    """What --work names: "missing", "empty", "owned" (it has the marker),
    "foreign" (anything else in it) or "not-dir"."""
    if not os.path.lexists(root):
        return "missing"
    if os.path.islink(root) or not os.path.isdir(root):
        return "not-dir"
    names = os.listdir(root)
    if MARKER in names and os.path.isfile(os.path.join(root, MARKER)):
        return "owned"
    return "empty" if not names else "foreign"


def store_ready(store, facts):
    """Whether `store` was filled completely (Bench.fill_store), with what
    `facts` names (the npm that filled it, matchapack, the commit)."""
    try:
        with open(store + ".ready") as f:
            return os.path.isdir(store) and json.load(f) == facts
    except (OSError, ValueError):
        return False


def resume_conflicts(first, machine_now, args):
    """How the current tools and settings differ from those the bench id
    was started with (`first`, its first meta record): empty when it may
    resume."""
    out = []
    was_m, was_a = first.get("machine") or {}, first.get("args") or {}
    now_a = vars(args) if not isinstance(args, dict) else args
    for k in RESUME_TOOLCHAIN:
        if was_m.get(k) != machine_now.get(k):
            out.append(f"{k} {was_m.get(k)!r} then, {machine_now.get(k)!r} now")
    for k in RESUME_SETTINGS:
        if was_a.get(k) != now_a.get(k):
            out.append(f"--{k.replace('_', '-')} {was_a.get(k)!r} then, {now_a.get(k)!r} now")
    # `small` is built from this checkout's fixtures/small (the other
    # projects are pinned by commit): its tree must be the same, and clean.
    if "small" in set(was_a.get("projects") or []) | set(now_a.get("projects") or []):
        if was_m.get("small_fixture_tree") != machine_now.get("small_fixture_tree"):
            out.append(f"fixtures/small tree {was_m.get('small_fixture_tree')!r} then, {machine_now.get('small_fixture_tree')!r} now")
        if machine_now.get("small_fixture_dirty"):
            out.append("fixtures/small has uncommitted changes now")
    return out


def nearest_existing(path):
    while not os.path.lexists(path):
        path = os.path.dirname(path)
    return path


def repair_tail(path):
    """Cuts an incomplete last line (a write torn by a crash) off a JSONL
    file, so that the next record starts on a line of its own. Complete
    lines are left as they are. Returns the number of bytes removed."""
    try:
        f = open(path, "rb+")
    except FileNotFoundError:
        return 0
    with f:
        size = f.seek(0, os.SEEK_END)
        if size == 0:
            return 0
        f.seek(size - 1)
        if f.read(1) == b"\n":
            return 0
        # Back to the last newline, a block at a time.
        end, keep = size, 0
        while end > 0:
            start = max(0, end - 65536)
            f.seek(start)
            block = f.read(end - start)
            i = block.rfind(b"\n")
            if i >= 0:
                keep = start + i + 1
                break
            end = start
        f.truncate(keep)
        f.flush()
        os.fsync(f.fileno())
        return size - keep


def append_line(path, rec):
    """One complete JSON line, flushed to disk: a run is recorded whole or
    not at all. Called only while the work directory's lock is held, so the
    torn tail of an earlier, interrupted write can be cut off first (it
    would otherwise swallow this record into an unreadable line)."""
    cut = repair_tail(path)
    if cut:
        say(f"note: {os.path.basename(path)} ended in an incomplete line ({cut} bytes, an interrupted write): removed")
    with open(path, "a") as f:
        f.write(json.dumps(rec, sort_keys=True) + "\n")
        f.flush()
        os.fsync(f.fileno())


def read_lines(path):
    out = []
    try:
        with open(path) as f:
            for line in f:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
    except FileNotFoundError:
        pass
    return out


def write_atomic(path, text):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(text)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


# --- processes -----------------------------------------------------------

def on_signal(signum, frame):
    raise KeyboardInterrupt(f"signal {signum}")


def run(cmd, cwd, env, log=None, timeout=None, check=True):
    """Runs a command in its own process group; output to `log` (appended)
    or captured. Returns (exit status or "timeout", captured output)."""
    out = open(log, "ab") if log else subprocess.PIPE
    try:
        if log:
            out.write(("\n$ " + " ".join(cmd) + "   (in " + cwd + ")\n").encode())
            out.flush()
        p = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT,
                             stdin=subprocess.DEVNULL, start_new_session=True)
        CURRENT["pgid"] = p.pid
        try:
            data, _ = p.communicate(timeout=timeout)
            rc = p.returncode
        except subprocess.TimeoutExpired:
            kill_group(p)
            data, rc = b"", "timeout"
        except BaseException:
            kill_group(p)
            raise
        finally:
            CURRENT["pgid"] = None
    finally:
        if log:
            out.close()
    text = data.decode(errors="replace") if data else ""
    if check and rc != 0:
        tail = text[-3000:] if text else (open(log, errors="replace").read()[-3000:] if log else "")
        die(f"{' '.join(cmd)} in {cwd}: exit {rc}\n{tail}")
    return rc, text


def group_alive(pgid):
    """Whether any process is left in the process group."""
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def kill_group(p, grace=30):
    """SIGTERM to the command's process group; after `grace` seconds,
    SIGKILL to whatever of the group is left (a worker that outlived its
    parent included); the direct child is reaped either way."""
    pgid = p.pid
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline:
        p.poll()  # reap the child: a zombie still counts as in the group
        if not group_alive(pgid):
            break
        time.sleep(0.2)
    if group_alive(pgid):
        try:
            os.killpg(pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            p.poll()
            if not group_alive(pgid):
                break
            time.sleep(0.2)
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    if group_alive(pgid):
        say(f"warning: processes of group {pgid} are still there after SIGKILL")


def out_of(cmd, cwd=None, env=None):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip()


# --- disk: blocks shared with the store or cache --------------------------
# The same test as scripts/shared-blocks.py (a file shares when its first
# extent, device and physical offset by F_LOG2PHYS_EXT, is the first extent
# of a file in the store or cache), plus the bytes each file allocates
# (st_blocks), the package count and the node_modules size.

F_LOG2PHYS_EXT = 65


def first_block(p):
    fd = os.open(p, os.O_RDONLY)
    try:
        out = fcntl.fcntl(fd, F_LOG2PHYS_EXT, struct.pack("=Iqq", 0, 1, 0))
        return os.fstat(fd).st_dev, struct.unpack("=Iqq", out)[2]
    finally:
        os.close(fd)


def source_index(src):
    offsets = set()
    if not src or not os.path.isdir(src):
        return offsets
    for d, dirs, names in os.walk(src):
        for n in names:
            p = os.path.join(d, n)
            try:
                st = os.lstat(p)
                if (st.st_mode & 0o170000) == 0o100000 and st.st_size > 0:
                    offsets.add(first_block(p))
            except OSError:
                pass
    return offsets


def dir_alloc(path):
    """Bytes allocated by the regular files of a tree (clones counted in
    full, as du does), and their count."""
    total = n = 0
    for d, dirs, names in os.walk(path):
        for x in names:
            try:
                st = os.lstat(os.path.join(d, x))
            except OSError:
                continue
            if (st.st_mode & 0o170000) == 0o100000:
                total += st.st_blocks * 512
                n += 1
    return total, n


def disk(tree, src):
    """node_modules of `tree` (every regular file below a node_modules
    directory, workspaces' too) against `src` (a store or a Yarn cache)."""
    t0 = time.monotonic()
    offsets = source_index(src)
    r = dict(files=0, empty=0, bytes=0, alloc=0, shared_files=0, shared_bytes=0, shared_alloc=0,
             new_alloc=0, packages=0, links=0)
    for d, dirs, names in os.walk(tree):
        rel = d[len(tree):].split(os.sep)
        if os.path.basename(d) == "node_modules":
            # Package roots: node_modules/<name>, node_modules/@scope/<name>.
            for x in os.listdir(d):
                if x.startswith("."):
                    continue
                q = os.path.join(d, x)
                cands = [os.path.join(q, y) for y in os.listdir(q)] if x.startswith("@") and os.path.isdir(q) and not os.path.islink(q) else [q]
                for c in cands:
                    if os.path.islink(c):
                        r["links"] += 1
                    elif os.path.isfile(os.path.join(c, "package.json")):
                        r["packages"] += 1
        if "node_modules" not in rel:
            continue
        for x in names:
            p = os.path.join(d, x)
            try:
                st = os.lstat(p)
            except OSError:
                continue
            if (st.st_mode & 0o170000) != 0o100000:
                continue
            if st.st_size == 0:
                r["empty"] += 1
                continue
            a = st.st_blocks * 512
            r["files"] += 1
            r["bytes"] += st.st_size
            r["alloc"] += a
            try:
                shared = first_block(p) in offsets
            except OSError:
                shared = False
            if shared:
                r["shared_files"] += 1
                r["shared_bytes"] += st.st_size
                r["shared_alloc"] += a
            else:
                r["new_alloc"] += a
    if src and os.path.isdir(src):
        r["source_alloc"], r["source_files"] = dir_alloc(src)
    r["secs"] = round(time.monotonic() - t0, 1)
    return r


# --- the machine, and what must stay untouched ----------------------------

SENTINEL_DEPTHS = {
    "~/.cache/matchapack": 3,
    "~/.config/matchapack": 3,
    "~/.npm": 4,
    "~/Library/Caches/Yarn": 2,
}


def tree_state(root, depth):
    """Every entry (files and directories) down to `depth` levels below
    `root`, by relative path, kind, size, mtime (ns) and inode, as one
    SHA-256 and a count; also the newest mtime, for the report. Two states
    are equal only if no entry at those levels was added, removed or
    rewritten (a change deeper down shows only where it changed a
    directory's mtime within the levels)."""
    import hashlib
    h = hashlib.sha256()
    n = 0
    newest, newest_path = None, None
    base = root.count(os.sep)
    entries = []
    for d, dirs, names in os.walk(root):
        dirs.sort()
        if d.count(os.sep) - base >= depth:
            dirs[:] = []
        for x in dirs + names:
            p = os.path.join(d, x)
            try:
                st = os.lstat(p)
            except OSError:
                continue
            entries.append((os.path.relpath(p, root), st.st_mode & 0o170000, st.st_size, st.st_mtime_ns, st.st_ino))
            if newest is None or st.st_mtime > newest:
                newest, newest_path = st.st_mtime, p
    st = os.lstat(root)
    entries.append((".", st.st_mode & 0o170000, st.st_size, st.st_mtime_ns, st.st_ino))
    for e in sorted(entries):
        h.update(repr(e).encode())
        n += 1
    return {"digest": h.hexdigest(), "entries": n, "depth": depth, "mtime": newest, "path": newest_path}


def sentinels():
    """The user's real matchapack state and npm / Yarn caches, entry by
    entry a few levels deep (tree_state). The run must not change them;
    another process on the machine can, which the report then says."""
    out = {}
    for name, depth in SENTINEL_DEPTHS.items():
        root = os.path.expanduser(name)
        out[name] = tree_state(root, depth) if os.path.lexists(root) else None
    return out


def sentinel_changes(a, b, since):
    """What changed, with up to 10 of the entries written since `since`
    (another process may have written them: the times tell)."""
    out = []
    for k in a:
        x, y = a[k], b.get(k)
        if (x or {}).get("digest") == (y or {}).get("digest") and bool(x) == bool(y):
            continue
        if not y:
            out.append(f"{k}: gone")
            continue
        if not x:
            out.append(f"{k}: created during the run")
            continue
        root = os.path.expanduser(k)
        newer = []
        for d, dirs, names in os.walk(root):
            for name in dirs + names:
                p = os.path.join(d, name)
                try:
                    m = os.lstat(p).st_mtime
                except OSError:
                    continue
                if m > since:
                    newer.append((m, p))
            if len(newer) > 200:
                break
        newer.sort()
        listed = ", ".join(f"{p} ({datetime.datetime.fromtimestamp(m).isoformat(timespec='seconds')})" for m, p in newer[:10])
        out.append(f"{k}: entries changed within {x.get('depth')} levels ({x.get('entries')} before, {y.get('entries')} after); "
                   f"{len(newer)}{'+' if len(newer) > 200 else ''} entries with a time after the start: {listed or 'none'}")
    return out


def top_cpu():
    return out_of(["/bin/ps", "-Ao", "pcpu,comm", "-r"]).splitlines()[:11]


SPOTLIGHT = ("mds", "mds_stores", "mdworker", "mdworker_shared", "mdsync", "corespotlightd")


def noise():
    """CPU of other processes when a run starts (ps's %CPU, a recent
    average): Spotlight's indexers, a virtual machine, the top five."""
    rows = []
    for line in out_of(["/bin/ps", "-Ao", "pcpu,comm"]).splitlines()[1:]:
        parts = line.strip().split(None, 1)
        if len(parts) != 2:
            continue
        try:
            rows.append((float(parts[0]), os.path.basename(parts[1])))
        except ValueError:
            continue
    rows.sort(reverse=True)
    return {
        "spotlight_pcpu": round(sum(pc for pc, c in rows if c in SPOTLIGHT), 1),
        "vm_pcpu": round(sum(pc for pc, c in rows if "Virtualization.VirtualMachine" in c), 1),
        "top": [f"{pc:.1f} {c}" for pc, c in rows[:5]],
    }


def free_bytes(path):
    st = os.statvfs(path)
    return st.f_bavail * st.f_frsize


def files_sha256(paths):
    """One SHA-256 over the files' bytes, in order (each with its length,
    so that bytes cannot move from one file to the next unnoticed)."""
    import hashlib
    h = hashlib.sha256()
    for p in paths:
        with open(p, "rb") as f:
            data = f.read()
        h.update(f"{os.path.basename(p)}\0{len(data)}\0".encode())
        h.update(data)
    return h.hexdigest()


def machine(work_root, env):
    def sysctl(k):
        return out_of(["/usr/sbin/sysctl", "-n", k])
    dev = out_of(["/bin/df", work_root]).splitlines()[-1].split()[0]
    fs = ""
    for line in out_of(["/usr/sbin/diskutil", "info", dev]).splitlines():
        if "File System Personality" in line:
            fs = line.split(":", 1)[1].strip()
    return {
        "model": sysctl("hw.model"),
        "cpu": sysctl("machdep.cpu.brand_string"),
        "cores": sysctl("hw.ncpu"),
        "ram_gib": round(int(sysctl("hw.memsize") or 0) / GIB),
        "macos": out_of(["/usr/bin/sw_vers", "-productVersion"]) + " (" + out_of(["/usr/bin/sw_vers", "-buildVersion"]) + ")",
        "filesystem": fs,
        "device": dev,
        "node": out_of(["node", "-v"], env=env),
        "npm": out_of(["npm", "-v"], env=env),
        "yarn": out_of(["yarn", "--no-default-rc", "--version"], env=env),
        "matchapack": out_of([BIN, "--version"], env=env),
        "matchapack_sha256": files_sha256([BIN]),
        "bench_files_sha256": files_sha256([os.path.join(HERE, "scripts", "bench.py"), os.path.join(HERE, "scripts", "bench.sh")]),
        "small_fixture_tree": out_of(["git", "-C", HERE, "rev-parse", "HEAD:fixtures/small"]),
        "small_fixture_dirty": bool(out_of(["git", "-C", HERE, "status", "--porcelain", "--untracked-files=all", "--", "fixtures/small"])),
        "bench_commit": out_of(["git", "-C", HERE, "rev-parse", "HEAD"]),
        "bench_dirty": bool(out_of(["git", "-C", HERE, "status", "--porcelain", "--untracked-files=no"])),
        "power": out_of(["/usr/bin/pmset", "-g", "ps"]).splitlines()[:1],
    }


# --- the environment both sides get ---------------------------------------

def base_env(w, args):
    drop = ("npm_config_", "yarn_", "matchapack")
    env = {k: v for k, v in os.environ.items()
           if not k.lower().startswith(drop) and k not in ("GITHUB_TOKEN", "NODE_OPTIONS", "MATCHAPACK")}
    env.update({
        # npm: the scratch cache, no user or global npmrc, no update check.
        "npm_config_cache": w.p("npm-cache"),
        "NPM_CONFIG_USERCONFIG": w.p("env", "npmrc"),
        "NPM_CONFIG_GLOBALCONFIG": w.p("env", "global-npmrc"),
        "npm_config_update_notifier": "false",
        # Yarn 1: no ~/.yarnrc as its user config, no self-update check (it
        # would write ~/.yarnrc); every Yarn command also has
        # --no-default-rc (YARN_PRIME_ARGS); its cache and mirror are set
        # per run.
        "YARN_USERCONFIG": w.p("env", "yarnrc"),
        "YARN_DISABLE_SELF_UPDATE_CHECK": "true",
        # matchapack: scratch config and logs; the trial scope is the work
        # directory. The store is set per run.
        "MATCHAPACK_CONFIG_DIR": w.p("mp", "config"),
        "MATCHAPACK_LOG_DIR": w.p("mp", "log"),
        "MATCHAPACK_PATHS": w.root,
        # A postinstall of the large project (@scarf/scarf) reports to the
        # network unless told not to: off, for both sides.
        "SCARF_ANALYTICS": "false",
    })
    return env


def npm_env(env, store, alt_npm=None):
    e = dict(env, MATCHAPACK_STORE=store)
    if alt_npm:
        e["PATH"] = alt_npm + os.pathsep + e["PATH"]
    return e


def yarn_env(env, w, cache):
    return dict(env, YARN_CACHE_FOLDER=cache, YARN_YARN_OFFLINE_MIRROR=w.p("yarn-mirror"),
                MATCHAPACK_STORE=w.p("stores", "yarn-unused"))


def mp_env(env):
    # A decline would make "matchapack" a plain run: fail instead.
    return dict(env, MATCHAPACK_NO_FALLBACK="1")


# --- projects --------------------------------------------------------------

def git(w, *a, cwd=None, env=None, log=None):
    run(["git", *a], cwd or w.root, env or os.environ.copy(), log=log)


def fetch_project(w, name, spec, env, log):
    """src/<name>: a git repository at the pinned commit (depth 2, so the
    Yarn project's parent commit is there). A checkout left by an earlier
    run is used only when it is what is asked for now: the pinned commit,
    or for `small` the fixture from the commit this script runs at;
    otherwise it is fetched again."""
    src = w.p("src", name)
    if os.path.isdir(os.path.join(src, ".git")):
        if name == "small":
            want = out_of(["git", "-C", HERE, "rev-parse", "HEAD"])
            have = out_of(["git", "log", "-1", "--format=%s"], cwd=src).split()[-1:]
            ok = have == [want]
        else:
            ok = out_of(["git", "rev-parse", "HEAD"], cwd=src) == spec["commit"]
        if ok:
            return src
        say(f"  {src} is not at the requested revision: fetching it again")
        w.remove(src)
    tmp = src + ".partial"
    w.remove(tmp)
    os.makedirs(tmp)
    genv = dict(env, GIT_TERMINAL_PROMPT="0", GIT_CONFIG_NOSYSTEM="1")
    if name == "small":
        # This repository's fixture, from the commit the bench runs at.
        commit = out_of(["git", "-C", HERE, "rev-parse", "HEAD"])
        arch = subprocess.run(["git", "-C", HERE, "archive", "--format=tar", commit, "fixtures/small"], capture_output=True, check=True).stdout
        subprocess.run(["tar", "-x", "-C", tmp, "--strip-components=2"], input=arch, check=True)
        git(w, "init", "-q", "-b", "main", cwd=tmp, env=genv)
        git(w, "add", "package.json", "package-lock.json", cwd=tmp, env=genv)
        git(w, "-c", "user.name=bench", "-c", "user.email=bench@localhost", "-c", "commit.gpgsign=false",
            "commit", "-q", "-m", f"fixtures/small at {commit}", cwd=tmp,
            env=dict(genv, GIT_AUTHOR_DATE="2026-11-21T00:00:00Z", GIT_COMMITTER_DATE="2026-11-21T00:00:00Z"))
        spec["commit"] = out_of(["git", "rev-parse", "HEAD"], cwd=tmp)
        spec["source_commit"] = commit
    else:
        git(w, "init", "-q", cwd=tmp, env=genv)
        git(w, "fetch", "-q", "--depth", "2", f"https://github.com/{spec['repo']}", spec["commit"], cwd=tmp, env=genv, log=log)
        git(w, "-c", "advice.detachedHead=false", "checkout", "-q", "--detach", spec["commit"], cwd=tmp, env=genv, log=log)
    os.rename(tmp, src)
    return src


def worktree(w, src, path, commit, env):
    w.remove(path)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    git(w, "worktree", "prune", cwd=src, env=env)
    git(w, "-c", "advice.detachedHead=false", "worktree", "add", "-q", "--detach", path, commit, cwd=src, env=env)


def drop_tree(w, src, path, env):
    w.remove(path)
    git(w, "worktree", "prune", cwd=src, env=env)


# --- the benchmark ---------------------------------------------------------

class Bench:
    def __init__(self, w, args):
        self.w, self.a = w, args
        self.env = base_env(w, args)
        self.results = w.p("results", "runs.jsonl")
        # Every recorded attempt of this bench id, and whether it succeeded
        # (a resumed run does not repeat one; its tree is reused only if it
        # succeeded).
        self.ok = {(r["project"], r["case"], r["round"], r["side"]): r.get("exit") == 0
                   for r in read_lines(self.results) if r.get("bench_id") == args.bench_id}
        self.done = set(self.ok)
        self.npm = shutil.which("npm", path=self.env["PATH"])
        self.yarn = shutil.which("yarn", path=self.env["PATH"])

    def log(self, *parts):
        return self.w.p("logs", "-".join(str(x) for x in parts) + ".log")

    # One timed install. Load average before and after, wall and CPU time
    # of the command and everything it waited for.
    def timed(self, project, case, rnd, side, order, cmd, cwd, env, disk_of=None):
        key = (project, case, rnd, side)
        if key in self.done:
            say(f"  {case} r{rnd} {side}: done before (resumed)")
            return None
        if self.a.settle:
            time.sleep(self.a.settle)
        waited = 0.0
        # Up to --load-wait seconds for the load to fall and Spotlight to
        # go quiet; then the run starts anyway and is flagged.
        while waited < self.a.load_wait and (os.getloadavg()[0] > self.a.load_max or noise()["spotlight_pcpu"] >= 25):
            time.sleep(5)
            waited += 5
        log = self.log(project, case, f"r{rnd}", side)
        mplog = self.w.p("mp", "log", "log.jsonl")
        mp_before = os.path.getsize(mplog) if os.path.exists(mplog) else 0
        busy = noise()
        load0 = os.getloadavg()
        r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
        t0 = time.monotonic()
        rc, _ = run(cmd, cwd, env, log=log, timeout=self.a.timeout, check=False)
        t1 = time.monotonic()
        r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
        load1 = os.getloadavg()
        rec = {
            "bench_id": self.a.bench_id, "ts": time.time(), "project": project, "case": case, "round": rnd,
            "side": side, "order": order, "wall_s": round(t1 - t0, 3),
            "cpu_user_s": round(r1.ru_utime - r0.ru_utime, 3), "cpu_sys_s": round(r1.ru_stime - r0.ru_stime, 3),
            "load_start": [round(x, 2) for x in load0], "load_end": [round(x, 2) for x in load1],
            "load_flag": load0[0] > self.a.load_max, "load_waited_s": waited, "exit": rc, "noise": busy,
            "cmd": cmd, "log": os.path.relpath(log, self.w.root),
        }
        if side == "matchapack":
            rec["mp_log"] = self.mp_line(mplog, mp_before)
        if rc == 0 and disk_of and rnd <= self.a.disk_rounds:
            rec["disk"] = disk(*disk_of)
        append_line(self.results, rec)
        self.done.add(key)
        self.ok[key] = rc == 0
        flag = " LOAD" if rec["load_flag"] else ""
        bad = "" if rc == 0 else f" EXIT {rc} (see {rec['log']})"
        clones = ""
        m = rec.get("mp_log") or {}
        if m:
            clones = f" [{m.get('outcome')}, cloned {m.get('cloned')}]"
        spot = f" spotlight {busy['spotlight_pcpu']:.0f}%" if busy["spotlight_pcpu"] >= 25 else ""
        say(f"  {case} r{rnd} {side}: {rec['wall_s']:.2f}s wall, cpu {rec['cpu_user_s']:.1f}+{rec['cpu_sys_s']:.1f}s, "
            f"load {load0[0]:.1f}->{load1[0]:.1f}{flag}{spot}{bad}{clones}")
        return rec

    def mp_line(self, path, start):
        """The run log line matchapack wrote for this run (the last one
        after `start`), without the bulky fields."""
        try:
            with open(path) as f:
                f.seek(start)
                lines = [l for l in f.read().splitlines() if l.strip()]
            r = json.loads(lines[-1])
        except (OSError, ValueError, IndexError):
            return {"outcome": "no log line"}
        for k in ("files_before", "files_after", "env_settings", "replay", "manifests_before", "manifests_after"):
            r.pop(k, None)
        inj, y = r.get("inject") or {}, r.get("yarn1") or {}
        r["cloned"] = y.get("cloned") if y else inj.get("cloned", r.get("cloned"))
        return r

    def sides(self, rnd):
        """Interleaved: plain first in odd rounds, matchapack first in even."""
        return [("plain", 1), ("matchapack", 2)] if rnd % 2 else [("matchapack", 1), ("plain", 2)]

    # --- npm ---------------------------------------------------------------

    def npm_project(self, name, spec):
        w, env = self.w, self.env
        cases = [c for c in NPM_CASES if c in self.a.cases]
        if not cases:
            return
        setup_log = self.log(name, "setup")
        say(f"== {name}: {spec['repo']} @ {spec.get('commit', '(this checkout)')}")
        src = fetch_project(w, name, spec, env, setup_log)
        commit = out_of(["git", "rev-parse", "HEAD"], cwd=src)
        if name == "small":
            # "fixtures/small at <commit of this repository>"
            spec["source_commit"] = out_of(["git", "log", "-1", "--format=%s"], cwd=src).split()[-1]
        runs = w.p("runs", name)
        stores = w.p("stores", name)
        os.makedirs(stores, exist_ok=True)
        warm = os.path.join(stores, "warm")
        snap = os.path.join(stores, f"filled-by-npm-{self.a.fill_npm}")
        info_path = w.p("results", "projects.jsonl")
        known = {(r["project"], r.get("bench_id")): r for r in read_lines(info_path)}

        # Prime npm's cache with a plain install (the only one that may
        # download), and record the project's size from that tree.
        if (name, self.a.bench_id) not in known:
            t = os.path.join(runs, "prime")
            worktree(w, src, t, commit, env)
            say(f"  priming npm's cache: plain npm ci in {t}")
            t0 = time.monotonic()
            run([self.npm, *NPM_PRIME_ARGS], os.path.join(t, spec["dir"]), npm_env(env, warm), log=setup_log,
                timeout=self.a.timeout)
            prime_s = time.monotonic() - t0
            d = disk(os.path.join(t, spec["dir"]), None)
            rec = {"bench_id": self.a.bench_id, "project": name, "kind": "npm", "repo": spec["repo"], "commit": commit,
                   "source_commit": spec.get("source_commit"), "dir": spec["dir"], "about": spec["about"],
                   "lock_entries": lock_entries(os.path.join(t, spec["dir"], "package-lock.json")),
                   "packages": d["packages"], "nm_files": d["files"] + d["empty"], "nm_bytes": d["bytes"], "nm_alloc": d["alloc"],
                   "prime_s": round(prime_s, 1)}
            append_line(info_path, rec)
            known[(name, self.a.bench_id)] = rec
            drop_tree(w, src, t, env)
            say(f"  {rec['packages']} packages, node_modules {rec['nm_alloc'] / MIB:.0f} MiB allocated, {rec['nm_files']} files")
        info = known[(name, self.a.bench_id)]
        self.check_space(f"{name} rounds", 4 * info["nm_alloc"] + 2 * GIB)

        # The warm store: one matchapack install with this npm.
        warm_facts = {"npm": self.a.versions["npm"], "matchapack": self.a.versions["matchapack"], "commit": commit}
        if any(c in cases for c in ("npm-fresh-worktree", "npm-reinstall")) and not store_ready(warm, warm_facts):
            say(f"  filling the warm store with npm {self.a.versions['npm']}")
            self.fill_store(warm, warm_facts, src, commit, os.path.join(runs, "fill-warm"), spec["dir"],
                            lambda store: mp_env(npm_env(env, store)), setup_log)
        # The store another npm filled (its extraction code differs, so
        # each entry gets its one-time comparison under this npm's).
        snap_facts = {"npm": self.a.fill_npm, "matchapack": self.a.versions["matchapack"], "commit": commit}
        if "npm-first-use" in cases and not store_ready(snap, snap_facts):
            alt = self.alt_npm()
            say(f"  filling a store with npm {self.a.fill_npm}")
            self.fill_store(snap, snap_facts, src, commit, os.path.join(runs, "fill-alt"), spec["dir"],
                            lambda store: mp_env(npm_env(env, store, alt)), setup_log)

        for rnd in range(1, self.a.rounds + 1):
            say(f"-- {name} round {rnd}/{self.a.rounds}")
            for case in cases:
                for side, order in self.sides(rnd):
                    tree = os.path.join(runs, f"{case}-r{rnd}-{side}")
                    pdir = os.path.join(tree, spec["dir"])
                    store = warm
                    if (name, case, rnd, side) in self.done:
                        say(f"  {case} r{rnd} {side}: done before (resumed)")
                        continue
                    if case == "npm-empty-store":
                        store = os.path.join(stores, f"empty-r{rnd}-{side}")
                        w.remove(store)
                    elif case == "npm-first-use":
                        store = os.path.join(stores, f"first-use-r{rnd}-{side}")
                        w.remove(store)
                        if side == "matchapack":
                            # A clone of the filled store: every round starts
                            # from the same state (cp -c: copy-on-write).
                            run(["/bin/cp", "-c", "-R", snap, store], w.root, env)
                    if case == "npm-reinstall":
                        # Over the tree this side installed in the fresh
                        # worktree case, if that install succeeded; else a
                        # tree installed now.
                        prev = os.path.join(runs, f"npm-fresh-worktree-r{rnd}-{side}")
                        if self.ok.get((name, "npm-fresh-worktree", rnd, side)) and \
                                os.path.isdir(os.path.join(prev, spec["dir"], "node_modules")):
                            tree, pdir = prev, os.path.join(prev, spec["dir"])
                        else:
                            worktree(w, src, tree, commit, env)
                            cmd0 = [self.npm, *NPM_ARGS] if side == "plain" else [BIN, "npm", *NPM_ARGS]
                            e0 = npm_env(env, warm) if side == "plain" else mp_env(npm_env(env, warm))
                            run(cmd0, pdir, e0, log=self.log(name, case, f"r{rnd}", side, "setup"), timeout=self.a.timeout)
                    else:
                        worktree(w, src, tree, commit, env)
                    cmd = [self.npm, *NPM_ARGS] if side == "plain" else [BIN, "npm", *NPM_ARGS]
                    e = npm_env(env, store) if side == "plain" else mp_env(npm_env(env, store))
                    self.timed(name, case, rnd, side, order, cmd, pdir, e, disk_of=(pdir, store))
                    if case == "npm-empty-store" or case == "npm-first-use":
                        w.remove(store)
                    # The fresh worktree's tree stays for the reinstall case.
                    if not (case == "npm-fresh-worktree" and "npm-reinstall" in cases):
                        drop_tree(w, src, tree, env)
            for side in ("plain", "matchapack"):
                for case in ("npm-fresh-worktree", "npm-reinstall"):
                    t = os.path.join(runs, f"{case}-r{rnd}-{side}")
                    if os.path.lexists(t):
                        drop_tree(w, src, t, env)
        say(f"== {name}: done")

    def fill_store(self, store, facts, src, commit, tree, pdir, env_for, log):
        """Fills `store` with one matchapack install into a fresh worktree:
        into `<store>.partial`, published (renamed, then `<store>.ready`
        written with `facts`) only after the install succeeded, so an
        interrupted or failed fill is never taken for a filled store."""
        w = self.w
        for p in (store + ".ready", store, store + ".partial"):
            w.remove(p)
        worktree(w, src, tree, commit, self.env)
        run([BIN, "npm", *NPM_ARGS], os.path.join(tree, pdir), env_for(store + ".partial"), log=log, timeout=self.a.timeout)
        drop_tree(w, src, tree, self.env)
        os.rename(store + ".partial", store)
        write_atomic(store + ".ready", json.dumps(facts, sort_keys=True) + "\n")

    def alt_npm(self):
        """npm <fill_npm> in a scratch prefix, from the registry, with the
        scratch cache and npmrc. Returns the directory to put first on PATH."""
        prefix = self.w.p("npm-alt", self.a.fill_npm)
        bindir = os.path.join(prefix, "node_modules", ".bin")
        if not os.path.exists(os.path.join(bindir, "npm")):
            os.makedirs(prefix, exist_ok=True)
            run([self.npm, "install", "--prefix", prefix, "--no-audit", "--no-fund", f"npm@{self.a.fill_npm}"],
                prefix, self.env, log=self.log("npm-alt"), timeout=self.a.timeout)
        v = out_of(["npm", "-v"], env=dict(self.env, PATH=bindir + os.pathsep + self.env["PATH"]))
        if v != self.a.fill_npm:
            die(f"npm in {bindir} is {v}, not {self.a.fill_npm}")
        return bindir

    # --- Yarn 1 --------------------------------------------------------------

    def yarn_project(self, name, spec):
        w, env = self.w, self.env
        cases = [c for c in YARN_CASES if c in self.a.cases]
        if not cases:
            return
        setup_log = self.log(name, "setup")
        say(f"== {name}: {spec['repo']} @ {spec['commit']} (Yarn runs use --ignore-scripts)")
        src = fetch_project(w, name, spec, env, setup_log)
        commit = out_of(["git", "rev-parse", "HEAD"], cwd=src)
        parent = out_of(["git", "rev-parse", "HEAD^"], cwd=src)
        changed = out_of(["git", "diff", "--name-only", parent, commit], cwd=src).split()
        if not set(changed) <= {"package.json", "yarn.lock", "bun.lock"}:
            die(f"{name}: {parent}..{commit} changes more than the root manifests: {changed}")
        runs = w.p("runs", name)
        warm = w.p("yarn-cache")
        info_path = w.p("results", "projects.jsonl")
        known = {(r["project"], r.get("bench_id")): r for r in read_lines(info_path)}
        yargs = [self.yarn, *YARN_PRIME_ARGS, "--cache-folder"]

        def ycmd(side, cache):
            return ([self.yarn] if side == "plain" else [BIN, "yarn"]) + YARN_ARGS + ["--cache-folder", cache]

        def yenv(side, cache):
            e = yarn_env(env, w, cache)
            return e if side == "plain" else mp_env(e)

        def at_parent(tree):
            # The parent commit's root manifests (the state before the bump).
            for f in ("package.json", "yarn.lock"):
                data = subprocess.run(["git", "show", f"{parent}:{f}"], cwd=src, capture_output=True, check=True).stdout
                with open(os.path.join(tree, f), "wb") as fh:
                    fh.write(data)

        # Prime the offline mirror and the warm cache: plain Yarn at the
        # parent commit, then at the commit (the bump's packages too).
        if (name, self.a.bench_id) not in known:
            t = os.path.join(runs, "prime")
            worktree(w, src, t, commit, env)
            at_parent(t)
            say("  priming Yarn's offline mirror and the warm cache (plain Yarn, the parent commit, then the commit)")
            os.makedirs(w.p("yarn-mirror"), exist_ok=True)
            t0 = time.monotonic()
            run(yargs + [warm], t, yarn_env(env, w, warm), log=setup_log, timeout=self.a.timeout)
            git(w, "checkout", "--", "package.json", "yarn.lock", cwd=t, env=env)
            run(yargs + [warm], t, yarn_env(env, w, warm), log=setup_log, timeout=self.a.timeout)
            prime_s = time.monotonic() - t0
            d = disk(t, None)
            rec = {"bench_id": self.a.bench_id, "project": name, "kind": "yarn", "repo": spec["repo"], "commit": commit,
                   "parent": parent, "dir": spec["dir"], "about": spec["about"], "yarn_args": YARN_ARGS,
                   "yarn_prime_args": YARN_PRIME_ARGS,
                   "packages": d["packages"], "nm_files": d["files"] + d["empty"], "nm_bytes": d["bytes"], "nm_alloc": d["alloc"],
                   "mirror_files": len(os.listdir(w.p("yarn-mirror"))), "prime_s": round(prime_s, 1)}
            append_line(info_path, rec)
            known[(name, self.a.bench_id)] = rec
            drop_tree(w, src, t, env)
            say(f"  {rec['packages']} packages, node_modules {rec['nm_alloc'] / MIB:.0f} MiB allocated, {rec['nm_files']} files")
        info = known[(name, self.a.bench_id)]
        self.check_space(f"{name} rounds", 5 * info["nm_alloc"] + 2 * GIB)

        for rnd in range(1, self.a.rounds + 1):
            say(f"-- {name} round {rnd}/{self.a.rounds}")
            for case in cases:
                for side, order in self.sides(rnd):
                    if (name, case, rnd, side) in self.done:
                        say(f"  {case} r{rnd} {side}: done before (resumed)")
                        continue
                    tree = os.path.join(runs, f"{case}-r{rnd}-{side}")
                    cache = warm
                    if case == "yarn-empty-cache":
                        cache = w.p("yarn-cache-empty", f"r{rnd}-{side}")
                        w.remove(cache)
                        worktree(w, src, tree, commit, env)
                    elif case == "yarn-installed":
                        # The fresh worktree's tree, if that install
                        # succeeded (a timed-out one leaves a partial tree);
                        # else a tree installed now.
                        prev = os.path.join(runs, f"yarn-fresh-worktree-r{rnd}-{side}")
                        if self.ok.get((name, "yarn-fresh-worktree", rnd, side)) and os.path.isdir(os.path.join(prev, "node_modules")):
                            tree = prev
                        else:
                            worktree(w, src, tree, commit, env)
                            run(ycmd(side, cache), tree, yenv(side, cache), log=self.log(name, case, f"r{rnd}", side, "setup"),
                                timeout=self.a.timeout)
                    elif case == "yarn-incremental":
                        worktree(w, src, tree, commit, env)
                        at_parent(tree)
                        run(ycmd(side, cache), tree, yenv(side, cache), log=self.log(name, case, f"r{rnd}", side, "setup"),
                            timeout=self.a.timeout)
                        git(w, "checkout", "--", "package.json", "yarn.lock", cwd=tree, env=env)
                    else:
                        worktree(w, src, tree, commit, env)
                    self.timed(name, case, rnd, side, order, ycmd(side, cache), tree, yenv(side, cache),
                               disk_of=(tree, cache) if case != "yarn-installed" else None)
                    if case == "yarn-empty-cache":
                        w.remove(cache)
                    if case != "yarn-fresh-worktree" or "yarn-installed" not in cases:
                        drop_tree(w, src, tree, env)
            for side in ("plain", "matchapack"):
                for case in ("yarn-fresh-worktree", "yarn-installed"):
                    t = os.path.join(runs, f"{case}-r{rnd}-{side}")
                    if os.path.lexists(t):
                        drop_tree(w, src, t, env)
        say(f"== {name}: done")

    def check_space(self, what, need):
        free = free_bytes(self.w.root)
        if free < need + self.a.margin_gib * GIB:
            die(f"{what}: {free / GIB:.1f} GiB free, needs about {need / GIB:.1f} GiB plus a margin of {self.a.margin_gib} GiB")


def lock_entries(path):
    try:
        d = json.load(open(path))
        return sum(1 for k in d.get("packages", {}) if "node_modules/" in k)
    except (OSError, ValueError):
        return None


# --- the summary -----------------------------------------------------------

def fmt_s(xs):
    if not xs:
        return "-"
    return f"{statistics.median(xs):.2f} s ({min(xs):.2f}–{max(xs):.2f})" if len(xs) > 1 else f"{xs[0]:.2f} s"


def human(b):
    return f"{b / GIB:.2f} GiB" if b >= GIB else f"{b / MIB:.0f} MiB" if b >= MIB else f"{b / 1024:.0f} KiB"


def summary(w, bench_id, fill_npm, smoke):
    runs = [r for r in read_lines(w.p("results", "runs.jsonl")) if r.get("bench_id") == bench_id]
    projects = {r["project"]: r for r in read_lines(w.p("results", "projects.jsonl")) if r.get("bench_id") == bench_id}
    metas = [r for r in read_lines(w.p("results", "meta.jsonl")) if r.get("bench_id") == bench_id]
    m = next((r["machine"] for r in metas if r.get("machine")), {})
    a = next((r["args"] for r in metas if r.get("args")), {})
    # What was measured is what the benchmark was started with, not this
    # command's options.
    fill_npm = a.get("fill_npm", fill_npm)
    smoke = a.get("smoke", smoke)

    def timed_args(kind):
        # The arguments of the plain side's first timed run, as recorded.
        r = next((r for r in runs if r["side"] == "plain" and r["case"].startswith(kind + "-")), None)
        if not r:
            return "(no run)"
        cmd = r["cmd"][1:]
        if "--cache-folder" in cmd:
            cmd = cmd[:cmd.index("--cache-folder")]
        return " ".join(cmd)

    L = []
    title = "matchapack benchmark" + (" — SMOKE TEST, not results" if smoke else "")
    L += [f"# {title}", ""]
    L += [f"Bench `{bench_id}`. {m.get('model', '?')}, {m.get('cpu', '?')}, {m.get('cores', '?')} cores, {m.get('ram_gib', '?')} GiB RAM, "
          f"macOS {m.get('macos', '?')}, {m.get('filesystem', '?')}. Node {m.get('node', '?')}, npm {m.get('npm', '?')}, "
          f"Yarn {m.get('yarn', '?')}, {m.get('matchapack', '?')}; bench script at {m.get('bench_commit', '?')[:10]}"
          f"{' (uncommitted changes)' if m.get('bench_dirty') else ''}.", ""]
    L += ["Method: each case runs in interleaved rounds (plain first in odd rounds, matchapack first in even ones), "
          f"{a.get('rounds', '?')} rounds, {a.get('settle', '?')} s settle time before each run; times are medians with min–max. "
          "Wall time and CPU time (user + sys) of the install command and every process it waited for. "
          "npm's cache is warm for both sides in every case (primed by a plain install first, the only install that may download; "
          f"network is not measured); timed npm runs: `npm {timed_args('npm')}`. Yarn 1 runs use `--ignore-scripts` (the project's "
          "install scripts fail in plain Yarn too), and Yarn's tarballs come from an offline mirror primed first, so an empty Yarn "
          f"cache means extracting, not downloading; timed Yarn runs: `yarn {timed_args('yarn')}`. "
          f"A run that started with a 1-minute load average above {a.get('load_max', '?')} is counted in the LOAD column, not dropped. "
          "Disk, a first-extent estimate: a node_modules file counts as shared when its first extent is the first extent of a file in "
          "the store or Yarn's cache (as `scripts/shared-blocks.py`); new bytes are the allocation of the files that share no first "
          "extent (a shared file whose later blocks were rewritten is not counted there); metadata (inodes, directories) not counted.", ""]
    L += ["## Projects", "", "| Project | Repository @ commit | Packages installed | node_modules (files, size) |", "|---|---|---|---|"]
    for name, p in projects.items():
        c = p.get("source_commit") or p.get("commit", "")
        L.append(f"| {name} ({p['kind']}) | {p['repo']} @ `{c[:12]}`{'' if p['dir'] == '.' else ' `' + p['dir'] + '`'} | "
                 f"{p['packages']:,} | {p['nm_files']:,} files, {human(p['nm_alloc'])} |")
    L.append("")
    L += ["## Time", "", "| Project | Case | Plain wall | matchapack wall | Ratio (medians) | Plain CPU | matchapack CPU | Rounds | LOAD | Failed |",
          "|---|---|---|---|---|---|---|---|---|---|"]
    order = NPM_CASES + YARN_CASES
    keys = sorted({(r["project"], r["case"]) for r in runs}, key=lambda k: (list(projects).index(k[0]) if k[0] in projects else 99, order.index(k[1])))
    for pname, case in keys:
        rs = [r for r in runs if r["project"] == pname and r["case"] == case]
        ok = {s: [r for r in rs if r["side"] == s and r["exit"] == 0] for s in ("plain", "matchapack")}
        wall = {s: [r["wall_s"] for r in ok[s]] for s in ok}
        cpu = {s: [r["cpu_user_s"] + r["cpu_sys_s"] for r in ok[s]] for s in ok}
        ratio = f"{statistics.median(wall['matchapack']) / statistics.median(wall['plain']):.2f}×" if wall["plain"] and wall["matchapack"] else "-"
        rounds = len({r["round"] for r in rs})
        flagged = sum(1 for r in rs if r.get("load_flag"))
        failed = sum(1 for r in rs if r["exit"] != 0)
        L.append(f"| {pname} | {CASE_TEXT[case].format(fill_npm=fill_npm)} | {fmt_s(wall['plain'])} | {fmt_s(wall['matchapack'])} | {ratio} | "
                 f"{fmt_s(cpu['plain'])} | {fmt_s(cpu['matchapack'])} | {rounds} | {flagged} of {len(rs)} | {failed} |")
    L.append("")
    L += ["## Disk per extra worktree", "",
          "From the first round(s) of each case. New bytes: the allocation of node_modules files that share no first extent with the "
          "store or cache (a first-extent estimate). "
          "Store / cache: what matchapack's store (npm) or Yarn's cache holds after the run, allocated, paid once per machine.", "",
          "| Project | Case | Side | Files sharing a first extent | Their bytes | New bytes | Store / cache |", "|---|---|---|---|---|---|---|"]
    for pname, case in keys:
        for side in ("plain", "matchapack"):
            ds = [r["disk"] for r in runs if r["project"] == pname and r["case"] == case and r["side"] == side and r.get("disk")]
            if not ds:
                continue
            d = ds[0]
            pct = 100.0 * d["shared_bytes"] / d["bytes"] if d["bytes"] else 0.0
            L.append(f"| {pname} | {case} | {side} | {d['shared_files']:,} of {d['files']:,} | {pct:.1f} % | {human(d['new_alloc'])} | "
                     f"{human(d.get('source_alloc', 0))} |")
    L.append("")
    L += ["## Headline (fresh worktree, warm store or cache)", ""]
    for pname in projects:
        case = "npm-fresh-worktree" if projects[pname]["kind"] == "npm" else "yarn-fresh-worktree"
        dp = [r["disk"] for r in runs if r["project"] == pname and r["case"] == case and r["side"] == "plain" and r.get("disk")]
        dm = [r["disk"] for r in runs if r["project"] == pname and r["case"] == case and r["side"] == "matchapack" and r.get("disk")]
        if dp and dm:
            p, q = dp[0]["new_alloc"], dm[0]["new_alloc"]
            src = dm[0].get("source_alloc", 0)
            n = p / q if q else float("inf")
            L.append(f"- **{pname}**: each extra worktree's node_modules allocates {human(q)} with matchapack, against {human(p)} plain, "
                     f"in files that share no first extent with the {'store' if projects[pname]['kind'] == 'npm' else 'Yarn cache'} "
                     f"(about {n:.0f} worktrees for the disk of one plain copy). 10 worktrees: plain {human(10 * p)}; "
                     f"matchapack {human(10 * q)} plus the {'store' if projects[pname]['kind'] == 'npm' else 'Yarn cache'} once ({human(src)}).")
    L.append("")
    L += ["## Machine noise", "",
          "Per run, at its start (`ps` %CPU, a recent average): Spotlight's indexers (mds, mds_stores, mdworker, ...) and a virtual "
          "machine (com.apple.Virtualization.VirtualMachine); every run's top five are in runs.jsonl (`noise`).", "",
          "| Project | Case | Runs | Load above the limit | Spotlight ≥ 25 % | Spotlight %, median (max) | VM %, median (max) |",
          "|---|---|---|---|---|---|---|"]
    for pname, case in keys:
        rs = [r for r in runs if r["project"] == pname and r["case"] == case]
        sp = [r.get("noise", {}).get("spotlight_pcpu", 0.0) for r in rs]
        vm = [r.get("noise", {}).get("vm_pcpu", 0.0) for r in rs]
        if rs:
            L.append(f"| {pname} | {case} | {len(rs)} | {sum(1 for r in rs if r.get('load_flag'))} | {sum(1 for x in sp if x >= 25)} | "
                     f"{statistics.median(sp):.0f} ({max(sp):.0f}) | {statistics.median(vm):.0f} ({max(vm):.0f}) |")
    L += ["", "Top CPU users at the start of the benchmark (`ps -Ao pcpu,comm -r`; nothing was stopped):", "", "```"]
    L += next((r.get("top_cpu", []) for r in metas if r.get("machine")), []) + ["```", ""]
    sent = [r for r in read_lines(w.p("results", "sentinels.jsonl")) if r.get("bench_id") == bench_id]
    if sent:
        L += ["## Isolation check", "", isolation_text(sent), ""]
    return "\n".join(L) + "\n"


def isolation_text(sent):
    """The isolation check of the latest invocation: its start record and
    the end record after it. No end record: the check did not complete."""
    starts = [i for i, r in enumerate(sent) if r.get("when") == "start"]
    if not starts:
        return "The isolation check did not complete (no start record)."
    end = next((r for r in sent[starts[-1] + 1:] if r.get("when") == "end"), None)
    if end is None:
        return ("The isolation check did not complete: the last invocation stopped before its end check, "
                "so whether the user's real matchapack state and npm / Yarn caches changed is not known.")
    if "changed" not in end:
        return "The isolation check did not complete (the end record has no result)."
    state = end.get("state") or {}
    how = ("entries compared by path, kind, size, mtime and inode, " +
           ", ".join(f"{k} {v['depth']} levels deep" for k, v in state.items() if isinstance(v, dict) and "depth" in v)
           if any(isinstance(v, dict) and "depth" in v for v in state.values()) else "the newest mtime of each, a few levels deep")
    if not end["changed"]:
        return f"The user's real matchapack state and npm / Yarn caches: unchanged ({how})."
    return f"The user's real matchapack state and npm / Yarn caches: CHANGED ({how}): " + "; ".join(end["changed"])


# --- main ------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="matchapack benchmark (issue grimmerk/matchapack-private#7); run through scripts/bench.sh")
    ap.add_argument("--work", required=True,
                    help=f"work directory: new or empty (then marked with {MARKER}), or marked before; never under ~/git")
    ap.add_argument("--projects", default="small,medium,large,yarn", help="comma list of " + ",".join(PROJECTS))
    ap.add_argument("--cases", default=",".join(NPM_CASES + YARN_CASES), help="comma list of cases")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--disk-rounds", type=int, default=1, help="rounds whose trees are measured for disk (default 1)")
    ap.add_argument("--load-max", type=float, default=3.0, help="1-minute load average above which a run is flagged")
    ap.add_argument("--load-wait", type=float, default=60, help="seconds to wait for the load to fall below --load-max before a run (then it runs anyway)")
    ap.add_argument("--settle", type=float, default=10, help="seconds to wait before each timed run")
    ap.add_argument("--timeout", type=float, default=1800, help="seconds per install before it is killed")
    # 11.10.1: another extraction code than 11.19 (pacote 21.3.1, its own
    # fingerprint), and an npm 11 accepts the large project's lockfile
    # (npm 10.9.10 refuses it as out of sync).
    ap.add_argument("--fill-npm", default="11.10.1", help="the npm that fills the store of the first-use case")
    ap.add_argument("--margin-gib", type=float, default=20, help="free space to keep beyond the estimate")
    ap.add_argument("--bench-id", default=None, help="name of this benchmark; the same name resumes it (default: a new one)")
    ap.add_argument("--smoke", action="store_true", help="mark the summary as a smoke test")
    ap.add_argument("--summary-only", action="store_true", help="rewrite results/summary.md and stop")
    ap.add_argument("--cleanup", action="store_true", help="remove everything but logs/ and results/, then stop")
    a = ap.parse_args()

    # Everything about --work is checked before anything is created.
    root = os.path.abspath(os.path.expanduser(a.work))
    real = os.path.realpath(root)
    gitdir = os.path.realpath(os.path.expanduser("~/git"))
    here = os.path.realpath(HERE)
    for r in (root, real):
        if r == gitdir or r.startswith(gitdir + os.sep):
            die(f"--work {root} is under ~/git: choose another directory")
        if r == here or r.startswith(here + os.sep):
            die("--work is inside this checkout")
    st = work_state(root)
    if st == "not-dir":
        die(f"--work {root} is not a directory")
    if st == "foreign":
        die(f"--work {root} is not empty and has no {MARKER}: the benchmark starts only in a new or empty directory, "
            f"or in one it marked. If it is a work directory of an older bench.py, mark it by hand: touch {os.path.join(root, MARKER)}")
    if (a.summary_only or a.cleanup) and st != "owned":
        die(f"--work {root} is not a benchmark directory (no {MARKER})")
    gid = os.stat(nearest_existing(root)).st_gid
    if gid not in os.getgroups():
        # macOS gives a new file its directory's group: npm's extraction
        # then has a group matchapack cannot give its clones (/tmp is
        # wheel), and the numbers would not be a user's.
        die(f"{nearest_existing(root)} has group {gid}, not one of yours: use a directory in your home")
    if st in ("missing", "empty"):
        os.makedirs(root, exist_ok=True)
        write_atomic(os.path.join(root, MARKER), "matchapack benchmark work directory (scripts/bench.py); --cleanup may remove its contents\n")
    root = os.path.realpath(root)
    w = Work(root)
    for d in ("logs", "results", "env", "mp/config", "mp/log", "src", "runs", "stores"):
        os.makedirs(w.p(d), exist_ok=True)
    lockf = open(w.p("bench.lock"), "w")
    try:
        fcntl.flock(lockf, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        die(f"another bench runs in {root}")

    state = w.p("results", "current-bench-id")
    if a.bench_id is None:
        a.bench_id = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    if a.summary_only or a.cleanup:
        bid = a.bench_id if "--bench-id" in " ".join(sys.argv) else (open(state).read().strip() if os.path.exists(state) else a.bench_id)
        if a.cleanup:
            for d in ("src", "runs", "stores", "npm-cache", "npm-alt", "yarn-mirror", "yarn-cache", "yarn-cache-empty", "mp", "env"):
                say(f"removing {w.p(d)}")
                w.remove(w.p(d))
            say("kept: logs/ and results/")
            return
        write_atomic(w.p("results", "summary.md"), summary(w, bid, a.fill_npm, a.smoke))
        say(f"wrote {w.p('results', 'summary.md')}")
        return
    write_atomic(state, a.bench_id + "\n")

    if not os.access(BIN, os.X_OK):
        die(f"build first: cargo build --release ({BIN})")
    a.projects = [x for x in a.projects.split(",") if x]
    a.cases = [x for x in a.cases.split(",") if x]
    for x in a.projects:
        if x not in PROJECTS:
            die(f"unknown project {x}")
    for x in a.cases:
        if x not in NPM_CASES + YARN_CASES:
            die(f"unknown case {x}")
    for f in ("npmrc", "global-npmrc", "yarnrc"):
        open(w.p("env", f), "a").close()

    OUT["file"] = open(w.p("logs", "bench.out"), "a")
    signal.signal(signal.SIGTERM, on_signal)
    # Under nohup a hang-up stays ignored.
    if signal.getsignal(signal.SIGHUP) is not signal.SIG_IGN:
        signal.signal(signal.SIGHUP, on_signal)
    signal.signal(signal.SIGPIPE, signal.SIG_IGN)
    b = Bench(w, a)
    if not b.npm or not b.yarn:
        die("npm and yarn must be on PATH")
    m = machine(root, b.env)
    a.versions = m
    if not m["filesystem"].upper().startswith("APFS"):
        die(f"{root} is on {m['filesystem'] or 'an unknown filesystem'}, not APFS")
    if not m["yarn"].startswith("1."):
        die(f"yarn on PATH is {m['yarn']}, not Yarn 1")
    if not root.endswith(".noindex"):
        say("note: a work directory whose name ends in .noindex is not indexed by Spotlight (less noise)")
    first = next((r for r in read_lines(w.p("results", "meta.jsonl")) if r.get("bench_id") == a.bench_id and r.get("machine")), None)
    if first:
        diff = resume_conflicts(first, m, a)
        if diff:
            die(f"bench {a.bench_id} was started with other settings or tools; resuming would mix them: "
                + "; ".join(diff) + ". Start a new benchmark with another --bench-id")
        say(f"resuming bench {a.bench_id} (same settings and tools)")
    started = time.time()
    s0 = sentinels()
    top = top_cpu()
    say(f"bench {a.bench_id} in {root}")
    say(f"{m['model']}, {m['cpu']}, {m['ram_gib']} GiB, macOS {m['macos']}, {m['filesystem']}; node {m['node']}, npm {m['npm']}, yarn {m['yarn']}, {m['matchapack']}")
    say("top CPU users now (recorded; nothing is stopped):")
    for line in top:
        say("    " + line)
    est = sum(PROJECTS[x]["est_gib"] for x in a.projects) * GIB
    free = free_bytes(root)
    say(f"free space {free / GIB:.1f} GiB; this run needs about {est / GIB:.1f} GiB at its peak, plus a margin of {a.margin_gib} GiB")
    if free < est + a.margin_gib * GIB:
        die("not enough free space")
    append_line(w.p("results", "meta.jsonl"), {"bench_id": a.bench_id, "ts": time.time(), "machine": m, "top_cpu": top,
                                               "args": {k: v for k, v in vars(a).items() if k != "versions"},
                                               "node_options_dropped": "NODE_OPTIONS" in os.environ,
                                               "env_npmrc": "NPM_CONFIG_USERCONFIG and NPM_CONFIG_GLOBALCONFIG are empty files in env/"})
    append_line(w.p("results", "sentinels.jsonl"), {"bench_id": a.bench_id, "when": "start", "ts": time.time(), "state": s0})

    t0 = time.monotonic()
    stopped_code = run_projects(b, w, a, t0)
    code = finish(w, a, s0, started, t0, stopped_code)
    if code:
        sys.exit(code)


def run_projects(b, w, a, t0):
    """Runs every project. Never raises for a stop: returns None when every
    project ran, 130 when a signal stopped it (Ctrl-C, SIGTERM, SIGHUP:
    on_signal), or a setup error's status (die). The runs recorded so far
    are kept either way."""
    try:
        for x in a.projects:
            spec = dict(PROJECTS[x])
            if spec["kind"] == "npm":
                b.npm_project(x, spec)
            else:
                b.yarn_project(x, spec)
            append_line(w.p("results", "meta.jsonl"), {"bench_id": a.bench_id, "ts": time.time(), "after": x,
                                                       "elapsed_s": round(time.monotonic() - t0), "top_cpu": top_cpu(),
                                                       "free_gib": round(free_bytes(w.root) / GIB, 1)})
    except KeyboardInterrupt as e:
        say(f"stopped ({str(e) or 'interrupted'}); runs recorded so far are kept; the same --bench-id {a.bench_id} resumes")
        return 130
    except SystemExit as e:
        # A setup step failed (die): the isolation check and the summary
        # still run (finish), then the run ends with that status.
        code = e.code if isinstance(e.code, int) and e.code else 1
        say(f"stopped by an error (exit {code}); runs recorded so far are kept; the same --bench-id {a.bench_id} resumes")
        return code
    return None


def finish(w, a, s0, started, t0, stopped_code, state=None):
    """The end of every run, however it stopped: the isolation check's end
    record, the summary, and the exit status (4: the user's real state
    changed; else `stopped_code`; else 1 when a recorded timed run of this
    bench id failed; else 0). `state` replaces sentinels() in tests."""
    s1 = (state or sentinels)()
    changed = sentinel_changes(s0, s1, started)
    append_line(w.p("results", "sentinels.jsonl"), {"bench_id": a.bench_id, "when": "end", "ts": time.time(), "state": s1, "changed": changed})
    write_atomic(w.p("results", "summary.md"), summary(w, a.bench_id, a.fill_npm, a.smoke))
    say(f"elapsed {(time.monotonic() - t0) / 3600:.2f} h; results in {w.p('results')} (summary.md, runs.jsonl)")
    if changed:
        say("!!! ISOLATION CHECK FAILED: the user's real state changed during the run (this run or another process):")
        for c in changed:
            say("!!!   " + c)
        return 4
    if stopped_code is not None:
        return stopped_code
    # Every recorded attempt of this bench id, those of earlier invocations
    # (a resumed run does not repeat them) included.
    failed = recorded_failures(read_lines(w.p("results", "runs.jsonl")), a.bench_id)
    if failed:
        say(f"{failed} timed runs of bench {a.bench_id} failed (exit status in runs.jsonl, logs in logs/)")
        return 1
    return 0


def recorded_failures(records, bench_id):
    """Timed attempts of `bench_id` whose recorded exit status is not 0."""
    return sum(1 for r in records if r.get("bench_id") == bench_id and r.get("exit") != 0)


if __name__ == "__main__":
    main()
