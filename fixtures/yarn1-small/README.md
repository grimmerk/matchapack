# Yarn 1 fixtures

The `yarn1-*` fixtures are Yarn 1 projects
for `scripts/verify-yarn1.sh` and `scripts/test-yarn1.sh`. Every dependency is
a tarball URL on the local tarball server (127.0.0.1:48321, which serves
`fixtures/optional-upward/tarballs`), so they install without a network.
The tarballs are made by `make-tarballs.py` (deterministic).

- `yarn1-small`: a bin (`.bin/y1a`), a nested `node_modules` (two versions of
  `y1-b`), a scoped package, nested directories. No empty files.
- `yarn1-emptyfirst`: one package with 400 empty files among its first
  copies. Under Node >= 22 the copy of an empty file keeps the source's
  times, which flips Yarn's time probe (NOTES "Yarn 1 harness").
- `yarn1-scriptpkg`: a dependency with an install script that writes
  `built.txt`, a file the two runs write at different times.
- `yarn1-many` (Yarn 1 engine): 322 package files, more than the first 128
  copies of Yarn's link step that the engine leaves to Node; a bin, a nested
  `node_modules`, a 256 KiB file, devDependencies (`--production`), an
  install script. `yarn1-many/v2/` is the next version of its `package.json`
  and `yarn.lock` (one package upgraded with files changed, added and
  removed, one added, one removed) for an incremental install over the tree.
- `yarn1-workspaces` (Yarn 1 engine): two workspaces linked from the root
  `node_modules`, one with its own nested `node_modules`.

To change a tarball: edit `make-tarballs.py`, run it, then regenerate each
affected `yarn.lock` with the tarball server running:

```sh
cp fixtures/yarn1-small/package.json /tmp/gen/ && cd /tmp/gen && \
  YARN_DISABLE_SELF_UPDATE_CHECK=true yarn install --cache-folder /tmp/gen-cache
```

and copy `/tmp/gen/yarn.lock` back (the resolved URLs carry each tarball's
sha1).
