# odd-tarballs

Two remote tarballs (served from `fixtures/optional-upward/tarballs` by the
local tarball server, like the other fixtures) that `npm pack` cannot make,
built by `make-tarballs.py`:

- `lc-dup` lists `run.sh` twice, first with mode 0755, then with 0644 and other
  contents. npm (node-tar) writes the second entry into the file the first one
  made: `run.sh` ends up 0755 with the second contents. thinclone's own
  extractor (the classic engine's) gives 0644, so the classic engine's
  harness run on this fixture reports a difference; it gets no fixes
  (grimmerk/thinclone#33). The inject engine fills the store through pacote:
  identical.
- `lc-ignore` has `.gitignore` before `.npmignore`, and a lone
  `sub/.gitignore`. pacote renames a `.gitignore` to `.npmignore` unless a
  `.npmignore` came first in the same directory.

Only the inject engine's harness run includes this fixture
(`THINCLONE_ENGINE=inject scripts/verify-npm.sh`).
