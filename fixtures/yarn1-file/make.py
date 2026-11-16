#!/usr/bin/env python3
"""The local package of the Yarn 1 `file:` fixtures, written into a copy.

  python3 -I make.py tgz <project-dir>   writes <project-dir>/local-1.0.0.tgz
  python3 -I make.py dir <project-dir>   writes <project-dir>/local-dir/

One package, `local-tgz` or `local-dir` 1.0.0: package.json with a bin,
index.js, README.md, nested files, an executable bin/cli.js, two empty
files, and 300 padding files (more than the 128 first copies of Yarn's link
step, which the engine leaves to Node, so some files must be clones).
Fixed times, owner and order, a gzip header without a name or time; but the
compressed bytes depend on the zlib Python uses (CI's Python wrote another
sha1 than this machine's), so the tarball is kept in the repository
(tgz/local-1.0.0.tgz, the sha1 in tgz/yarn.lock) and the test copies it;
`make.py tgz` only regenerates it. After a change: write it again, then
generate yarn.lock again with plain Yarn and a scratch cache of its own
(Yarn's --frozen-lockfile checks the sha1, "Integrity check failed").
"""
import gzip, io, json, os, sys, tarfile


def pkg_files(name, version, pad):
    """{relative path: (content, mode)} of the package."""
    files = {
        "package.json": (json.dumps({
            "name": name, "version": version, "main": "index.js",
            "bin": {name + "-cli": "bin/cli.js"},
        }, indent=2) + "\n", 0o644),
        "index.js": (f"module.exports = '{name}@{version}'\n", 0o644),
        "README.md": (f"# {name}\n", 0o644),
        "lib/nested/deep/data.txt": (f"{name} nested data\n", 0o644),
        "lib/util.js": ("exports.x = 1\n", 0o644),
        "bin/cli.js": ("#!/usr/bin/env node\nconsole.log(require('..'))\n", 0o755),
        "empty.txt": ("", 0o644),
        "lib/nested/empty-too": ("", 0o644),
    }
    for i in range(pad):
        files[f"pad/d{i % 10}/f{i:03d}.txt"] = (f"{name} pad file {i}\n" * (1 + i % 7), 0o644)
    return files


def write_dir(root, files):
    for rel, (data, mode) in files.items():
        p = os.path.join(root, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(data)
        os.chmod(p, mode)


def write_tgz(path, files):
    """An npm-style tarball (entries under package/), fixed times and order."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for rel in sorted(files):
            data, mode = files[rel]
            b = data.encode()
            ti = tarfile.TarInfo("package/" + rel)
            ti.size, ti.mode, ti.mtime = len(b), mode, 499162500
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = ""
            t.addfile(ti, io.BytesIO(b))
    with open(path, "wb") as f:
        with gzip.GzipFile(fileobj=f, mode="wb", mtime=0, filename="") as g:
            g.write(raw.getvalue())


def main():
    kind, out = sys.argv[1], sys.argv[2]
    if kind == "tgz":
        write_tgz(os.path.join(out, "local-1.0.0.tgz"), pkg_files("local-tgz", "1.0.0", 300))
    elif kind == "dir":
        write_dir(os.path.join(out, "local-dir"), pkg_files("local-dir", "1.0.0", 300))
    else:
        sys.exit("usage: make.py tgz|dir <project-dir>")


main()
