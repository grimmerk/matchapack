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
--cleanup removes everything but logs/ and results/.
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
NPM_ARGS = ["ci", "--no-audit", "--no-fund", "--prefer-offline"]
YARN_ARGS = ["install", "--frozen-lockfile", "--ignore-scripts", "--non-interactive"]

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


def append_line(path, rec):
    """One complete JSON line, flushed to disk: a run is recorded whole or
    not at all (a torn last line is dropped when the file is read)."""
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


def kill_group(p):
    for sig, wait in ((signal.SIGTERM, 30), (signal.SIGKILL, 10)):
        try:
            os.killpg(p.pid, sig)
        except ProcessLookupError:
            return
        try:
            p.wait(timeout=wait)
            return
        except subprocess.TimeoutExpired:
            pass


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

def sentinels():
    """Newest mtime (files and directories, a few levels deep) of the
    user's real matchapack state and npm / Yarn caches. The run must not
    change them; another process on the machine can, which the report
    then says."""
    home = os.path.expanduser("~")
    paths = {
        "~/.cache/matchapack": (os.path.join(home, ".cache/matchapack"), 3),
        "~/.config/matchapack": (os.path.join(home, ".config/matchapack"), 3),
        "~/.npm": (os.path.join(home, ".npm"), 4),
        "~/Library/Caches/Yarn": (os.path.join(home, "Library/Caches/Yarn"), 2),
    }
    out = {}
    for name, (root, depth) in paths.items():
        if not os.path.lexists(root):
            out[name] = None
            continue
        newest, newest_path = os.lstat(root).st_mtime, root
        base = root.count(os.sep)
        for d, dirs, names in os.walk(root):
            if d.count(os.sep) - base >= depth:
                dirs[:] = []
            for x in dirs + names:
                p = os.path.join(d, x)
                try:
                    m = os.lstat(p).st_mtime
                except OSError:
                    continue
                if m > newest:
                    newest, newest_path = m, p
        out[name] = {"mtime": newest, "path": newest_path}
    return out


def sentinel_changes(a, b, since):
    """What changed, with up to 10 of the entries written since `since`
    (another process may have written them: the times tell)."""
    out = []
    for k in a:
        if a[k] == b.get(k):
            continue
        if not b.get(k):
            out.append(f"{k}: gone")
            continue
        root = os.path.expanduser(k)
        newer = []
        for d, dirs, names in os.walk(root):
            for x in dirs + names:
                p = os.path.join(d, x)
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
        out.append(f"{k}: {len(newer)}{'+' if len(newer) > 200 else ''} entries written since the start: {listed}")
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
        "yarn": out_of(["yarn", "--version"], env=env),
        "matchapack": out_of([BIN, "--version"], env=env),
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
        # Yarn 1: no ~/.yarnrc, no self-update check (it would write
        # ~/.yarnrc); its cache and mirror are set per run.
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
    Yarn project's parent commit is there)."""
    src = w.p("src", name)
    if os.path.isdir(os.path.join(src, ".git")):
        return src
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
        self.done = {(r["project"], r["case"], r["round"], r["side"]) for r in read_lines(self.results)
                     if r.get("bench_id") == args.bench_id}
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
        flag = " LOAD" if rec["load_flag"] else ""
        bad = "" if rc == 0 else f" EXIT {rc} (see {rec['log']})"
        clones = ""
        m = rec.get("mp_log") or {}
        if m:
            clones = f" [{m.get('outcome')}, cloned {m.get('cloned')}]"
        spot = f" spotlight {busy['spotlight_pcpu']:.0f}%" if busy["spotlight_pcpu"] >= 25 else ""
        say(f"  {case} r{rnd} {side}: {rec['wall_s']:.2f}s wall, cpu {rec['cpu_user_s']:.1f}+{rec['cpu_sys_s']:.1f}s, "
            f"load {load0[0]:.1f}->{load1[0]:.1f}{flag}{spot}{bad}{clones}")
        if rc != 0:
            self.failures += 1
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

    failures = 0

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
            run([self.npm, *NPM_ARGS], os.path.join(t, spec["dir"]), npm_env(env, warm), log=setup_log)
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
        if any(c in cases for c in ("npm-fresh-worktree", "npm-reinstall")) and not os.path.isdir(os.path.join(warm, "v2")):
            t = os.path.join(runs, "fill-warm")
            worktree(w, src, t, commit, env)
            say(f"  filling the warm store with npm {self.a.versions['npm']}")
            run([BIN, "npm", *NPM_ARGS], os.path.join(t, spec["dir"]), mp_env(npm_env(env, warm)), log=setup_log)
            drop_tree(w, src, t, env)
        # The store another npm filled (its extraction code differs, so
        # each entry gets its one-time comparison under this npm's).
        if "npm-first-use" in cases and not os.path.isdir(os.path.join(snap, "v2")):
            alt = self.alt_npm()
            t = os.path.join(runs, "fill-alt")
            worktree(w, src, t, commit, env)
            say(f"  filling a store with npm {self.a.fill_npm}")
            w.remove(snap + ".partial")
            run([BIN, "npm", *NPM_ARGS], os.path.join(t, spec["dir"]),
                mp_env(npm_env(env, snap + ".partial", alt)), log=setup_log)
            drop_tree(w, src, t, env)
            os.rename(snap + ".partial", snap)

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
                        # worktree case, else a tree installed now.
                        prev = os.path.join(runs, f"npm-fresh-worktree-r{rnd}-{side}")
                        if os.path.isdir(os.path.join(prev, spec["dir"], "node_modules")):
                            tree, pdir = prev, os.path.join(prev, spec["dir"])
                        else:
                            worktree(w, src, tree, commit, env)
                            cmd0 = [self.npm, *NPM_ARGS] if side == "plain" else [BIN, "npm", *NPM_ARGS]
                            e0 = npm_env(env, warm) if side == "plain" else mp_env(npm_env(env, warm))
                            run(cmd0, pdir, e0, log=self.log(name, case, f"r{rnd}", side, "setup"))
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

    def alt_npm(self):
        """npm <fill_npm> in a scratch prefix, from the registry, with the
        scratch cache and npmrc. Returns the directory to put first on PATH."""
        prefix = self.w.p("npm-alt", self.a.fill_npm)
        bindir = os.path.join(prefix, "node_modules", ".bin")
        if not os.path.exists(os.path.join(bindir, "npm")):
            os.makedirs(prefix, exist_ok=True)
            run([self.npm, "install", "--prefix", prefix, "--no-audit", "--no-fund", f"npm@{self.a.fill_npm}"],
                prefix, self.env, log=self.log("npm-alt"))
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
        yargs = [self.yarn, *YARN_ARGS, "--cache-folder"]

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
            run(yargs + [warm], t, yarn_env(env, w, warm), log=setup_log)
            git(w, "checkout", "--", "package.json", "yarn.lock", cwd=t, env=env)
            run(yargs + [warm], t, yarn_env(env, w, warm), log=setup_log)
            prime_s = time.monotonic() - t0
            d = disk(t, None)
            rec = {"bench_id": self.a.bench_id, "project": name, "kind": "yarn", "repo": spec["repo"], "commit": commit,
                   "parent": parent, "dir": spec["dir"], "about": spec["about"], "yarn_args": YARN_ARGS,
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
                        prev = os.path.join(runs, f"yarn-fresh-worktree-r{rnd}-{side}")
                        if os.path.isdir(os.path.join(prev, "node_modules")):
                            tree = prev
                        else:
                            worktree(w, src, tree, commit, env)
                            run(ycmd(side, cache), tree, yenv(side, cache), log=self.log(name, case, f"r{rnd}", side, "setup"))
                    elif case == "yarn-incremental":
                        worktree(w, src, tree, commit, env)
                        at_parent(tree)
                        run(ycmd(side, cache), tree, yenv(side, cache), log=self.log(name, case, f"r{rnd}", side, "setup"))
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
    m = metas[0]["machine"] if metas else {}
    a = next((r["args"] for r in metas if r.get("args")), {})
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
          "npm's cache is warm for both sides in every case (primed by a plain install first; network is not measured), "
          "npm runs with `--no-audit --no-fund --prefer-offline`. Yarn 1 runs use `--ignore-scripts` (the project's install "
          "scripts fail in plain Yarn too), and Yarn's tarballs come from an offline mirror primed first, so an empty Yarn cache "
          "means extracting, not downloading. "
          f"A run that started with a 1-minute load average above {a.get('load_max', '?')} is counted in the LOAD column, not dropped. "
          "Disk: blocks of node_modules shared with the store or Yarn's cache (first extent, as `scripts/shared-blocks.py`), and the "
          "bytes allocated by the files that share nothing (new bytes); metadata (inodes, directories) not counted.", ""]
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
          "From the first round(s) of each case. New bytes: allocated by node_modules files that share no block with the store or cache. "
          "Store / cache: what matchapack's store (npm) or Yarn's cache holds after the run, allocated, paid once per machine.", "",
          "| Project | Case | Side | Files sharing blocks | Bytes shared | New bytes | Store / cache |", "|---|---|---|---|---|---|---|"]
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
            L.append(f"- **{pname}**: each extra worktree's node_modules writes {human(q)} with matchapack instead of {human(p)} "
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
    L += (metas[0].get("top_cpu", []) if metas else []) + ["```", ""]
    sent = [r for r in read_lines(w.p("results", "sentinels.jsonl")) if r.get("bench_id") == bench_id]
    if sent:
        last = sent[-1]
        L += ["## Isolation check", "", "The user's real matchapack state and npm / Yarn caches: "
              + ("unchanged." if not last.get("changed") else "CHANGED: " + "; ".join(last["changed"])), ""]
    return "\n".join(L) + "\n"


# --- main ------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="matchapack benchmark (issue grimmerk/matchapack-private#7); run through scripts/bench.sh")
    ap.add_argument("--work", required=True, help="work directory (created; never under ~/git)")
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

    root = os.path.abspath(os.path.expanduser(a.work))
    gitdir = os.path.realpath(os.path.expanduser("~/git"))
    if root == gitdir or root.startswith(gitdir + os.sep) or os.path.realpath(root).startswith(gitdir + os.sep):
        die(f"--work {root} is under ~/git: choose another directory")
    os.makedirs(root, exist_ok=True)
    root = os.path.realpath(root)
    if root.startswith(os.path.realpath(HERE) + os.sep):
        die("--work is inside this checkout")
    if os.stat(root).st_gid not in os.getgroups():
        # macOS gives a new file its directory's group: npm's extraction
        # then has a group matchapack cannot give its clones (/tmp is
        # wheel), and the numbers would not be a user's.
        die(f"{root} has group {os.stat(root).st_gid}, not one of yours: use a directory in your home")
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
    interrupted = None
    try:
        for x in a.projects:
            spec = dict(PROJECTS[x])
            if spec["kind"] == "npm":
                b.npm_project(x, spec)
            else:
                b.yarn_project(x, spec)
            append_line(w.p("results", "meta.jsonl"), {"bench_id": a.bench_id, "ts": time.time(), "after": x,
                                                       "elapsed_s": round(time.monotonic() - t0), "top_cpu": top_cpu(),
                                                       "free_gib": round(free_bytes(root) / GIB, 1)})
    except KeyboardInterrupt as e:
        interrupted = str(e) or "interrupted"
        say(f"stopped ({interrupted}); runs recorded so far are kept; the same --bench-id {a.bench_id} resumes")
    s1 = sentinels()
    changed = sentinel_changes(s0, s1, started)
    append_line(w.p("results", "sentinels.jsonl"), {"bench_id": a.bench_id, "when": "end", "ts": time.time(), "state": s1, "changed": changed})
    write_atomic(w.p("results", "summary.md"), summary(w, a.bench_id, a.fill_npm, a.smoke))
    say(f"elapsed {(time.monotonic() - t0) / 3600:.2f} h; results in {w.p('results')} (summary.md, runs.jsonl)")
    if changed:
        say("!!! ISOLATION CHECK FAILED: the user's real state changed during the run (this run or another process):")
        for c in changed:
            say("!!!   " + c)
        sys.exit(4)
    if interrupted:
        sys.exit(130)
    if b.failures:
        say(f"{b.failures} timed runs failed (exit status in runs.jsonl, logs in logs/)")
        sys.exit(1)


if __name__ == "__main__":
    main()
