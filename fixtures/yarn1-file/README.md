# yarn1-file

Two Yarn 1 projects with a `file:` dependency: `tgz/` on
`file:./local-1.0.0.tgz`, `dir/` on `file:./local-dir`. The local package
is not kept here: `make.py tgz|dir <copy>` writes it into a copy of the
project (deterministically: the tarball's sha1 is the one in
`tgz/yarn.lock`). `scripts/test-yarn1.sh` (engine part) installs each with
plain Yarn twice and `matchapack yarn` once (`verify-yarn1.sh --engine
yarn1`) and requires the trees to be identical and some files cloned.

Each `yarn.lock` was written by plain Yarn 1.22.22 with a scratch cache of
its own. Do not generate two of them with one shared cache: two tarballs
with the same relative path, name and version but other contents get the
first one's sha1 from the cache, and the later `--frozen-lockfile`
install fails with "Integrity check failed".
