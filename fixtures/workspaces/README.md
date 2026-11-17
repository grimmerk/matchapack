# workspaces fixture

`npm ci` at the root of an npm workspaces project:

- two workspaces, `ws-app` depending on `ws-lib`: npm links
  `node_modules/ws-app` and `node_modules/ws-lib`;
- a version conflict: the root wants `y1-b` 2.0.0 and `y1-many` 2.0.0,
  `ws-app` wants 1.0.0 of each, so `packages/app/node_modules/` holds
  them, with its own `.bin/y1many`; `y1-a` nests its own `y1-b` 1.0.0;
- an install script: `y1-script` (a dependency of `ws-lib`, hoisted),
  allowed by `allowScripts`;
- bins at the root (`.bin/y1a`, `.bin/y1many`) and in `ws-app`;
- dev dependencies at the root (`@y1/scoped`) and in `ws-app` (`lc-s`), for
  `--omit=dev`.

Tarballs: the existing ones in `fixtures/optional-upward/tarballs` (made by
`fixtures/yarn1-small/make-tarballs.py`), named at the placeholder 127.0.0.1:48321 (each suite serves them on a port
of its own, `scripts/fixture-server.sh`). Used by
`scripts/verify-npm.sh` (both engines, with `--meta`) and
`scripts/test-inject.sh` case 14 (npm's workspace, omit and install-strategy
flags). The lockfile was made with `npm install --package-lock-only
--ignore-scripts` (npm 11.19.1).
