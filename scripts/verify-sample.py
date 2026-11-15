#!/usr/bin/env python3
"""The picking, replaying and recording of scripts/verify-sample.sh.

  verify-sample.py npm  <log dir> <N> <checkout> [project-dir...]
  verify-sample.py yarn <log dir> <N> <checkout> [project-dir...]

One log (`log.jsonl`) to pick from and one file (`verify.jsonl`) to record
in, for both engines; `matchapack report` shows both (src/trial.rs). A
sample replays one logged run of one project, plain and through matchapack,
on copies, and compares the trees (NOTES "Sampled verification of real
runs"). Two kinds of run are replayed (issue grimmerk/matchapack-private#77):

- full installs: `npm ci` (with the logged words, on copy-on-write copies of
  the whole project, by scripts/verify.sh); `yarn` / `yarn install` with or
  without --frozen-lockfile (on copies of the manifests, by
  scripts/verify-yarn1.sh --engine yarn1), from the files as the run left
  them;
- commands that started with no node_modules (part 1b): npm `install`,
  `update`, `uninstall`; Yarn `add`, `upgrade`, `remove` and their `yarn
  workspace <name> …` forms, from the files before the run: the project's
  while they still have the hashes the run logged, else its git HEAD's
  when those have them.

The rule (review rounds 1 and 2): a sample says IDENTICAL only when (a)
the replay's inputs equal what the logged run recorded (the files it
hashed, their bytes, the words it logged; the environment and the rc
files are not logged), (b) matchapack's run in the replay went through
matchapack and cloned at least as much as the logged run, and (c) every
check of the comparison passed (its exit status, not only the line it
printed). When the sampler cannot be sure the replay repeated the logged
run, it does not say IDENTICAL.
Whatever cannot be proven is SKIPPED or ERROR, with the reason; matchapack
failing where plain npm or Yarn succeeded is DIFFERENT.
"""
import fcntl, hashlib, json, os, re, shutil, signal, subprocess, sys, tempfile, time, traceback

NPM_INCREMENTAL = ("install", "update", "uninstall")
YARN_INCREMENTAL = ("add", "upgrade", "remove", "upgrade-interactive")
STARTED_EMPTY = ("absent", "empty")
# The files a copy holds (every one outside node_modules and .git): the
# manifests the package manager reads to install, and the rc files it reads
# in the project.
COPIED = {"yarn1": ("package.json", "yarn.lock", ".yarnrc", ".npmrc", ".yarnclean"),
          "npm": ("package.json", "package-lock.json", "npm-shrinkwrap.json", ".npmrc")}
DEP_FIELDS = ("dependencies", "devDependencies", "optionalDependencies", "peerDependencies", "resolutions", "overrides")
# At most this many projects are recorded as SKIPPED (or as failed to plan)
# in one run; the search for N replays goes on past them.
MAX_SKIPS = 20


def read_log(path):
    try:
        lines = open(path, errors="replace").read().splitlines()
    except FileNotFoundError:
        return []
    out = []
    for l in lines:
        try:
            r = json.loads(l)
        except ValueError:
            continue
        if isinstance(r, dict):
            out.append(r)
    return out


def append(path, rec):
    """One whole line under the lock matchapack's own log writer takes
    (src/trial.rs append_line), in case two samplers run at once."""
    rec = dict(rec, ts=time.time())
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        f.write(json.dumps(rec) + "\n")


def project_of(r):
    return r.get("root") or r.get("dir")


def kind_of(r):
    """"full", "started-empty", or None (not replayed: an incremental command
    in a tree that already had node_modules, a failed run, …)."""
    if r.get("outcome") != "installed":
        return None
    if r.get("engine") == "yarn1":
        c = r.get("command", "install")
        if c == "install":
            return "full"
        base = c.split(" ", 1)[1] if c.startswith("workspace ") else c
        return "started-empty" if base in YARN_INCREMENTAL and r.get("node_modules_before") in STARTED_EMPTY else None
    # npm: records without a command are npm ci's (before the install
    # family was routed).
    c = r.get("command", "ci")
    if c == "ci":
        return "full"
    return "started-empty" if c in NPM_INCREMENTAL and r.get("node_modules_before") in STARTED_EMPTY else None


def logged_clones(r):
    """How many files (Yarn) or packages (npm) the logged run cloned, or None."""
    if r.get("engine") == "yarn1":
        return (r.get("yarn1") or {}).get("cloned")
    return r.get("packages")


def candidates(path, engine):
    """[(record, project)], newest project first: for each project, its
    newest replayable run that cloned something, else its newest replayable
    run (which is then skipped: nothing to compare a clone with)."""
    best, order = {}, []
    for r in reversed(read_log(path)):
        if (r.get("engine") == "yarn1") != (engine == "yarn1") or not kind_of(r):
            continue
        d = project_of(r)
        if not d:
            continue
        if d not in best:
            best[d] = r
            order.append(d)
        elif not (logged_clones(best[d]) or 0) and (logged_clones(r) or 0) > 0:
            best[d] = r
    return [(best[d], d) for d in order]


def pick_dirs(path, dirs, engine):
    """The run to replay for each directory given (its project or the
    directory it ran in), as `candidates` picks it, or (None, dir)."""
    cands = candidates(path, engine)
    out = []
    for d in dirs:
        real = os.path.realpath(d)
        hit = next((r for r, p in cands if real in (os.path.realpath(p), os.path.realpath(r.get("dir") or ""))), None)
        out.append((hit, project_of(hit) if hit else real))
    return out


def sha256_of(p):
    try:
        with open(p, "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()
    except FileNotFoundError:
        return None
    except OSError:
        return "unreadable"


def git_blob(root, rel):
    """The bytes of <rel> in the git HEAD of the repository holding root, or
    None (not a repository, not in HEAD)."""
    try:
        p = subprocess.run(["git", "-C", root, "show", "HEAD:./" + rel], stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    return p.stdout if p.returncode == 0 else None


def state_now(here, engine, root):
    """The project's state now with the logger's own rules (`matchapack
    replay-state`, src/replay.rs): {"node_modules", "files", "complete"}."""
    p = subprocess.run([os.path.join(here, "target/release/matchapack"), "replay-state", engine, root], capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError("matchapack replay-state failed: %s" % p.stderr.strip()[-200:])
    return json.loads(p.stdout)


def files_state(r, root, kind, now_state):
    """(files, source, None): {relative path: bytes, or None for a file that
    is not there} to give the copies, the state the replay starts from, and
    where it came from ("project" or "git HEAD"); or (None, None, why). A
    full install starts from the files the run left (its lockfile may have
    been written by it); a command that started empty from the files before
    the run. The set of files must be the run's too: no workspace added or
    removed since, and none the logger could not enumerate."""
    fb, fa = r.get("files_before"), r.get("files_after")
    if not isinstance(fb, dict) or not isinstance(fa, dict) or not fb:
        return None, None, "the log record has no hashes of the files before and after the run (logged by a matchapack build from before replays were recorded)"
    if r.get("files_complete") is not True:
        return None, None, "the logged file set may be incomplete (a workspace pattern matchapack does not expand, or its listing budget ran out)" \
            if r.get("files_complete") is False else "the log record does not say whether its file set is complete (logged by an older matchapack build)"
    if now_state.get("complete") is not True:
        return None, None, "the project's workspaces cannot all be found now (a pattern matchapack does not expand, or its listing budget ran out)"
    now = now_state.get("files") or {}
    if set(now) != set(fa) or set(fb) != set(fa):
        added = sorted(set(now) - set(fa))
        gone = sorted(set(fa) - set(now))
        return None, None, "the workspaces changed since the run (%s)" % ", ".join(["+" + k for k in added] + ["-" + k for k in gone] + (["the run itself changed them"] if set(fb) != set(fa) else []))
    want = fa if kind == "full" else fb
    changed_since = sorted(k for k in fa if now.get(k) != fa[k])
    if all(now.get(k) == want[k] for k in want):
        files = {}
        for k in want:
            if want[k] is None:
                files[k] = None
                continue
            b = open(os.path.join(root, k), "rb").read()
            if hashlib.sha256(b).hexdigest() != want[k]:
                return None, None, "%s changed while it was read" % k
            files[k] = b
        return files, "project", None
    if kind == "full":
        return None, None, "%s changed since the run" % ", ".join(changed_since)
    # The files before the run, from git HEAD.
    got = {}
    for k, h in want.items():
        if h is None:
            got[k] = None
            continue
        b = git_blob(root, k)
        if b is None or hashlib.sha256(b).hexdigest() != h:
            got = None
            break
        got[k] = b
    if got is not None:
        return got, "git HEAD", None
    changed_by_run = sorted(k for k in fb if fb[k] != fa.get(k))
    why = "%s changed since the run" % ", ".join(changed_since) if changed_since else "the run changed %s" % ", ".join(changed_by_run)
    return None, None, why + ", and the files before it are not in the project's git HEAD"


def local_dependency(files):
    """A dependency on a local path named by one of the manifests the replay
    reads (`file:`, `link:`, `portal:`, or a path: `./`, `../`, `/`, `~/`),
    or None. The copies hold manifests only: the directory or tarball it
    names is not in them, so the replay would not be the run's (measured: a
    `file:` directory, and a `./` one, replayed IDENTICAL with nothing or
    almost nothing of it cloned)."""
    for k, b in sorted(files.items()):
        if b is None or not k.endswith("package.json"):
            continue
        try:
            pj = json.loads(b)
        except ValueError:
            continue
        for field in DEP_FIELDS:
            deps = pj.get(field) if isinstance(pj, dict) else None
            if not isinstance(deps, dict):
                continue
            for name, spec in deps.items():
                if isinstance(spec, str) and re.match(r"(file|link|portal):|\.{1,2}/|/|~/", spec.strip()):
                    return "%s: %s %s" % (k, name, spec)
    return None


def copy_manifests(root, dest, sub, names, files):
    """The project's manifests and rc files (`names`), outside node_modules
    and .git, into dest, with their times and modes; then `files` written
    over them (None: removed); and the directory the run started in.
    Returns how many files the copy holds."""
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(x for x in dirnames if x not in ("node_modules", ".git"))
        rel = os.path.relpath(dirpath, root)
        for f in filenames:
            if f not in names:
                continue
            s = os.path.join(dirpath, f)
            if os.path.islink(s) or not os.path.isfile(s):
                continue
            os.makedirs(os.path.join(dest, rel), exist_ok=True)
            shutil.copy2(s, os.path.join(dest, rel, f))
    for k, b in files.items():
        p = os.path.join(dest, k)
        if b is None:
            if os.path.lexists(p):
                os.remove(p)
            continue
        if not (os.path.isfile(p) and open(p, "rb").read() == b):
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "wb") as f:
                f.write(b)
    os.makedirs(os.path.join(dest, sub), exist_ok=True)
    return sum(len(fs) for _, _, fs in os.walk(dest))


def run_version(argv, cwd):
    try:
        p = subprocess.run(argv, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60,
                           env=dict(os.environ, YARN_DISABLE_SELF_UPDATE_CHECK="true"))
    except (OSError, subprocess.SubprocessError) as e:
        return None, str(e)
    return (p.stdout.strip() if p.returncode == 0 else None), p.stderr.strip()[-200:]


def node_at_least(version, major, minor):
    m = re.match(r"v?(\d+)\.(\d+)", version or "")
    return bool(m) and (int(m.group(1)), int(m.group(2))) >= (major, minor)


def subdir_of(r, root):
    cwd = r.get("dir")
    if not cwd or not os.path.isdir(cwd):
        return None, "the directory the run started in is gone (%s)" % cwd
    sub = os.path.relpath(os.path.realpath(cwd), os.path.realpath(root))
    if sub == ".." or sub.startswith("../"):
        return None, "the run started outside its project (%s)" % cwd
    return sub, None


def hidden(args):
    return any("***" in a for a in args)


def clones_needed(r):
    """None when the logged run cloned something, else why it is skipped."""
    n = logged_clones(r)
    if not isinstance(n, int):
        return "the log record does not say how much the run cloned"
    if n == 0:
        return "nothing was cloned in the logged run, so there is no clone to check"
    return None


def started_as_logged(plan, started, result, reason):
    """A command that started with no node_modules must have started so in
    the replay too (matchapack's own record of the replay says): otherwise
    the comparison is of another run."""
    if plan["kind"] == "started-empty" and result not in ("ERROR", "DIFFERENT") and started not in STARTED_EMPTY:
        return "ERROR", "the replay did not start with no node_modules (matchapack logged %s)" % started
    return result, reason


def cloned_as_logged(plan, cloned, result, reason):
    """The replay's matchapack run must have cloned when the logged run did."""
    if result in ("ERROR", "DIFFERENT"):
        return result, reason
    if not isinstance(cloned, int):
        return "ERROR", "matchapack's run in the replay left no record of what it cloned"
    if cloned == 0 and (plan.get("logged_cloned") or 0) > 0:
        return "ERROR", "the replay cloned 0, the logged run %s: the comparison would not test matchapack" % plan["logged_cloned"]
    if cloned == 0 and plan.get("logged_cloned") is None:
        return "ERROR", "the replay cloned nothing: the comparison would not test matchapack"
    # Fewer than the logged run: the replay did not repeat it (every faithful
    # replay measured cloned as many: 194, 314, 632, 190; npm 2, 3). More is
    # fine: the logged run's store may have lacked some packages.
    if isinstance(plan.get("logged_cloned"), int) and cloned < plan["logged_cloned"]:
        return "ERROR", "the replay cloned %d, the logged run %d: it did not repeat the run" % (cloned, plan["logged_cloned"])
    return result, reason


def first_copies_kept(e):
    """None, or why the Yarn engine's own record breaks its invariant: when
    it cloned, it left exactly Yarn's first copies (`first`, 128 in every
    correct record measured) to Node (`copied["first-copies"]`). A replay
    that breaks it is DIFFERENT, whatever the trees: with those copies
    cloned, plain Yarn's time probe sees clones, and the tree is right only
    by chance (NOTES "The probe race", mutation M2)."""
    y = e.get("yarn1") or {}
    if not isinstance(y.get("cloned"), int) or y["cloned"] == 0:
        return None
    first, kept = y.get("first"), (y.get("copied") or {}).get("first-copies", 0)
    if not isinstance(first, int) or kept != first:
        return "the engine did not leave Yarn's first copies to Node (first-copies %s, first %s)" % (kept, first)
    return None


def first_error(lines):
    for l in lines:
        if re.search(r"error|ERR!|failed|panicked|not handled", l, re.I) and not l.startswith(("engine yarn failed", "== ")):
            return l.strip()
    return lines[0].strip() if lines else "no output"


# --- Yarn 1 ------------------------------------------------------------------

def yarn_plan(r, d, here):
    """(plan, None) when the logged run can be replayed, else (None, why)."""
    if r is None:
        return None, "no replayable `matchapack yarn` run of this directory in the log"
    kind = kind_of(r)
    y = r.get("yarn1") or {}
    root = r.get("root")
    need = {"root": root, "yarn1.yarn_js": y.get("yarn_js"), "yarn1.node_path": y.get("node_path"), "yarn1.node": y.get("node"), "replay": r.get("replay")}
    missing = [k for k, v in need.items() if not v]
    if missing:
        return None, "the log record has no %s (logged by a matchapack build from before replays were recorded)" % ", ".join(missing)
    if r.get("command") == "upgrade-interactive" or r.get("command", "").endswith(" upgrade-interactive"):
        return None, "upgrade-interactive: the choices made in its prompt are not logged"
    why = clones_needed(r)
    if why:
        return None, why
    if not os.path.isdir(root):
        return None, "the project directory is gone"
    sub, why = subdir_of(r, root)
    if why:
        return None, why
    files, source, why = files_state(r, root, kind, state_now(here, "yarn1", root))
    if why:
        return None, why
    why = local_dependency(files)
    if why:
        return None, "a local dependency (%s); the copies hold manifests only" % why
    node, yarn_js = y["node_path"], y["yarn_js"]
    version = r.get("yarn") or y.get("yarn")
    if not os.access(node, os.X_OK):
        return None, "the node that ran is gone (%s)" % node
    tmp = tempfile.gettempdir()
    nv, err = run_version([node, "--version"], tmp)
    if nv != y["node"]:
        return None, "%s is now %s, the run used %s" % (node, nv or "not runnable (%s)" % err, y["node"])
    if not os.path.isfile(yarn_js):
        return None, "the Yarn that ran is gone (%s)" % yarn_js
    yv, err = run_version([node, yarn_js, "--version"], tmp)
    if not version or yv != version:
        return None, "%s is now Yarn %s, the run used %s" % (yarn_js, yv or "not runnable (%s)" % err, version or "an unknown version")
    cache = y.get("cache")
    if not isinstance(cache, str) or not cache:
        return None, "the run did not learn Yarn's cache folder"
    if cache.startswith("unusable: "):
        return None, "Yarn's cache folder was not usable in the run (%s)" % cache[len("unusable: "):]
    # `yarn cache dir` prints <root>/v<N>; --cache-folder takes the root.
    if not re.fullmatch(r"v\d+", os.path.basename(cache)):
        return None, "Yarn's cache folder %s does not end in v<N>: its root is not known" % cache
    if not os.path.isdir(cache):
        return None, "Yarn's cache folder is gone (%s)" % cache
    args = r["replay"].get("args") if isinstance(r["replay"], dict) else None
    if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
        return None, "the logged arguments are not a list of words"
    if hidden(args):
        return None, "an argument was hidden in the log (a secret): the run cannot be repeated"
    if kind != "full" and any(re.search(r"\s", a) or not a for a in args):
        return None, "an argument holds a space: the command cannot be given as words"
    for i, a in enumerate(args):
        v = args[i + 1] if a == "--use-yarnrc" and i + 1 < len(args) else a.split("=", 1)[1] if a.startswith("--use-yarnrc=") else None
        if v is not None and not os.path.isabs(v):
            return None, "--use-yarnrc %s is relative: the copies would read another file, or none" % v
    # Yarn reads .yarnrc and .npmrc in every directory from where it runs up
    # to /. The copies sit elsewhere: one above the project would be read by
    # the logged run and not by the replay. (The home directory's are
    # Yarn's user files, read by both; they are not looked at here.)
    home = os.path.realpath(os.path.expanduser("~"))
    p = os.path.dirname(os.path.realpath(root))
    while True:
        if p != home:
            for f in (".yarnrc", ".npmrc"):
                if os.path.lexists(os.path.join(p, f)):
                    return None, "%s, above the project, is read by Yarn there but not in the copies" % os.path.join(p, f)
        if p == "/":
            break
        p = os.path.dirname(p)
    return {"kind": kind, "root": root, "sub": sub, "files": files, "source": source, "node": node, "node_version": nv, "yarn_js": yarn_js,
            "yarn": version, "cache_root": os.path.dirname(cache), "args": args, "logged_cloned": y.get("cloned"),
            "fresh": kind == "full" and r.get("node_modules_before") not in STARTED_EMPTY}, None


def window_secs(runs_file, name):
    try:
        for l in open(runs_file):
            w = l.split()
            if len(w) >= 2 and w[0] == name:
                s, e = w[1].split(":")
                return round((int(e) - int(s)) / 1e9, 2)
    except (OSError, ValueError):
        pass
    return None


def has_empty_package_file(tree):
    for dirpath, _, filenames in os.walk(os.path.join(tree, "node_modules")):
        for f in filenames:
            p = os.path.join(dirpath, f)
            try:
                if not os.path.islink(p) and os.path.isfile(p) and os.lstat(p).st_size == 0:
                    return True
            except OSError:
                pass
    return False


def race_shaped(own, tree, node_version):
    """Can the engine's own differences be plain Yarn's probe race (#57,
    NOTES "The probe race")? Under Node 22.12 or later (libuv 1.49: a copy
    sets the times before the data), in a tree with an empty package file
    (the race needs empty and non-empty files among Yarn's first copies):
    mtime differences only, each on a non-empty regular file under a
    node_modules, never .yarn-integrity."""
    if not own or not node_at_least(node_version, 22, 12) or not has_empty_package_file(tree):
        return False
    for cls, path in own:
        if cls != "mtime" or not (path.startswith("node_modules/") or "/node_modules/" in path) or path.endswith(".yarn-integrity"):
            return False
        p = os.path.join(tree, path)
        if os.path.islink(p) or not os.path.isfile(p) or os.lstat(p).st_size == 0:
            return False
    return True


def yarn_result(rc, out, v, here, node_version, race_possible=True):
    """(result, reason, extra) from verify-yarn1.sh's exit status, output
    and work directory. `race_possible`: false for a run with one pool
    thread, where plain Yarn's race cannot happen."""
    extra = {}
    if rc == 0 and "plain vs engine: IDENTICAL" in out:
        return "IDENTICAL", None, extra
    if rc == 3 and re.search(r"plain vs engine: (IDENTICAL|differs only where plain varies)", out):
        # Two plain runs differ; the engine differs from plain1 only where
        # they do, in the same class (scripts/yarn1-own-diffs.sh).
        try:
            plain = [l.strip() for l in open(os.path.join(v, "diff-plain.txt"), errors="replace") if l.startswith("    ")]
        except OSError:
            plain = []
        extra["plain_varies"] = plain[:20]
        return "PLAIN-VARIES", "plain Yarn varies between two runs; the engine differs only there", extra
    if rc == 1 and "plain vs engine: ENGINE DIFFERS" in out:
        p = subprocess.run([os.path.join(here, "scripts/yarn1-own-diffs.sh"), os.path.join(v, "classes-plain.txt"), os.path.join(v, "classes-engine.txt")],
                           capture_output=True, text=True)
        if p.returncode != 0:
            return "ERROR", "comparing the engine's differences with plain Yarn's failed (yarn1-own-diffs.sh exit %d)" % p.returncode, extra
        own = [tuple(l.split("\t", 1)) for l in p.stdout.splitlines() if "\t" in l]
        extra["differing"] = ["%s %s" % c for c in own[:20]]
        extra["differing_count"] = len(own)
        if race_possible and race_shaped(own, os.path.join(v, "engine"), node_version):
            return "RACE-SHAPED", "mtimes of non-empty package files only, as plain Yarn's own probe race (#57) can leave them", extra
        return "DIFFERENT", "the engine's tree differs from plain Yarn's where two plain runs do not", extra
    lines = [l for l in out.strip().splitlines() if l.strip()]
    extra["detail"] = lines[-12:]
    failed = next((i for i, l in enumerate(lines) if l.startswith("plain yarn failed in ")), None)
    if failed is not None:
        return "ERROR", "plain Yarn failed in its copy (%s: %s)" % (lines[failed].rstrip(":"), first_error(lines[failed + 1:])), extra
    eng = next((i for i, l in enumerate(lines) if l.startswith("engine yarn failed in ")), None)
    if eng is not None:
        tail = lines[eng + 1:]
        if any("not handled, left unchanged" in l for l in tail):
            return "ERROR", "matchapack declined in the replay: %s" % next(l.strip() for l in tail if "not handled, left unchanged" in l), extra
        # Plain Yarn installed in three copies; matchapack failed in the fourth.
        return "DIFFERENT", "matchapack failed where plain Yarn did not: %s" % first_error(tail), extra
    why = next((l for l in reversed(lines) if l.startswith(("engine: ", "command: "))), lines[-1] if lines else "no output")
    return "ERROR", "verify-yarn1.sh exit %d: %s" % (rc, why.rstrip(":")), extra


def yarn_run(plan, here, src, v, one_thread):
    argv = [os.path.join(here, "scripts/verify-yarn1.sh"), "--work", v, "--cache-dir", plan["cache_root"], "--engine", "yarn1",
            "--engine-min-clones", "any", "--args-as-given"]
    if plan["sub"] != ".":
        argv += ["--subdir", plan["sub"]]
    if plan["kind"] == "full":
        argv += [src] + plan["args"]
    else:
        argv += ["--command", " ".join(plan["args"]), "--command-fresh", src]
    env = dict(os.environ, MATCHAPACK_YARN_NODE=plan["node"], MATCHAPACK_YARN_JS=plan["yarn_js"])
    if one_thread:
        env["UV_THREADPOOL_SIZE"] = "1"
    p = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace")
    out = p.stdout + p.stderr
    for l in out.splitlines():
        if l.startswith(("yarn ", "engine: ", "plain vs ", "engine tree damaged", "command: ")):
            print("  " + l)
    result, reason, extra = yarn_result(p.returncode, out, v, here, plan["node_version"], race_possible=not one_thread)
    cloned, started, e = None, None, {}
    try:
        e = json.load(open(os.path.join(v, "engine.json")))
        cloned, started = (e.get("yarn1") or {}).get("cloned"), e.get("node_modules_before")
    except (OSError, ValueError):
        pass
    broken = first_copies_kept(e)
    if broken and result != "DIFFERENT":
        result, reason = "DIFFERENT", broken
    result, reason = started_as_logged(plan, started, result, reason)
    result, reason = cloned_as_logged(plan, cloned, result, reason)
    return result, reason, extra, cloned


def yarn_one(plan, here, work):
    src = os.path.join(work, "src")
    copied = copy_manifests(plan["root"], src, plan["sub"], COPIED["yarn1"], plan["files"])
    v = os.path.join(work, "v")
    result, reason, extra, cloned = yarn_run(plan, here, src, v, False)
    rec = dict(yarn=plan["yarn"], node=plan["node_version"], args=plan["args"], subdir=plan["sub"], replayed_from=plan["source"],
               manifests_copied=copied, yarn_secs=window_secs(os.path.join(v, "runs"), "plain1"),
               matchapack_secs=window_secs(os.path.join(v, "runs"), "engine"), logged_cloned=plan["logged_cloned"])
    if plan["fresh"]:
        # The logged install ran over an existing node_modules; the replay
        # installs from scratch.
        rec["replayed_fresh"] = True
    if result in ("RACE-SHAPED", "PLAIN-VARIES"):
        # Once more with one pool thread, where plain Yarn's probe race
        # cannot happen: a difference that stays is the engine's (DIFFERENT).
        # One that is gone does not prove the first was plain Yarn's race (a
        # faulty engine is not deterministic either: measured with mutation
        # M2, 7 of 40 samples identical after a race-shaped first run), so
        # the sample stays RACE-SHAPED, with the second run's verdict kept.
        print("  %s: once more with one pool thread (UV_THREADPOOL_SIZE=1)" % result)
        first, first_reason, first_extra = result, reason, extra
        rec["first_result"], rec["first_reason"] = first, first_reason
        rec["first_differing"] = extra.get("differing") or extra.get("plain_varies")
        shutil.rmtree(v, ignore_errors=True)
        result, reason, extra, cloned = yarn_run(plan, here, src, v, True)
        rec["rerun"] = "one pool thread"
        rec["rerun_result"], rec["rerun_reason"] = result, reason
        if first == "RACE-SHAPED" and result in ("IDENTICAL", "PLAIN-VARIES"):
            result, reason, extra = first, first_reason + "; gone with one pool thread, which does not prove it was the race: sample it again", first_extra
        elif result == "PLAIN-VARIES":
            # Not the race (one thread): output that varies from run to run,
            # in which the engine differs only where plain Yarn does.
            result = "IDENTICAL"
            reason = "plain Yarn varies between two runs with one pool thread too (not the race); the engine differs only there"
    if result == "PLAIN-VARIES":
        result = "IDENTICAL"
    rec.update(result=result, cloned=cloned)
    if reason:
        rec["reason"] = reason
    rec.update(extra)
    return rec


# --- npm ---------------------------------------------------------------------

# The comparison is fixed here, never inherited (as in verify-npm.sh): a
# caller's settings that loosen or reroute verify.sh do not apply.
VERIFY_KNOBS = ("MATCHAPACK_VERIFY_IGNORE", "MATCHAPACK_VERIFY_BASELINE", "MATCHAPACK_VERIFY_MTIME", "MATCHAPACK_VERIFY_VIA",
                "MATCHAPACK_VERIFY_BYTES", "MATCHAPACK_VERIFY_CLONES", "MATCHAPACK_VERIFY_META", "MATCHAPACK_VERIFY_ARGS_FILE")
# verify.sh's lines that say a check failed (it exits 1 for each).
FAILED_CHECKS = (r"^stale in B only: [1-9]", r"^npm ls: DIFFERS", r"^hidden lockfile is not byte-identical", r"^meta: DIFFERENT",
                 r"byte-identical \(A, B\): false")


def verify_env(**kw):
    env = {k: v for k, v in os.environ.items() if k not in VERIFY_KNOBS}
    env.update(kw)
    return env


def show_npm(out):
    shown = [l for l in out.splitlines() if re.search(r"RESULT|npm [\w-]+: [\d.]+s|noise|npm ls:|stale in B|byte-identical \(A, B\)|damaged|^  ", l)]
    for l in shown[:20]:
        print(l)


def npm_plan(r, d, here, explicit):
    if r is None:
        return None, "no replayable `matchapack npm` run of this directory in the log"
    kind = kind_of(r)
    root = r.get("root") or r.get("dir")
    if not root or not os.path.isdir(root):
        return None, "the project directory is gone"
    if explicit:
        # A directory given: npm ci on the project as it is now.
        return {"kind": "full", "root": root, "args": ["ci"], "explicit": True, "logged_cloned": None, "source": "project"}, None
    why = clones_needed(r)
    if why:
        return None, why
    sub, why = subdir_of(r, root)
    if why:
        return None, why
    if sub != ".":
        return None, "the run started in %s inside the project: replays start at the root only" % sub
    args = r.get("args")
    if not isinstance(args, list) or not args or not all(isinstance(a, str) for a in args):
        return None, "the logged arguments are not a list of words"
    if hidden(args):
        return None, "an argument was hidden in the log (a secret): the run cannot be repeated"
    # The copies are elsewhere: an absolute path would reach the project
    # itself (`--prefix /…`, `file:/…`), or another place than the run's.
    for a in args:
        if a.startswith("/") or "=/" in a or a.startswith("file:/") or a.startswith("~"):
            return None, "an argument is an absolute path (%s): the copies cannot repeat it" % a
        if "\n" in a or "\0" in a:
            return None, "an argument holds a line break or a NUL: it cannot be given back as one word"
    files, source, why = files_state(r, root, kind, state_now(here, "npm", root))
    if why:
        return None, why
    if kind != "full":
        why = local_dependency(files)
        if why:
            return None, "a local dependency (%s); the copies hold manifests only" % why
    nv, err = run_version(["npm", "--version"], tempfile.gettempdir())
    if not r.get("npm") or nv != r.get("npm"):
        return None, "the npm on PATH is %s, the run used %s" % (nv or "not runnable (%s)" % err, r.get("npm") or "an unknown version")
    return {"kind": kind, "root": root, "files": files, "source": source, "args": args, "npm": nv, "logged_cloned": logged_clones(r)}, None


def npm_result(rc, out):
    """(record fields) from verify.sh's exit status and output. IDENTICAL
    only on exit 0; a failing check (a stale mtime, npm ls, the bytes of
    package.json or the lockfile) is DIFFERENT, named; npm failing in A is
    ERROR; matchapack failing in B, where npm passed in A, is DIFFERENT."""
    lines = out.splitlines()
    m = re.search(r"RESULT: (\w+)", out)
    t = re.search(r"npm [\w-]+: ([\d.]+)s\s+matchapack: ([\d.]+)s", out)
    n = re.search(r"noise .*?: (\d+) paths", out)
    ls = re.search(r"npm ls: (.*)", out)
    paths = re.findall(r"^  ([^ :]+)(?::|$)", out.split("== diff A B")[-1].split("== mtime")[0], re.M)
    bytes_differ = re.findall(r"^(\S+) byte-identical \(A, B\): false", out, re.M)
    failed = [l for l in lines if any(re.search(f, l) for f in FAILED_CHECKS)]
    rec = {"differing": (["bytes " + b for b in bytes_differ] + paths)[:20],
           "npm_ci_secs": float(t.group(1)) if t else None, "matchapack_secs": float(t.group(2)) if t else None,
           "noise_paths": int(n.group(1)) if n else None, "npm_ls": ls.group(1).strip() if ls else None}
    if m:
        result = m.group(1)
        if result == "DIFFERENT" and paths and not failed and all(not (p.startswith("node_modules/") or "/node_modules/" in p) for p in paths):
            # Differences only OUTSIDE node_modules come from the project's
            # own build scripts: reported, kept apart from a node_modules mismatch.
            result = "BUILD-OUTPUT-ONLY"
        elif result == "IDENTICAL" and (failed or rc != 0):
            result = "DIFFERENT"
            rec["reason"] = "the trees are identical, but %s" % (failed[0] if failed else "verify.sh exited %d" % rc)
        elif result == "DIFFERENT":
            rec["reason"] = "the trees differ" + ("; also %s" % failed[0] if failed else "")
        rec["result"] = result
        return rec
    # No comparison: which run failed.
    rec["detail"] = lines[-15:]
    b_started = any(l.startswith("== matchapack install (B)") for l in lines)
    b_done = t is not None
    if b_started and not b_done:
        at = next(i for i, l in enumerate(lines) if l.startswith("== matchapack install (B)"))
        rec["result"], rec["reason"] = "DIFFERENT", "matchapack failed where plain npm did not: %s" % first_error(lines[at + 1:])
    elif not b_started:
        rec["result"], rec["reason"] = "ERROR", "plain npm failed in its copy: %s" % first_error(lines)
    else:
        rec["result"], rec["reason"] = "ERROR", "verify.sh exit %d after both installs: %s" % (rc, first_error(lines[-15:]))
    return rec


def npm_one(plan, here, work):
    """One npm replay through verify.sh with the logged words
    (MATCHAPACK_VERIFY_ARGS_FILE, NUL-separated): npm ci on copy-on-write
    copies of the whole project, a command that started empty on copies of
    its manifests."""
    if plan["kind"] == "full":
        src, copied = plan["root"], None
    else:
        src = os.path.join(work, "src")
        copied = copy_manifests(plan["root"], src, ".", COPIED["npm"], plan["files"])
    argsfile = os.path.join(work, "args")
    with open(argsfile, "wb") as f:
        f.write(b"".join(a.encode() + b"\0" for a in plan["args"]))
    # The clone count is checked from B's own record (cloned_as_logged), not
    # with verify.sh's MATCHAPACK_VERIFY_CLONES: that one would make "cloned
    # nothing" a failed comparison (DIFFERENT) instead of a replay that
    # tested nothing (ERROR).
    env = verify_env(MATCHAPACK_VERIFY_NOISE="1", MATCHAPACK_LOG_DIR=os.path.join(work, "logs"), MATCHAPACK_VERIFY_ARGS_FILE=argsfile)
    p = subprocess.run([os.path.join(here, "scripts/verify.sh"), src, os.path.join(work, "v")], env=env, stdin=subprocess.DEVNULL,
                       capture_output=True, text=True, errors="replace")
    show_npm(p.stdout + p.stderr)
    rec = npm_result(p.returncode, p.stdout + p.stderr)
    # The words verify.sh ran must be the logged ones, word for word.
    ran = re.search(r"^npm words: (.*)$", p.stdout, re.M)
    if not ran or json.loads(ran.group(1)) != plan["args"]:
        rec["result"], rec["reason"] = "ERROR", "the replay ran other words than the logged run (%s, not %s)" % (ran.group(1) if ran else "none reported", json.dumps(plan["args"]))
    # B's own log record (verify.sh gives it the work directory's log).
    logged = [x for x in read_log(os.path.join(work, "logs", "log.jsonl")) if x.get("engine") != "yarn1"]
    last = logged[-1] if logged else {}
    if rec["result"] != "ERROR" and last.get("outcome") in ("broke", "npm_failed", "declined"):
        # B finished (npm ci rebuilt a broken run with plain npm, or npm ran
        # plain), but matchapack's own run failed or declined.
        what = "declined" if last["outcome"] == "declined" else "failed (%s)" % last["outcome"]
        rec["result"] = "ERROR" if last["outcome"] == "declined" else "DIFFERENT"
        rec["reason"] = "matchapack %s in the replay where plain npm did not: %s" % (what, last.get("reason") or "?")
    rec["result"], reason = started_as_logged(plan, last.get("node_modules_before"), rec["result"], rec.get("reason"))
    rec["result"], reason = cloned_as_logged(plan, last.get("packages") if last.get("outcome") == "installed" else (0 if last else None), rec["result"], reason)
    if reason:
        rec["reason"] = reason
    rec.update(npm=plan.get("npm"), args=plan["args"], manifests_copied=copied, replayed_from=plan["source"], cloned=last.get("packages"),
               logged_cloned=plan.get("logged_cloned"))
    return rec


# --- both ---------------------------------------------------------------------

class Stop(Exception):
    pass


def on_signal(signum, frame):
    raise Stop("signal %d" % signum)


def main_engine(engine, logdir, n, here, dirs):
    log = os.path.join(logdir, "log.jsonl")
    vlog = os.path.join(logdir, "verify.jsonl")
    explicit = engine == "npm" and bool(dirs)
    if explicit:
        # Directories given: npm ci on each, as the sampler always did.
        picked = [({"command": "ci", "outcome": "installed", "root": os.path.realpath(d)}, os.path.realpath(d)) for d in dirs]
    else:
        picked = pick_dirs(log, dirs, engine) if dirs else candidates(log, engine)
    if not picked:
        what = "`matchapack yarn` runs" if engine == "yarn1" else "matchapack installs"
        print("nothing to verify: no replayable %s in %s yet" % (what, log))
        return 0
    m = re.search(r"\((.*)\)", subprocess.run([os.path.join(here, "target/release/matchapack"), "--version"], capture_output=True, text=True).stdout)
    build = m.group(1) if m else "unknown"
    keep = os.environ.get("MATCHAPACK_VERIFY_KEEP") == "1"
    # A replay's copies go even when the sampler is stopped (SIGTERM, SIGHUP).
    for s in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(s, on_signal)
    # The checkout's build must answer for the sampler's own questions.
    try:
        state_now(here, engine, tempfile.gettempdir())
    except Exception as e:
        append(vlog, {"build": build, "engine": engine, "result": "ERROR",
                      "reason": "this checkout's matchapack (%s) cannot tell a project's state (`replay-state`): build it first (cargo build --release); %s" % (build, e)})
        print("ERROR: %s cannot answer replay-state: %s" % (os.path.join(here, "target/release/matchapack"), e))
        return 1
    replayed = not_replayed = 0
    def note(rec):
        """A skip or a plan-step failure: recorded up to MAX_SKIPS per run;
        the search goes on either way."""
        nonlocal not_replayed
        not_replayed += 1
        if not_replayed <= MAX_SKIPS:
            append(vlog, rec)
            print("recorded: %s (%s)" % (rec["result"], rec["reason"]))
        else:
            print("not recorded (more than %d skips in this run): %s (%s)" % (MAX_SKIPS, rec["result"], rec["reason"]))
    for r, d in picked:
        # Without directories: until N replays ran; the search covers every
        # logged project (a skip does not use up one of N).
        if not dirs and replayed >= n:
            break
        kind = kind_of(r) if r else None
        command = (r.get("command") or ("install" if engine == "yarn1" else "ci")) if r else None
        print("######## %s %s %s" % ("yarn" if engine == "yarn1" else "npm", command or "", d))
        base = {"build": build, "logged_build": r.get("build") if r else None, "engine": engine, "kind": kind, "command": command, "dir": d,
                "logged_ts": r.get("ts") if r else None}
        try:
            plan, why = yarn_plan(r, d, here) if engine == "yarn1" else npm_plan(r, d, here, explicit)
        except Stop as e:
            append(vlog, dict(base, result="ERROR", reason="the sampler was stopped (%s)" % e))
            return 1
        except Exception as e:
            note(dict(base, result="ERROR", reason="planning the replay failed: %s: %s" % (type(e).__name__, e)))
            continue
        if why:
            note(dict(base, result="SKIPPED", reason=why))
            continue
        replayed += 1
        work = None
        try:
            work = tempfile.mkdtemp(prefix="matchapack-verify-%s." % ("yarn" if engine == "yarn1" else "npm"), dir=os.environ.get("TMPDIR") or None)
            rec = yarn_one(plan, here, work) if engine == "yarn1" else npm_one(plan, here, work)
        except Stop as e:
            append(vlog, dict(base, result="ERROR", reason="the sampler was stopped (%s)" % e))
            print("stopped (%s)" % e)
            return 1
        except Exception as e:
            # One sample's failure is recorded and the next goes on.
            rec = {"result": "ERROR", "reason": "the sampler failed: %s: %s" % (type(e).__name__, e), "detail": traceback.format_exc().splitlines()[-6:]}
        finally:
            if work:
                if keep:
                    print("  kept: %s" % work)
                else:
                    # The copies go; the comparison is in the record.
                    shutil.rmtree(work, ignore_errors=True)
        rec = dict(base, **rec)
        if rec["result"] == "ERROR" and not rec.get("reason"):
            rec["reason"] = "no reason was found (see detail)"
        append(vlog, rec)
        print("recorded: %s%s" % (rec["result"], " (%s)" % rec["reason"] if rec.get("reason") else ""))
        for l in rec.get("differing", [])[:10]:
            print("    " + l)
        if rec["result"] == "ERROR":
            print("\n".join("    " + l for l in rec.get("detail", [])))
    if not_replayed > MAX_SKIPS:
        print("%d projects skipped in all, %d of them recorded" % (not_replayed, MAX_SKIPS))
    return 0


def main(argv):
    if argv[:1] in (["npm"], ["yarn"]) and len(argv) >= 4:
        return main_engine("yarn1" if argv[0] == "yarn" else "npm", argv[1], int(argv[2]), argv[3], argv[4:])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
