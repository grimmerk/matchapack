# dep-event fixture

The root package defines `predependencies`, `dependencies` and
`postdependencies` scripts. `npm ci` runs them inside reify, after the hidden
lockfile is saved, whenever the tree changed (arborist `reify.js`); each
writes an `ev-<event>.txt` file in the project. The classic engine does not
run them (it runs `npm rebuild` and the seven root events of `ci.js`), so
this fixture is identical to `npm ci` only with `MATCHAPACK_ENGINE=inject`,
where npm itself runs. `scripts/verify-npm.sh` includes it for that engine
only. Tarballs: see optional-upward's README.
