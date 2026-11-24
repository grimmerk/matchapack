# Yarn 1 fixtures

The `yarn1-*` fixtures are Yarn 1 projects
for `scripts/verify-yarn1.sh` and `scripts/test-yarn1.sh`. Every dependency is
a tarball URL on the local tarball server (127.0.0.1:48321, which serves
`fixtures/optional-upward/tarballs`), so they install without a network.
That host is a placeholder: each suite serves the tarballs on a free port
of its own and installs from a copy of `fixtures/` whose URLs name it
(`scripts/fixture-server.sh`).
The tarballs are made by `make-tarballs.py` (deterministic).

- `yarn1-small`: a bin (`.bin/y1a`), a nested `node_modules` (two versions of
  `y1-b`), a scoped package, nested directories. No empty files.
- `yarn1-emptyfirst`: one package with 400 empty files among its first
  copies. Under Node >= 22 the copy of an empty file keeps the source's
  times, which flips Yarn's time probe (TESTING.md, "The Yarn 1 harness").
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
- `yarn1-nohoist` (Yarn 1 engine): `yarn1-workspaces` with `nohoist:
  ["**/y1-many"]` and `y1-many` in both workspaces, so each workspace has its
  own copy of it (two copies of 302 files: `--link-duplicates` hardlinks
  one to the other). Same `yarn.lock` as `yarn1-workspaces`.
- `yarn1-focus` (Yarn 1 engine): workspace `app` depends on its sibling
  workspace `y1-c` 1.0.0. `yarn install --focus` in `packages/app` fetches a
  sibling from the registry instead of linking it; the `y1-c@1.0.0` entry
  in `yarn.lock` (added by hand, Yarn keeps it) resolves that to the
  `y1-c-1.0.0.tgz` on the local tarball server, so no registry is needed.
  The workspace's own `index.js` differs from the tarball's, so a tree
  shows which one it got.
- `yarn1-flat` (Yarn 1 engine): `yarn1-many` with `resolutions: {"y1-b":
  "2.0.0"}`, so `--flat` has its answer for the one package with two
  versions and does not prompt. `yarn.lock` as `yarn --flat` wrote it, from
  `yarn1-many`'s plus a `y1-b@2.0.0` entry added by hand (the resolution's
  pattern would otherwise be asked of the registry).
- `yarn1-rootpath` (Yarn 1 engine): `y1-many` and `y1-rootpath`, whose
  install script writes `built/<its own absolute directory>/marker`, so each
  tree has another path there, as node-gyp's `build/Release/.deps/`
  (`matchapack diff --normalize-roots`). The npm fixture `rootpath` uses
  the same tarball.

- `yarn1-upgrade` (Yarn 1 commands): dependencies by name and range
  (`y1-many >=1.0.0`, `y1-b ^1.0.0`, `y1-script ^1.0.0`, dev `@y1/scoped
  ^1.0.0`), locked to the 1.0.0 versions, and a `.yarnrc` naming the
  tarball server as the registry: `make-tarballs.py` also writes a registry
  document per package next to the tarballs (Yarn 1 reads a plain file
  server as a registry). So `yarn upgrade` moves `y1-many` to 2.0.0, `yarn
  upgrade y1-b --latest` rewrites its range, and `yarn add y1-c` resolves by
  name, all offline. Its `yarn.lock` was made by installing exact versions,
  then writing the ranges into the patterns.

To change a tarball: edit `make-tarballs.py`, run it, then regenerate each
affected `yarn.lock` with the tarball server running:

```sh
cp fixtures/yarn1-small/package.json /tmp/gen/ && cd /tmp/gen && \
  YARN_DISABLE_SELF_UPDATE_CHECK=true yarn install --cache-folder /tmp/gen-cache
```

and copy `/tmp/gen/yarn.lock` back (the resolved URLs carry each tarball's
sha1).
