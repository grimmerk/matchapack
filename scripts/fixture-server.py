#!/usr/bin/env python3
"""The fixtures' tarball server for one test run (#66).

    fixture-server.py <fixtures> <copy> <ready-file> <parent-pid>

The fixtures name their tarballs by the placeholder host 127.0.0.1:48321
(package-lock.json, yarn.lock, package.json, .yarnrc, the registry documents
next to the tarballs, and the package.json inside five tarballs). This
server takes a free port (bind to port 0), writes a copy of <fixtures> to
<copy> with every 127.0.0.1:48321 replaced by 127.0.0.1:<port>, and serves
<copy>/optional-upward/tarballs on that port. Several runs (worktrees) then
each have their own port and their own tarballs.

Ephemeral ports have five digits (macOS 49152-65535, Linux 32768-60999), so
the replacement keeps every length: inside a tarball only file contents
change, never a tar header. The rewritten tarballs get new hashes; the copy
carries them wherever the old ones were written (lockfile integrity, the
yarn.lock #sha1 fragment, the registry documents' shasum).

Identity: the server answers GET /.matchapack-server-id with a random token
that it also writes to <copy>/.matchapack-server-id. A suite compares the
two before using the server (scripts/fixture-server.sh), so a server that
serves anything but this copy is refused, loudly.

<ready-file> gets the port once the copy is written and the server is about
to answer. The server stops by itself when <parent-pid> is gone (the suite
that started it), whatever way the suite ended.
"""
import base64
import functools
import gzip
import hashlib
import http.server
import io
import os
import secrets
import shutil
import sys
import tarfile
import threading
import time

PLACEHOLDER = b"127.0.0.1:48321"


def digests(data):
    out = []
    for algo in ("sha512", "sha1"):
        h = hashlib.new(algo, data)
        out.append(f"{algo}-{base64.b64encode(h.digest()).decode()}".encode())
        out.append(h.hexdigest().encode())
    return out


def rewrite(src, dst, port):
    new = b"127.0.0.1:%d" % port
    assert len(new) == len(PLACEHOLDER), port
    if os.path.lexists(dst):
        shutil.rmtree(dst)
    shutil.copytree(src, dst, symlinks=True)  # copy2: modes and times kept
    files = []
    for root, _dirs, names in os.walk(dst):
        files += [os.path.join(root, n) for n in names if not os.path.islink(os.path.join(root, n))]
    # The tarballs (gzip, whatever their name) first: their new hashes go
    # into the other files.
    gz = [p for p in files if open(p, "rb").read(2) == b"\x1f\x8b"]
    hashes = []
    for p in gz:
        raw = open(p, "rb").read()
        tar = gzip.decompress(raw)
        if PLACEHOLDER not in tar:
            continue
        out = gzip.compress(tar.replace(PLACEHOLDER, new), mtime=0)
        # Same lengths, so the headers are untouched; a header that held the
        # placeholder would fail its checksum here.
        names = [m.name for m in tarfile.open(fileobj=io.BytesIO(out)).getmembers()]
        assert names == [m.name for m in tarfile.open(fileobj=io.BytesIO(raw)).getmembers()], p
        put(p, out)
        hashes += zip(digests(raw), digests(out))
    for p in files:
        if p in gz:
            continue
        data = open(p, "rb").read()
        out = data.replace(PLACEHOLDER, new)
        for old, h in hashes:
            out = out.replace(old, h)
        if out != data:
            put(p, out)
    # Nothing in the copy may still name the placeholder: a request to it
    # would reach whatever answers there (another run's server, or none).
    for p in files:
        data = open(p, "rb").read()
        assert PLACEHOLDER not in (gzip.decompress(data) if p in gz else data), f"{p} still names the placeholder"


def put(p, data):
    st = os.stat(p)
    with open(p, "wb") as f:
        f.write(data)
    os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))


class Handler(http.server.SimpleHTTPRequestHandler):
    token = b""

    def do_GET(self):
        if self.path == "/.matchapack-server-id":
            self.send_response(200)
            self.send_header("Content-Length", str(len(self.token)))
            self.end_headers()
            self.wfile.write(self.token)
            return
        super().do_GET()

    def log_message(self, *args):
        pass


def watch(parent):
    while True:
        time.sleep(0.5)
        try:
            os.kill(parent, 0)
        except ProcessLookupError:
            os._exit(0)
        except PermissionError:
            pass


def main():
    src, dst, ready, parent = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    served = os.path.join(dst, "optional-upward", "tarballs")
    handler = functools.partial(Handler, directory=served)
    for _ in range(100):
        httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        port = httpd.server_address[1]
        if 10000 <= port <= 65535:
            break
        httpd.server_close()
    else:
        sys.exit("no five-digit free port")
    threading.Thread(target=watch, args=(parent,), daemon=True).start()
    rewrite(src, dst, port)
    Handler.token = secrets.token_hex(16).encode()
    with open(os.path.join(dst, ".matchapack-server-id"), "wb") as f:
        f.write(Handler.token)
    with open(ready + ".tmp", "w") as f:
        f.write(f"{port}\n")
    os.rename(ready + ".tmp", ready)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
