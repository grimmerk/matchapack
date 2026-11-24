# rootpath fixture

One dependency, `y1-rootpath` (tarball made by
`fixtures/yarn1-small/make-tarballs.py`), whose install script writes
`built/<its own absolute directory>/marker`, the way node-gyp's make build
names `build/Release/.deps/<absolute path>/…` files. A (npm ci) and B
(matchapack) therefore each have a path named after their own tree:
`scripts/verify.sh` compares with `matchapack diff --normalize-roots`, which
writes each tree's root as `<root>`, so the two paths match.
`allowScripts` lets npm >= 11.19 run the script. Tarballs: see
optional-upward's README.
