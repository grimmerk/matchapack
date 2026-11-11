#!/usr/bin/env python3
"""Write the two tarballs of fixtures/odd-tarballs into
fixtures/optional-upward/tarballs (where the local tarball server serves
from). `npm pack` cannot make them: they are what a hand-made or odd
publisher's tarball can hold, and where an extractor other than pacote's can
give another result (found by the Codex review of grimmerk/matchapack-private#34).

  lc-dup     lists package/run.sh twice: first mode 0755, then 0644 with other
             contents. node-tar writes the second into the file the first
             made, so the file keeps the first entry's mode and gets the
             second's contents.
  lc-ignore  has package/.gitignore before package/.npmignore, and a lone
             package/sub/.gitignore. pacote renames a .gitignore to
             .npmignore unless a .npmignore was seen before it in the same
             directory, so the first is renamed, then overwritten by the real
             .npmignore; the lone one is renamed.

Deterministic: fixed times, owner, order, gzip header without a name or time.
Re-run after a change, then update the integrity in package-lock.json.
"""
import base64, gzip, hashlib, io, os, tarfile

here = os.path.dirname(os.path.abspath(__file__))
out = os.path.join(here, "..", "optional-upward", "tarballs")


def tarball(name, entries):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for path, mode, data in entries:
            info = tarfile.TarInfo(path)
            info.size, info.mode, info.mtime = len(data), mode, 1700000000
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            t.addfile(info, io.BytesIO(data))
    gz = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=gz, mtime=0) as g:
        g.write(raw.getvalue())
    data = gz.getvalue()
    with open(os.path.join(out, name), "wb") as f:
        f.write(data)
    sri = "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode()
    print(f"{name} {sri}")


tarball("lc-dup-1.0.0.tgz", [
    ("package/package.json", 0o644, b'{"name":"lc-dup","version":"1.0.0"}\n'),
    ("package/run.sh", 0o755, b"#!/bin/sh\necho first\n"),
    ("package/run.sh", 0o644, b"#!/bin/sh\necho second\n"),
])
tarball("lc-ignore-1.0.0.tgz", [
    ("package/package.json", 0o644, b'{"name":"lc-ignore","version":"1.0.0"}\n'),
    ("package/.gitignore", 0o644, b"from-gitignore\n"),
    ("package/.npmignore", 0o644, b"from-npmignore\n"),
    ("package/sub/.gitignore", 0o644, b"lone-gitignore\n"),
])
