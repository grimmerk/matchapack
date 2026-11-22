#!/usr/bin/env python3
"""Offline tests of scripts/bench.py's own logic (no npm, no Yarn, no
network): the JSONL tail repair, the work directory's marker, the store
markers, resume safety, the isolation-check report, the per-entry
sentinel, the exit status from recorded failures, the end of a run however
it stops (a signal during an install, a setup error, a complete run), the
process-group clean-up on a timeout, and the machine-wide lock bench.sh
takes.
    python3 scripts/test-bench.py
Exit 0 when every check passes."""
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import types

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("bench", os.path.join(HERE, "bench.py"))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

failed = 0


def check(name, ok, detail=""):
    global failed
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": {detail}"))
    if not ok:
        failed += 1


tmp = tempfile.mkdtemp(prefix="test-bench-")
try:
    # --- the torn JSONL tail ---------------------------------------------
    p = os.path.join(tmp, "runs.jsonl")
    with open(p, "w") as f:
        f.write('{"a": 1}\n{"b": 2}\n')
    check("repair_tail: complete lines are left alone", bench.repair_tail(p) == 0 and open(p).read() == '{"a": 1}\n{"b": 2}\n')
    with open(p, "a") as f:
        f.write('{"c": 3, "tor')
    cut = bench.repair_tail(p)
    check("repair_tail: an incomplete last line is cut off", cut == len('{"c": 3, "tor') and open(p).read() == '{"a": 1}\n{"b": 2}\n',
          repr(open(p).read()))
    with open(p, "a") as f:
        f.write('{"d": 4, "tor')
    bench.append_line(p, {"e": 5})
    recs = bench.read_lines(p)
    check("append_line after a torn write: the new record is readable", recs == [{"a": 1}, {"b": 2}, {"e": 5}], repr(recs))
    q = os.path.join(tmp, "one.jsonl")
    with open(q, "w") as f:
        f.write('{"only": "tor')
    bench.repair_tail(q)
    check("repair_tail: a file of one incomplete line becomes empty", open(q).read() == "")
    big = os.path.join(tmp, "big.jsonl")
    with open(big, "w") as f:
        f.write('{"x": 1}\n' + "y" * 200000)
    bench.repair_tail(big)
    check("repair_tail: an incomplete line longer than a block", open(big).read() == '{"x": 1}\n')
    check("repair_tail: a missing file", bench.repair_tail(os.path.join(tmp, "nope.jsonl")) == 0)

    # --- the work directory's marker -----------------------------------------
    wd = os.path.join(tmp, "w")
    check("work_state: missing", bench.work_state(wd) == "missing")
    os.makedirs(wd)
    check("work_state: empty", bench.work_state(wd) == "empty")
    open(os.path.join(wd, "src"), "w").close()
    check("work_state: a directory with other files is foreign", bench.work_state(wd) == "foreign")
    open(os.path.join(wd, bench.MARKER), "w").close()
    check("work_state: marked", bench.work_state(wd) == "owned")
    f2 = os.path.join(tmp, "a-file")
    open(f2, "w").close()
    check("work_state: a file is not a directory", bench.work_state(f2) == "not-dir")
    ln = os.path.join(tmp, "link")
    os.symlink(wd, ln)
    check("work_state: a symlink is not taken as a directory", bench.work_state(ln) == "not-dir")
    check("nearest_existing", bench.nearest_existing(os.path.join(wd, "x", "y")) == wd)

    # --- store markers ---------------------------------------------------------
    st = os.path.join(tmp, "warm")
    facts = {"npm": "11.19.1", "matchapack": "matchapack 0.1.0 (abc)", "commit": "c1"}
    check("store_ready: no store", not bench.store_ready(st, facts))
    os.makedirs(os.path.join(st, "v2"))
    check("store_ready: a store without its marker (a fill that did not finish)", not bench.store_ready(st, facts))
    with open(st + ".ready", "w") as f:
        json.dump(facts, f)
    check("store_ready: filled and marked", bench.store_ready(st, facts))
    check("store_ready: filled by another npm", not bench.store_ready(st, dict(facts, npm="11.20.0")))
    check("store_ready: for another commit", not bench.store_ready(st, dict(facts, commit="c2")))

    # --- resume safety -------------------------------------------------------------
    args = {"rounds": 10, "disk_rounds": 1, "load_max": 3.0, "load_wait": 60.0, "settle": 10.0, "timeout": 1800.0,
            "fill_npm": "11.10.1", "smoke": False, "projects": ["small"]}
    mach = {"model": "M", "cpu": "C", "macos": "27", "filesystem": "APFS", "node": "v24", "npm": "11.19.1", "yarn": "1.22.22",
            "matchapack": "matchapack 0.1.0 (abc-dirty)", "matchapack_sha256": "m1", "bench_files_sha256": "f1",
            "bench_commit": "b1", "bench_dirty": True}
    first = {"machine": mach, "args": args}
    check("resume_conflicts: the same tools and settings", bench.resume_conflicts(first, dict(mach), dict(args)) == [])
    check("resume_conflicts: another scope (projects) may resume", bench.resume_conflicts(first, dict(mach), dict(args, projects=["medium"])) == [])
    d = bench.resume_conflicts(first, dict(mach, npm="11.20.0"), dict(args))
    check("resume_conflicts: another npm", len(d) == 1 and d[0].startswith("npm "), repr(d))
    d = bench.resume_conflicts(first, dict(mach), dict(args, rounds=5, fill_npm="11.9.0"))
    check("resume_conflicts: other rounds and fill npm", len(d) == 2, repr(d))
    d = bench.resume_conflicts(first, dict(mach, bench_files_sha256="f2"), dict(args))
    check("resume_conflicts: other bench.py / bench.sh contents (a dirty checkout edited since)", len(d) == 1, repr(d))
    d = bench.resume_conflicts(first, dict(mach, matchapack_sha256="m2"), dict(args))
    check("resume_conflicts: another matchapack binary under the same version string", len(d) == 1, repr(d))
    d = bench.resume_conflicts(first, dict(mach, bench_commit="b2", bench_dirty=False), dict(args))
    check("resume_conflicts: a commit that changes neither file nor binary may resume", d == [], repr(d))
    fa, fb = os.path.join(tmp, "fa"), os.path.join(tmp, "fb")
    with open(fa, "w") as f:
        f.write("ab")
    with open(fb, "w") as f:
        f.write("c")
    h1 = bench.files_sha256([fa, fb])
    with open(fa, "w") as f:
        f.write("a")
    with open(fb, "w") as f:
        f.write("bc")
    check("files_sha256: bytes moved from one file to the next change it", bench.files_sha256([fa, fb]) != h1)

    # --- the isolation-check report --------------------------------------------------
    s = {"~/.npm": {"digest": "x", "entries": 3, "depth": 4}}
    t = bench.isolation_text([{"when": "start", "state": s}])
    check("isolation_text: a start without an end did not complete", "did not complete" in t and "unchanged" not in t, t)
    t = bench.isolation_text([{"when": "start"}, {"when": "end", "state": s, "changed": []}])
    check("isolation_text: unchanged, saying how it was checked", "unchanged" in t and "4 levels" in t, t)
    t = bench.isolation_text([{"when": "start"}, {"when": "end", "state": s, "changed": ["~/.npm: …"]}])
    check("isolation_text: changed", "CHANGED" in t, t)
    t = bench.isolation_text([{"when": "start"}, {"when": "end", "state": s, "changed": []}, {"when": "start"}])
    check("isolation_text: the latest invocation counts", "did not complete" in t, t)

    # --- the per-entry sentinel ---------------------------------------------------------
    root = os.path.join(tmp, "cache")
    os.makedirs(os.path.join(root, "a", "b"))
    for n in ("x", "y"):
        with open(os.path.join(root, "a", n), "w") as f:
            f.write("1234")
    future = time.time() + 86400
    os.utime(os.path.join(root, "a", "x"), (future, future))
    s0 = bench.tree_state(root, 3)
    yp = os.path.join(root, "a", "y")
    old = os.stat(yp).st_mtime
    with open(yp, "w") as f:
        f.write("123456")
    os.utime(yp, (old, old + 1))
    s1 = bench.tree_state(root, 3)
    check("tree_state: an in-place rewrite under a newer (future) file is seen", s0["digest"] != s1["digest"]
          and s0["path"] == s1["path"] and s0["mtime"] == s1["mtime"])
    check("tree_state: unchanged is equal", bench.tree_state(root, 3)["digest"] == s1["digest"])
    ch = bench.sentinel_changes({"c": dict(s0)}, {"c": dict(s1)}, time.time())
    check("sentinel_changes: reported", len(ch) == 1 and "changed within 3 levels" in ch[0], repr(ch))

    # --- the exit status from recorded failures -----------------------------------------------
    recs = [{"bench_id": "b", "exit": 0}, {"bench_id": "b", "exit": "timeout"}, {"bench_id": "b", "exit": 1}, {"bench_id": "c", "exit": 1}]
    check("recorded_failures: every failed attempt of the bench id, earlier invocations included", bench.recorded_failures(recs, "b") == 2)

    # --- the end of a run, however it stops ------------------------------------------------------
    # run_projects and finish as main() calls them, with a stand-in for
    # the projects and for the user's real state (sentinels()).
    def run_stub(stop, records=()):
        wd2 = tempfile.mkdtemp(dir=tmp)
        os.makedirs(os.path.join(wd2, "results"))
        for r in records:
            bench.append_line(os.path.join(wd2, "results", "runs.jsonl"), r)
        # main() records the machine and the start of the isolation check
        # before the projects.
        bench.append_line(os.path.join(wd2, "results", "meta.jsonl"),
                          {"bench_id": "t", "machine": mach, "args": args, "top_cpu": ["%CPU COMM"]})
        bench.append_line(os.path.join(wd2, "results", "sentinels.jsonl"), {"bench_id": "t", "when": "start", "ts": time.time()})
        w2 = bench.Work(wd2)
        a2 = types.SimpleNamespace(projects=["small"], bench_id="t", fill_npm="11.10.1", smoke=False)

        class Stand:
            def npm_project(self, name, spec):
                stop()

            yarn_project = npm_project

        state = lambda: {"~/.npm": {"digest": "d", "entries": 1, "depth": 4, "mtime": 0, "path": "x"}}
        stopped = bench.run_projects(Stand(), w2, a2, time.monotonic())
        code = bench.finish(w2, a2, state(), time.time(), time.monotonic(), stopped, state=state)
        ends = [r for r in bench.read_lines(os.path.join(wd2, "results", "sentinels.jsonl")) if r.get("when") == "end"]
        summary_md = os.path.join(wd2, "results", "summary.md")
        text = open(summary_md).read() if os.path.exists(summary_md) else ""
        return code, ends, text

    def install_stopped_by_sigterm():
        # The real path: SIGTERM reaches this process while an install
        # runs; on_signal turns it into KeyboardInterrupt inside run().
        bench.run([sys.executable, "-c", "import os, signal, time; os.kill(os.getppid(), signal.SIGTERM); time.sleep(30)"],
                  tmp, dict(os.environ), check=False)

    old_term = signal.signal(signal.SIGTERM, bench.on_signal)
    try:
        code, ends, text = run_stub(install_stopped_by_sigterm)
    finally:
        signal.signal(signal.SIGTERM, old_term)
    check("a signal during an install: exit 130, the end check recorded, the summary written",
          code == 130 and len(ends) == 1 and "unchanged" in text, f"code={code} ends={len(ends)}")
    code, ends, text = run_stub(lambda: bench.die("a setup step failed", 7))
    check("a setup error: its status, the end check recorded, the summary written",
          code == 7 and len(ends) == 1 and "unchanged" in text, f"code={code} ends={len(ends)}")
    code, ends, _ = run_stub(lambda: None)
    check("a complete run with no failure: exit 0", code == 0 and len(ends) == 1, f"code={code}")
    failed_run = {"bench_id": "t", "project": "small", "case": "npm-fresh-worktree", "round": 1, "side": "plain", "order": 1,
                  "exit": "timeout", "wall_s": 1.0, "cpu_user_s": 0.5, "cpu_sys_s": 0.5, "load_flag": False,
                  "noise": {"spotlight_pcpu": 0.0, "vm_pcpu": 0.0, "top": []}, "cmd": ["npm", "ci", "--offline"]}
    code, _, _ = run_stub(lambda: None, records=[failed_run])
    check("a complete run with a recorded failure: exit 1", code == 1, f"code={code}")

    # --- the process group after a timeout -----------------------------------------------------
    # The parent ends on SIGTERM; a worker it started ignores SIGTERM. The
    # clean-up must SIGKILL the worker and reap the parent.
    code = ("import os, signal, subprocess, sys, time\n"
            "subprocess.Popen([sys.executable, '-c', 'import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)'])\n"
            "time.sleep(60)\n")
    p = subprocess.Popen([sys.executable, "-c", code], start_new_session=True)
    time.sleep(1.0)
    t0 = time.monotonic()
    bench.kill_group(p, grace=2)
    gone = not bench.group_alive(p.pid)
    check("kill_group: a worker that ignores SIGTERM is killed, the child reaped", gone and p.returncode is not None,
          f"alive={not gone} rc={p.returncode}")
    check("kill_group: within the grace period plus the kill", time.monotonic() - t0 < 15)

    # --- the machine-wide lock (the commands bench.sh runs) -------------------------------------
    lock = os.path.join(tmp, "bench.lock")
    take = f'exec 9>>"{lock}"; python3 -I -c "import fcntl; fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)" || exit 3'
    # `exec sleep`: the holder is one process (a child that inherits fd 9,
    # as bench.py does in bench.sh, keeps the lock while it runs).
    holder = subprocess.Popen(["bash", "-c", take + "; echo held; exec sleep 30"], stdout=subprocess.PIPE, text=True)
    first_line = holder.stdout.readline().strip()
    second = subprocess.run(["bash", "-c", take], capture_output=True)
    check("lock: held by the first launcher after its python check exited", first_line == "held" and second.returncode == 3,
          f"{first_line!r} {second.returncode}")
    os.kill(holder.pid, signal.SIGKILL)
    holder.wait()
    third = subprocess.run(["bash", "-c", take], capture_output=True)
    check("lock: free again once the launcher is gone (SIGKILL)", third.returncode == 0, str(third.returncode))
finally:
    shutil.rmtree(tmp, ignore_errors=True)

print(f"== {'all passed' if not failed else f'{failed} failed'}")
sys.exit(1 if failed else 0)
