# lockclone

Makes `npm ci` place every package as a copy-on-write clone from one
machine-wide store, so N worktrees of a repo cost about one copy on disk. The
tree it builds is the tree `npm ci` builds: same paths, contents, permissions,
symlinks, `.bin` links, hidden lockfile and mtimes (see `NOTES.md` for how that
was verified). Nothing is added to the project: no config file, no package.json
change; node_modules is all it writes.

Requires a filesystem with reflinks: APFS (every current Mac), Btrfs or XFS
with `reflink=1`. Elsewhere it hands over to npm.

## Use

```sh
cargo install --path .          # puts lockclone in ~/.cargo/bin
```

Then add one line to `~/.zshrc` or `~/.bashrc`:

```sh
[ -f "$HOME/git/lockclone/shell/lockclone.sh" ] && . "$HOME/git/lockclone/shell/lockclone.sh"
```

(fish: `shell/lockclone.fish`.) From then on, type `npm ci` as usual:

```
$ npm ci
lockclone: npm ci from the store (LOCKCLONE=0 npm ci for plain npm)
…lifecycle script output…
lockclone: added 3383 packages from the store in 23.6s
```

Only a bare `npm ci` (or `npm clean-install`, `npm ic`, `npm install-clean`,
with output-only flags such as `--no-audit`) is taken over. Every other npm
command runs npm directly and never starts lockclone. A Claude Code session
picks the function up from its shell snapshot, so sessions started after the
line was added use it.

lockclone runs plain npm instead, and prints why, when:
- a flag or npm setting changes the tree (`--omit=dev`, `NODE_ENV=production`,
  `install-strategy`, `--workspace`, …);
- npm is a version the harness has not been run against (currently 11.19.x;
  `LOCKCLONE_NPM_VERSIONS=any` overrides);
- the lockfile has entries it does not support (git dependencies, `file:`
  tarballs), or is out of sync with package.json;
- the filesystem cannot clone.
All of these are decided before anything in the project changes. A failing
install script is reported like npm would report it, not retried.

Bypass for one command: `LOCKCLONE=0 npm ci`, or `command npm ci`.
Remove: delete the line. Installed node_modules keep working without the store
(clones are independent files); the store is `~/.cache/lockclone/store`.

## Existing node_modules

```sh
scripts/convert-existing.sh ~/git            # lists what it would do
scripts/convert-existing.sh --apply ~/git    # re-installs each project via lockclone
```

Skips projects lockclone would not handle and projects a running process has
its working directory in.

## Other commands

- `lockclone install [dir]`: the install itself, with per-step timings.
- `lockclone plan [dir]`: what would be placed and what is skipped, and why.
- `lockclone diff A B`: the harness's tree comparison.
- `scripts/verify.sh <project> <work>`: `npm ci` vs lockclone on copies of a
  project (`LOCKCLONE_VERIFY_VIA=wrapper` goes through the shell function).
- `scripts/test-wrapper.sh <work>`: routing tests for the shell integration.
