# bundled fixture

A package with bundled dependencies whose directory npm also nests another
package in:

- `lc-bundler` ships `node_modules/lc-inner` inside its tarball
  (`bundleDependencies`);
- it also depends on `lc-s` 2.0.0, while the root depends on `lc-s` 1.0.0, so
  npm places 2.0.0 at `node_modules/lc-bundler/node_modules/lc-s` from its
  own tarball.

So `node_modules/lc-bundler` already holds a `node_modules/` when it is
extracted, and the package brings a `node_modules/` of its own: the inject
engine's merge placement has to merge the two (src/serve.rs `merge_into`).
Expected: `lc-bundler` (with `lc-inner` and `lc-s` 2.0.0 nested), `lc-s` 1.0.0.

The tarballs are served like optional-upward's (same directory, see its
README). `lc-bundler-1.0.0.tgz` was made with `npm pack` in a copy of
`../optional-upward/pkgs/lc-bundler` with `pkgs/lc-inner` copied to its
`node_modules/lc-inner`; `lc-s-2.0.0.tgz` from `pkgs/lc-s2`.
