#!/usr/bin/env python3
"""The picking, replaying and recording of scripts/verify-sample.sh.

  verify-sample.py npm  <log dir> <N> <checkout> [project-dir...]
  verify-sample.py yarn <log dir> <N> <checkout> [project-dir...]

One log (`log.jsonl`) to pick from and one file (`verify.jsonl`) to record
in, for both engines; `matchapack report` shows both (src/trial.rs). A
sample replays one logged run of one project, plain and through matchapack,
on copies, and compares the trees (NOTES "Sampled verification of real
runs"). Two kinds of run are replayed (issue grimmerk/matchapack-private#77):

- full installs: `npm ci` (on copy-on-write copies of the whole project, by
  scripts/verify.sh, as before); `yarn` / `yarn install` with or without
  --frozen-lockfile (on copies of the manifests, by scripts/verify-yarn1.sh
  --engine yarn1), when the project's lockfile and manifests are still what
  the run left;
- commands that started with no node_modules (part 1b): npm `install`,
  `update`, `uninstall`; Yarn `add`, `upgrade`, `remove` and their `yarn
  workspace <name> …` forms. They are replayed from the state before the
  run (the manifests and the lockfile, no node_modules): the files as they
  are now when they still have the hashes the run logged before it ran (or
  after it, when the run did not change them), else the versions in the
  project's git HEAD when those have the hashes. Otherwise the run is
  skipped, and the reason recorded.
"""
import fcntl, hashlib, json, os, re, shutil, subprocess, sys, tempfile, time

NPM_INCREMENTAL = ("install", "update", "uninstall")
YARN_INCREMENTAL = ("add", "upgrade", "remove", "upgrade-interactive")
STARTED_EMPTY = ("absent", "empty")
# The files a copy holds (every one outside node_modules and .git): the
# manifests the package manager reads to install, and the rc files it reads
# in the project.
COPIED = {"yarn1": ("package.json", "yarn.lock", ".yarnrc", ".npmrc"),
          "npm": ("package.json", "package-lock.json", "npm-shrinkwrap.json", ".npmrc")}


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


def pick(path, n, engine):
    """[(record, project)]: the newest replayable run of each of the N most
    recent distinct projects of `engine` ("npm" or "yarn1")."""
    seen, out = set(), []
    for r in reversed(read_log(path)):
        if (r.get("engine") == "yarn1") != (engine == "yarn1") or not kind_of(r):
            continue
        d = project_of(r)
        if not d or d in seen:
            continue
        seen.add(d)
        out.append((r, d))
        if len(out) >= n:
            break
    return out


def pick_dirs(path, dirs, engine):
    """The newest replayable run of each directory given (its project or the
    directory it ran in), or (None, dir)."""
    runs = [r for r in read_log(path) if (r.get("engine") == "yarn1") == (engine == "yarn1") and kind_of(r) and project_of(r)]
    runs.reverse()
    out = []
    for d in dirs:
        real = os.path.realpath(d)
        hit = next((r for r in runs if real in (os.path.realpath(project_of(r)), os.path.realpath(r.get("dir") or ""))), None)
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


def files_state(r, root, kind):
    """(files, None): {relative path: bytes, or None for a file that is not
    there} to give the copies, the state the replay starts from; or (None,
    why). A full install starts from the files the run left (its lockfile
    may have been written by it); a command that started empty from the
    files before the run."""
    fb, fa = r.get("files_before"), r.get("files_after")
    if not isinstance(fb, dict) or not isinstance(fa, dict) or not fb:
        return None, "the log record has no hashes of the files before and after the run (logged by a matchapack build from before replays were recorded)"
    want = fa if kind == "full" else fb
    now = {k: sha256_of(os.path.join(root, k)) for k in want}
    changed_since = sorted(k for k in fa if now.get(k, sha256_of(os.path.join(root, k))) != fa[k])
    if now == want:
        return {k: (open(os.path.join(root, k), "rb").read() if want[k] else None) for k in want}, None
    if kind == "full":
        return None, "%s changed since the run" % ", ".join(changed_since)
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
        return got, None
    changed_by_run = sorted(k for k in fb if fb[k] != fa.get(k))
    why = "%s changed since the run" % ", ".join(changed_since) if changed_since else "the run changed %s" % ", ".join(changed_by_run)
    return None, why + ", and the files before it are not in the project's git HEAD"


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


# --- Yarn 1 ------------------------------------------------------------------

def yarn_plan(r, d):
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
    if not os.path.isdir(root):
        return None, "the project directory is gone"
    sub, why = subdir_of(r, root)
    if why:
        return None, why
    files, why = files_state(r, root, kind)
    if why:
        return None, why
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
    return {"kind": kind, "root": root, "sub": sub, "files": files, "node": node, "node_version": nv, "yarn_js": yarn_js,
            "yarn": version, "cache_root": os.path.dirname(cache), "args": args, "logged_cloned": y.get("cloned")}, None


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
    node_modules, never .yarn-integrity. Not a pass: kept apart from both
    IDENTICAL and DIFFERENT, to be sampled again."""
    if not own or not node_at_least(node_version, 22, 12) or not has_empty_package_file(tree):
        return False
    for cls, path in own:
        if cls != "mtime" or not (path.startswith("node_modules/") or "/node_modules/" in path) or path.endswith(".yarn-integrity"):
            return False
        p = os.path.join(tree, path)
        if os.path.islink(p) or not os.path.isfile(p) or os.lstat(p).st_size == 0:
            return False
    return True


def yarn_result(rc, out, v, here, node_version):
    """(result, reason, extra) from verify-yarn1.sh's exit status, output
    and work directory."""
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
        return "IDENTICAL", "plain Yarn varies between two runs; the engine differs only there", extra
    if rc == 1 and "plain vs engine: ENGINE DIFFERS" in out:
        p = subprocess.run([os.path.join(here, "scripts/yarn1-own-diffs.sh"), os.path.join(v, "classes-plain.txt"), os.path.join(v, "classes-engine.txt")],
                           capture_output=True, text=True)
        if p.returncode != 0:
            return "ERROR", "comparing the engine's differences with plain Yarn's failed (yarn1-own-diffs.sh exit %d)" % p.returncode, extra
        own = [tuple(l.split("\t", 1)) for l in p.stdout.splitlines() if "\t" in l]
        extra["differing"] = ["%s %s" % c for c in own[:20]]
        extra["differing_count"] = len(own)
        if race_shaped(own, os.path.join(v, "engine"), node_version):
            return "RACE-SHAPED", "mtimes of non-empty package files only, as plain Yarn's own probe race (#57) can leave them: sample it again", extra
        return "DIFFERENT", "the engine's tree differs from plain Yarn's where two plain runs do not", extra
    lines = [l for l in out.strip().splitlines() if l.strip()]
    extra["detail"] = lines[-12:]
    failed = next((l for l in lines if l.startswith("plain yarn failed in ")), None)
    if failed:
        return "ERROR", "plain Yarn failed in its copy (%s; the copies hold the manifests only: a script or a `file:` dependency may need other files of the project)" % failed.rstrip(":"), extra
    why = next((l for l in reversed(lines) if l.startswith(("engine yarn failed", "engine: ", "command: "))), lines[-1] if lines else "no output")
    return "ERROR", "verify-yarn1.sh exit %d: %s" % (rc, why.rstrip(":")), extra


def yarn_one(plan, here, work):
    src = os.path.join(work, "src")
    copied = copy_manifests(plan["root"], src, plan["sub"], COPIED["yarn1"], plan["files"])
    v = os.path.join(work, "v")
    argv = [os.path.join(here, "scripts/verify-yarn1.sh"), "--work", v, "--cache-dir", plan["cache_root"], "--engine", "yarn1",
            "--engine-min-clones", "any", "--args-as-given"]
    if plan["sub"] != ".":
        argv += ["--subdir", plan["sub"]]
    if plan["kind"] == "full":
        argv += [src] + plan["args"]
    else:
        argv += ["--command", " ".join(plan["args"]), "--command-fresh", src]
    env = dict(os.environ, MATCHAPACK_YARN_NODE=plan["node"], MATCHAPACK_YARN_JS=plan["yarn_js"])
    p = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace")
    out = p.stdout + p.stderr
    open(os.path.join(work, "out.txt"), "w").write(out)
    for l in out.splitlines():
        if l.startswith(("yarn ", "engine: ", "plain vs ", "engine tree damaged", "command: ")):
            print("  " + l)
    result, reason, extra = yarn_result(p.returncode, out, v, here, plan["node_version"])
    cloned = None
    try:
        cloned = (json.load(open(os.path.join(v, "engine.json"))).get("yarn1") or {}).get("cloned")
    except (OSError, ValueError):
        pass
    rec = dict(result=result, yarn=plan["yarn"], node=plan["node_version"], args=plan["args"], subdir=plan["sub"],
               manifests_copied=copied, yarn_secs=window_secs(os.path.join(v, "runs"), "plain1"),
               matchapack_secs=window_secs(os.path.join(v, "runs"), "engine"), cloned=cloned, logged_cloned=plan["logged_cloned"])
    if reason:
        rec["reason"] = reason
    rec.update(extra)
    return rec


# --- npm ---------------------------------------------------------------------

# The comparison is fixed here, never inherited (as in verify-npm.sh): a
# caller's settings that loosen or reroute verify.sh do not apply.
VERIFY_KNOBS = ("MATCHAPACK_VERIFY_IGNORE", "MATCHAPACK_VERIFY_BASELINE", "MATCHAPACK_VERIFY_MTIME", "MATCHAPACK_VERIFY_VIA",
                "MATCHAPACK_VERIFY_BYTES", "MATCHAPACK_VERIFY_CLONES", "MATCHAPACK_VERIFY_META", "MATCHAPACK_VERIFY_ARGS_FILE")


def verify_env(**kw):
    env = {k: v for k, v in os.environ.items() if k not in VERIFY_KNOBS}
    env.update(kw)
    return env


def npm_ci_one(d, here, work):
    """npm ci on copy-on-write copies of the whole project (verify.sh), as
    the sampler always did."""
    v = os.path.join(work, "v")
    env = verify_env(MATCHAPACK_VERIFY_NOISE="1", MATCHAPACK_LOG_DIR=os.path.join(work, "logs"))
    p = subprocess.run([os.path.join(here, "scripts/verify.sh"), d, v], env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace")
    show_npm(p.stdout + p.stderr)
    return npm_result(p.stdout + p.stderr)


def show_npm(out):
    shown = [l for l in out.splitlines() if re.search(r"RESULT|npm [\w-]+: [\d.]+s|noise|npm ls:|stale in B|byte-identical \(A, B\)|damaged|^  ", l)]
    for l in shown[:20]:
        print(l)


def npm_plan(r, d):
    if r is None:
        return None, "no replayable `matchapack npm` run of this directory in the log"
    kind = kind_of(r)
    root = r.get("root") or r.get("dir")
    if not root or not os.path.isdir(root):
        return None, "the project directory is gone"
    if kind == "full":
        return {"kind": kind, "root": root}, None
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
    files, why = files_state(r, root, kind)
    if why:
        return None, why
    nv, err = run_version(["npm", "--version"], tempfile.gettempdir())
    if not r.get("npm") or nv != r.get("npm"):
        return None, "the npm on PATH is %s, the run used %s" % (nv or "not runnable (%s)" % err, r.get("npm") or "an unknown version")
    return {"kind": kind, "root": root, "files": files, "args": args, "npm": nv}, None


def npm_result(out):
    """(record fields) from verify.sh's output."""
    m = re.search(r"RESULT: (\w+)", out)
    t = re.search(r"npm [\w-]+: ([\d.]+)s\s+matchapack: ([\d.]+)s", out)
    n = re.search(r"noise .*?: (\d+) paths", out)
    ls = re.search(r"npm ls: (.*)", out)
    result = m.group(1) if m else "ERROR"
    # Differences only OUTSIDE node_modules come from the project's own build
    # scripts (e.g. tsdown output that varies run to run, which two npm ci runs may
    # happen to agree on): reported, but kept apart from a node_modules mismatch.
    paths = re.findall(r"^  ([^ :]+)(?::|$)", out.split("== diff A B")[-1].split("== mtime")[0], re.M)
    bytes_differ = re.findall(r"^(\S+) byte-identical \(A, B\): false", out, re.M)
    if result == "DIFFERENT" and paths and not bytes_differ and all(not (p.startswith("node_modules/") or "/node_modules/" in p) for p in paths):
        result = "BUILD-OUTPUT-ONLY"
    # Another command's package.json or lockfile bytes (verify.sh compares
    # them outside the noise rule).
    if bytes_differ and result in ("IDENTICAL", "ERROR") and m:
        result = "DIFFERENT"
    rec = {"result": result, "differing": (["bytes " + b for b in bytes_differ] + paths)[:20],
           "npm_ci_secs": float(t.group(1)) if t else None, "matchapack_secs": float(t.group(2)) if t else None,
           "noise_paths": int(n.group(1)) if n else None, "npm_ls": ls.group(1).strip() if ls else None}
    if result == "ERROR":
        rec["detail"] = out.strip().splitlines()[-15:]
    return rec


def npm_one(plan, here, work):
    src = os.path.join(work, "src")
    copied = copy_manifests(plan["root"], src, ".", COPIED["npm"], plan["files"])
    argsfile = os.path.join(work, "args")
    with open(argsfile, "w") as f:
        f.write("".join(a + "\n" for a in plan["args"]))
    env = verify_env(MATCHAPACK_VERIFY_NOISE="1", MATCHAPACK_LOG_DIR=os.path.join(work, "logs"), MATCHAPACK_VERIFY_ARGS_FILE=argsfile)
    p = subprocess.run([os.path.join(here, "scripts/verify.sh"), src, os.path.join(work, "v")], env=env, stdin=subprocess.DEVNULL,
                       capture_output=True, text=True, errors="replace")
    show_npm(p.stdout + p.stderr)
    rec = npm_result(p.stdout + p.stderr)
    rec.update(npm=plan["npm"], args=plan["args"], manifests_copied=copied)
    return rec


# --- both ---------------------------------------------------------------------

def main_engine(engine, logdir, n, here, dirs):
    log = os.path.join(logdir, "log.jsonl")
    vlog = os.path.join(logdir, "verify.jsonl")
    if engine == "npm" and dirs:
        # Directories given: replayed as npm ci, as the sampler always did.
        picked = [({"command": "ci", "outcome": "installed", "root": os.path.realpath(d)}, os.path.realpath(d)) for d in dirs]
    else:
        picked = pick_dirs(log, dirs, engine) if dirs else pick(log, n, engine)
    if not picked:
        what = "`matchapack yarn` runs" if engine == "yarn1" else "matchapack installs"
        print("nothing to verify: no replayable %s in %s yet" % (what, log))
        return 0
    m = re.search(r"\((.*)\)", subprocess.run([os.path.join(here, "target/release/matchapack"), "--version"], capture_output=True, text=True).stdout)
    build = m.group(1) if m else "unknown"
    keep = os.environ.get("MATCHAPACK_VERIFY_KEEP") == "1"
    for r, d in picked:
        kind = kind_of(r) if r else None
        command = (r.get("command") or ("install" if engine == "yarn1" else "ci")) if r else None
        print("######## %s %s %s" % ("yarn" if engine == "yarn1" else "npm", command or "", d))
        base = {"build": build, "engine": engine, "kind": kind, "command": command, "dir": d, "logged_ts": r.get("ts") if r else None}
        plan, why = (yarn_plan if engine == "yarn1" else npm_plan)(r, d)
        if why:
            append(vlog, dict(base, result="SKIPPED", reason=why))
            print("recorded: SKIPPED (%s)" % why)
            continue
        work = tempfile.mkdtemp(prefix="matchapack-verify-%s." % ("yarn" if engine == "yarn1" else "npm"), dir=os.environ.get("TMPDIR") or None)
        try:
            if engine == "yarn1":
                rec = yarn_one(plan, here, work)
            elif plan["kind"] == "full":
                rec = npm_ci_one(d, here, work)
            else:
                rec = npm_one(plan, here, work)
        finally:
            if keep:
                print("  kept: %s" % work)
            else:
                # The copies go; the comparison is in the record.
                shutil.rmtree(work, ignore_errors=True)
        rec = dict(base, **rec)
        append(vlog, rec)
        print("recorded: %s%s" % (rec["result"], " (%s)" % rec["reason"] if rec.get("reason") else ""))
        for l in rec.get("differing", [])[:10]:
            print("    " + l)
        if rec["result"] == "ERROR":
            print("\n".join("    " + l for l in rec.get("detail", [])))
    return 0


def main(argv):
    if argv[:1] in (["npm"], ["yarn"]) and len(argv) >= 4:
        return main_engine("yarn1" if argv[0] == "yarn" else "npm", argv[1], int(argv[2]), argv[3], argv[4:])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
