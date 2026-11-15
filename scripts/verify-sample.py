#!/usr/bin/env python3
"""The picking, replaying and recording of scripts/verify-sample.sh.

  verify-sample.py pick-npm <log.jsonl> <N>
      the N most recent distinct projects `matchapack npm ci` installed
  verify-sample.py record-npm <verify.jsonl> <dir> <verify.sh output> <build>
      one line for an npm sample (verify.sh's output parsed)
  verify-sample.py yarn <log dir> <N> <checkout> [project-dir...]
      the Yarn 1 samples: pick, replay, compare, record

One log (`log.jsonl`) to pick from and one file (`verify.jsonl`) to record
in, for both engines; `matchapack report` shows both (src/trial.rs). A Yarn
sample replays a logged full install (`yarn`, `yarn install`, with or
without --frozen-lockfile) of one project: on copies of its manifests, plain
Yarn and `matchapack yarn` with the same Yarn, node, cache folder and flags
(scripts/verify-yarn1.sh --engine yarn1), and the trees compared with the
harness's rules. NOTES "Sampled verification of Yarn 1 installs".
"""
import fcntl, json, os, re, shutil, subprocess, sys, tempfile, time


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


# --- npm ---------------------------------------------------------------------

def pick_npm(path, n):
    seen, out = set(), []
    for r in reversed(read_log(path)):
        # `matchapack yarn` runs are Yarn's (`--yarn`): a record of one from
        # before Yarn logged its command has no `command` either.
        if r.get("engine") == "yarn1":
            continue
        # Replayed as npm ci: only npm ci runs (records without a command are
        # from before the install family was routed). `npm install` in a
        # workspace member or with --prefix installs elsewhere (`root`).
        if r.get("command", "ci") != "ci":
            continue
        d = r.get("root") or r.get("dir")
        if r.get("outcome") == "installed" and d and d not in seen and os.path.isdir(d):
            seen.add(d)
            out.append(d)
        if len(out) >= n:
            break
    print("\n".join(out))


def record_npm(log, d, outfile, build):
    out = open(outfile, errors="replace").read()
    m = re.search(r"RESULT: (\w+)", out)
    t = re.search(r"npm ci: ([\d.]+)s\s+matchapack: ([\d.]+)s", out)
    n = re.search(r"noise .*?: (\d+) paths", out)
    ls = re.search(r"npm ls: (.*)", out)
    result = m.group(1) if m else "ERROR"
    # Differences only OUTSIDE node_modules come from the project's own build
    # scripts (e.g. tsdown output that varies run to run, which two npm ci runs may
    # happen to agree on): reported, but kept apart from a node_modules mismatch.
    paths = re.findall(r"^  ([^ :]+)(?::|$)", out.split("== diff A B")[-1].split("== mtime")[0], re.M)
    if result == "DIFFERENT" and paths and all(not (p.startswith("node_modules/") or "/node_modules/" in p) for p in paths):
        result = "BUILD-OUTPUT-ONLY"
    rec = {"build": build, "engine": "npm", "dir": d, "result": result, "differing": paths[:20],
           "npm_ci_secs": float(t.group(1)) if t else None, "matchapack_secs": float(t.group(2)) if t else None,
           "noise_paths": int(n.group(1)) if n else None, "npm_ls": ls.group(1).strip() if ls else None}
    append(log, rec)
    print("recorded:", rec["result"])
    if rec["result"] == "ERROR":
        print("\n".join("    " + l for l in out.strip().splitlines()[-15:]))


# --- Yarn 1 ------------------------------------------------------------------

# The files a copy holds (every one outside node_modules and .git): the
# manifests Yarn reads to install, and the rc files it reads in the project.
COPIED = ("package.json", "yarn.lock", ".yarnrc", ".npmrc")


def full_install(r):
    """A logged full install of `matchapack yarn` (records from before Yarn
    logged its command are installs: nothing else was routed then)."""
    return r.get("engine") == "yarn1" and r.get("outcome") == "installed" and r.get("command", "install") == "install"


def project_of(r):
    return r.get("root") or r.get("dir")


def pick_yarn(path, n, dirs):
    """[(record or None, project dir)]: the newest full install of each of the
    N most recent distinct projects, or of each directory given."""
    full = [r for r in read_log(path) if full_install(r) and project_of(r)]
    full.reverse()
    if dirs:
        out = []
        for d in dirs:
            real = os.path.realpath(d)
            hit = next((r for r in full if real in (os.path.realpath(project_of(r)), os.path.realpath(r.get("dir") or ""))), None)
            out.append((hit, project_of(hit) if hit else real))
        return out
    seen, out = set(), []
    for r in full:
        d = project_of(r)
        if d in seen:
            continue
        seen.add(d)
        out.append((r, d))
        if len(out) >= n:
            break
    return out


def stat_state(p):
    try:
        st = os.stat(p)
    except OSError:
        return None
    return [st.st_size, st.st_mtime_ns]


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


def replay_plan(r, d):
    """(plan, None) when the logged run can be replayed, else (None, why)."""
    if r is None:
        return None, "no `matchapack yarn` full install of this directory in the log"
    y = r.get("yarn1") or {}
    root, cwd = r.get("root"), r.get("dir")
    need = {"root": root, "yarn1.yarn_js": y.get("yarn_js"), "yarn1.node_path": y.get("node_path"), "yarn1.node": y.get("node"),
            "manifests": r.get("manifests"), "replay": r.get("replay")}
    missing = [k for k, v in need.items() if not v]
    if missing:
        return None, "the log record has no %s (logged by a matchapack build from before replays were recorded)" % ", ".join(missing)
    if not os.path.isdir(root):
        return None, "the project directory is gone"
    if not cwd or not os.path.isdir(cwd):
        return None, "the directory the install ran in is gone (%s)" % cwd
    sub = os.path.relpath(os.path.realpath(cwd), os.path.realpath(root))
    if sub == ".." or sub.startswith("../"):
        return None, "the install ran outside its project (%s)" % cwd
    for name in ("yarn.lock", "package.json"):
        if stat_state(os.path.join(root, name)) != r["manifests"].get(name):
            return None, "%s changed since the logged run" % name
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
    cache_root = os.path.dirname(cache)
    if not os.path.isdir(cache):
        return None, "Yarn's cache folder is gone (%s)" % cache
    args = r["replay"].get("args")
    if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
        return None, "the logged arguments are not a list of words"
    if any("***" in a for a in args):
        return None, "an argument was hidden in the log (a secret): the run cannot be repeated"
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
    return {"root": root, "sub": sub, "node": node, "node_version": nv, "yarn_js": yarn_js, "yarn": version,
            "cache_root": cache_root, "args": args, "logged_cloned": y.get("cloned")}, None


def copy_manifests(root, dest, sub):
    """The project's manifests and rc files (COPIED), outside node_modules
    and .git, into dest, with their times and modes; and the directory the
    install ran in. Returns how many files."""
    n = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(x for x in dirnames if x not in ("node_modules", ".git"))
        rel = os.path.relpath(dirpath, root)
        for f in filenames:
            if f not in COPIED:
                continue
            s = os.path.join(dirpath, f)
            if os.path.islink(s) or not os.path.isfile(s):
                continue
            os.makedirs(os.path.join(dest, rel), exist_ok=True)
            shutil.copy2(s, os.path.join(dest, rel, f))
            n += 1
    os.makedirs(os.path.join(dest, sub), exist_ok=True)
    return n


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
    why = next((l for l in reversed(lines) if l.startswith(("engine yarn failed", "engine: "))), lines[-1] if lines else "no output")
    return "ERROR", "verify-yarn1.sh exit %d: %s" % (rc, why.rstrip(":")), extra


def yarn_main(logdir, n, here, dirs):
    log = os.path.join(logdir, "log.jsonl")
    vlog = os.path.join(logdir, "verify.jsonl")
    picked = pick_yarn(log, n, dirs)
    if not picked:
        print("nothing to verify: no `matchapack yarn` full installs in %s yet" % log)
        return 0
    bin_ = os.path.join(here, "target/release/matchapack")
    m = re.search(r"\((.*)\)", subprocess.run([bin_, "--version"], capture_output=True, text=True).stdout)
    build = m.group(1) if m else "unknown"
    keep = os.environ.get("MATCHAPACK_VERIFY_KEEP") == "1"
    for r, d in picked:
        print("######## yarn %s" % d)
        base = {"build": build, "engine": "yarn1", "command": "install", "dir": d, "logged_ts": r.get("ts") if r else None}
        plan, why = replay_plan(r, d)
        if why:
            append(vlog, dict(base, result="SKIPPED", reason=why))
            print("recorded: SKIPPED (%s)" % why)
            continue
        work = tempfile.mkdtemp(prefix="matchapack-verify-yarn.", dir=os.environ.get("TMPDIR") or None)
        try:
            src = os.path.join(work, "src")
            copied = copy_manifests(plan["root"], src, plan["sub"])
            v = os.path.join(work, "v")
            argv = [os.path.join(here, "scripts/verify-yarn1.sh"), "--work", v, "--cache-dir", plan["cache_root"], "--engine", "yarn1",
                    "--engine-min-clones", "any", "--args-as-given"]
            if plan["sub"] != ".":
                argv += ["--subdir", plan["sub"]]
            argv += [src] + plan["args"]
            env = dict(os.environ, MATCHAPACK_YARN_NODE=plan["node"], MATCHAPACK_YARN_JS=plan["yarn_js"])
            p = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace")
            out = p.stdout + p.stderr
            open(os.path.join(work, "out.txt"), "w").write(out)
            for l in out.splitlines():
                if l.startswith(("yarn ", "engine: ", "plain vs ", "engine tree damaged")):
                    print("  " + l)
            result, reason, extra = yarn_result(p.returncode, out, v, here, plan["node_version"])
            cloned = None
            try:
                cloned = (json.load(open(os.path.join(v, "engine.json"))).get("yarn1") or {}).get("cloned")
            except (OSError, ValueError):
                pass
            rec = dict(base, result=result, yarn=plan["yarn"], node=plan["node_version"], args=plan["args"], subdir=plan["sub"],
                       manifests_copied=copied, yarn_secs=window_secs(os.path.join(v, "runs"), "plain1"),
                       matchapack_secs=window_secs(os.path.join(v, "runs"), "engine"), cloned=cloned, logged_cloned=plan["logged_cloned"])
            if reason:
                rec["reason"] = reason
            rec.update(extra)
            append(vlog, rec)
            print("recorded: %s%s" % (result, " (%s)" % reason if reason else ""))
            for l in extra.get("differing", [])[:10]:
                print("    " + l)
            if result == "ERROR":
                print("\n".join("    " + l for l in extra.get("detail", [])))
        finally:
            if keep:
                print("  kept: %s" % work)
            else:
                shutil.rmtree(work, ignore_errors=True)
    return 0


def main(argv):
    if argv[:1] == ["pick-npm"]:
        pick_npm(argv[1], int(argv[2]))
        return 0
    if argv[:1] == ["record-npm"]:
        record_npm(*argv[1:5])
        return 0
    if argv[:1] == ["yarn"]:
        return yarn_main(argv[1], int(argv[2]), argv[3], argv[4:])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
