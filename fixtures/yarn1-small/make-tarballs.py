#!/usr/bin/env python3
"""Write the tarballs of the Yarn 1 fixtures (yarn1-small, yarn1-emptyfirst,
yarn1-scriptpkg, yarn1-many, yarn1-workspaces, yarn1-rootpath; y1-rootpath
also for the npm fixture rootpath) into
fixtures/optional-upward/tarballs, where the local tarball server
(127.0.0.1:48321) serves from, so that the fixtures install without a
network.

  y1-a 1.0.0      a bin (bin/cli.js, 0755), nested directories, and two
                  dependencies: y1-b 1.0.0 (nested under y1-a, since the root
                  wants y1-b 2.0.0) and y1-c 1.0.0 (hoisted)
  y1-b 1.0.0/2.0.0, y1-c 1.0.0, @y1/scoped 1.0.0 (a scoped name)
  y1-empty 1.0.0  package.json, index.js, 40 small files and 400 EMPTY files:
                  under Node >= 22 the copy of an empty file keeps the source's
                  times, which flips Yarn's time probe (NOTES "Yarn 1
                  harness")
  y1-script 1.0.0 an install script that writes built.txt
  y1-execpath 1.0.0  an install script that records $npm_execpath and runs
                  `node "$npm_execpath" --version` (execpath.txt)
  y1-rootpath 1.0.0  an install script that writes built/<its own absolute
                  directory>/marker: a path named after the tree, as
                  node-gyp's .deps/ (issue grimmerk/matchapack-private#48)
  y1-many 1.0.0/2.0.0  300 files and a bin: more than the 128 first copies
                  of Yarn's link step, which the Yarn 1 engine leaves to Node

No package has an empty file except y1-empty. Deterministic: fixed times,
owner, order, gzip header without a name or time. Re-run after a change, then
regenerate each fixture's yarn.lock (see its README.md).
"""
import gzip, io, json, os, tarfile

here = os.path.dirname(os.path.abspath(__file__))
out = os.path.join(here, "..", "optional-upward", "tarballs")
URL = "http://127.0.0.1:48321/"


def tarball(file, entries):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for path, mode, data in entries:
            info = tarfile.TarInfo("package/" + path)
            info.size, info.mode, info.mtime = len(data), mode, 1700000000
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            t.addfile(info, io.BytesIO(data))
    gz = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=gz, mtime=0) as g:
        g.write(raw.getvalue())
    with open(os.path.join(out, file), "wb") as f:
        f.write(gz.getvalue())
    print(file)


def manifest(**m):
    return (json.dumps(m, indent=2) + "\n").encode()


tarball("y1-a-1.0.0.tgz", [
    ("package.json", 0o644, manifest(
        name="y1-a", version="1.0.0", main="lib/index.js", bin={"y1a": "bin/cli.js"},
        dependencies={"y1-b": URL + "y1-b-1.0.0.tgz", "y1-c": URL + "y1-c-1.0.0.tgz"})),
    ("bin/cli.js", 0o755, b"#!/usr/bin/env node\nconsole.log(require('../lib/index.js'))\n"),
    ("lib/index.js", 0o644, b"module.exports = require('./sub/deep.js') + require('y1-b')\n"),
    ("lib/sub/deep.js", 0o644, b"module.exports = 'deep'\n"),
    ("README.md", 0o644, b"# y1-a\n"),
])
for v in ("1.0.0", "2.0.0"):
    tarball(f"y1-b-{v}.tgz", [
        ("package.json", 0o644, manifest(name="y1-b", version=v)),
        ("index.js", 0o644, f"module.exports = '{v}'\n".encode()),
    ])
tarball("y1-c-1.0.0.tgz", [
    ("package.json", 0o644, manifest(name="y1-c", version="1.0.0")),
    ("index.js", 0o644, b"module.exports = require('./data/x.json')\n"),
    ("data/x.json", 0o644, b'{"x": 1}\n'),
])
tarball("y1-scoped-1.0.0.tgz", [
    ("package.json", 0o644, manifest(name="@y1/scoped", version="1.0.0")),
    ("index.js", 0o644, b"module.exports = 'scoped'\n"),
])
tarball("y1-empty-1.0.0.tgz", [
    ("package.json", 0o644, manifest(name="y1-empty", version="1.0.0", main="index.js")),
    ("index.js", 0o644, b"module.exports = 1\n"),
] + [(f"data{i:02d}.txt", 0o644, b"x" * (100 + i)) for i in range(40)]
  + [(f"e{i:03d}.empty", 0o644, b"") for i in range(400)])
tarball("y1-script-1.0.0.tgz", [
    ("package.json", 0o644, manifest(
        name="y1-script", version="1.0.0",
        scripts={"install": "node -e \"require('fs').writeFileSync('built.txt','x')\""})),
    ("index.js", 0o644, b"module.exports = 1\n"),
])

# Records what Yarn gives its lifecycle scripts as npm_execpath (Yarn's own
# entry, process.mainModule.filename) and runs it, as tools such as
# node-gyp's callers do: under the Yarn 1 engine it must be Yarn, not the hook.
tarball("y1-execpath-1.0.0.tgz", [
    ("package.json", 0o644, manifest(
        name="y1-execpath", version="1.0.0",
        scripts={"install": "node record.js"})),
    ("record.js", 0o644, b"""const cp = require('child_process')
const p = process.env.npm_execpath
const v = cp.execFileSync(process.execPath, [p, '--version']).toString()
require('fs').writeFileSync('execpath.txt', p + '\\n' + v)
"""),
])


# Names a file after its own directory, as node-gyp's make build does under
# build/Release/.deps/<absolute path>/ (issue grimmerk/matchapack-private#48): its install script writes
# built/<cwd without the leading />/marker, so every tree has another path.
# Used by yarn1-rootpath and by the npm fixture rootpath.
tarball("y1-rootpath-1.0.0.tgz", [
    ("package.json", 0o644, manifest(
        name="y1-rootpath", version="1.0.0",
        scripts={"install": "node build.js"})),
    ("build.js", 0o644, b"""const fs = require('fs'), path = require('path')
const d = path.join('built', process.cwd())
fs.mkdirSync(d, { recursive: true })
fs.writeFileSync(path.join(d, 'marker'), 'x\\n')
"""),
])


def many(v):
    """y1-many: enough files that Yarn's link step goes past its first
    CONCURRENT_QUEUE_ITEMS (128) copies, which the Yarn 1 engine leaves to
    Node: 300 files in 15 directories, a 256 KiB file, a bin. 2.0.0 changes
    some files, drops a directory and adds a file."""
    entries = [
        ("package.json", 0o644, manifest(name="y1-many", version=v, main="index.js", bin={"y1many": "bin/many.js"})),
        ("bin/many.js", 0o755, b"#!/usr/bin/env node\nconsole.log(require('../index.js'))\n"),
        ("index.js", 0o644, f"module.exports = '{v}'\n".encode()),
        ("data/big.bin", 0o644, bytes((i * 7 + 3) % 251 for i in range(256 * 1024))),
    ]
    for d in range(15 if v == "1.0.0" else 14):
        for f in range(20):
            body = f"// {d}/{f}\nmodule.exports = {d * 20 + f}\n".encode() + b"x" * ((d * 20 + f) * 37 % 3000)
            if v != "1.0.0" and f % 7 == 0:
                body += b"// changed in 2.0.0\n"
            entries.append((f"lib/d{d:02d}/f{f:02d}.js", 0o644, body))
    if v != "1.0.0":
        entries.append(("lib/new.js", 0o644, b"module.exports = 'new'\n"))
    return entries


for v in ("1.0.0", "2.0.0"):
    tarball(f"y1-many-{v}.tgz", many(v))
