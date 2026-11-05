# thinclone

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
cargo install --path .          # puts thinclone in ~/.cargo/bin
```

Then add one line to your shell's startup file. Not sure which shell you use?
Run `echo $0` (macOS has used zsh by default since 10.15).

| shell | file | line |
|---|---|---|
| zsh | `~/.zshrc` | `command -v thinclone >/dev/null && eval "$(thinclone init)"` |
| bash | `~/.bashrc` | `command -v thinclone >/dev/null && eval "$(thinclone init)"` |
| fish | `~/.config/fish/config.fish` | `type -q thinclone; and thinclone init \| source` |

Put it after the line that puts `~/.cargo/bin` on PATH (rustup usually does
that in `~/.zshenv` or `~/.profile`); if it comes earlier, it does nothing.
`thinclone init` detects the shell that runs it; `thinclone init zsh` (or
`bash`, `fish`) names it explicitly. The function comes from the binary, so no
checkout of this repository is needed and it updates with `cargo install`.
Sourcing `shell/thinclone.sh` from a checkout still works. Open a new terminal,
then type `npm ci` as usual:

```
$ npm ci
thinclone: npm ci from the store (THINCLONE=0 npm ci for plain npm)
…lifecycle script output…
thinclone: added 3383 packages from the store in 23.6s
```

Only a bare `npm ci` (or `npm clean-install`, `npm ic`, `npm install-clean`,
with output-only flags such as `--no-audit`) is taken over. Every other npm
command runs npm directly and never starts thinclone. A Claude Code session
picks the function up from its shell snapshot, so sessions started after the
line was added use it.

thinclone runs plain npm instead, and prints why, when:
- a flag changes the tree (`--omit=dev`, `--workspace`, …), or an npm setting
  is changed (in an `.npmrc` or the environment) that is not on thinclone's
  list of settings known to be safe (registry, credentials, cache, output,
  `legacy-peer-deps`, … ; `SAFE_CONFIG` in src/wrapper.rs), or
  `NODE_ENV=production` makes npm omit dev dependencies;
- a directory thinclone would write into resolves outside the project through
  a symlink (the project's node_modules, a workspace or `file:` directory);
- npm is a version the harness has not been run against (currently 11.19.x
  and 11.17.x, so not npm 10 / Node 22; `THINCLONE_NPM_VERSIONS=any`
  overrides; `thinclone report` counts these runs);
- the project is outside the trial scope (`~/.config/thinclone/paths`);
- the lockfile has entries it does not support (git dependencies, `file:`
  tarballs), or is out of sync with package.json;
- the filesystem cannot clone.
All of these are decided before anything in the project changes. A failing
install script is reported like npm would report it, not retried.

Bypass for one command: `THINCLONE=0 npm ci`, or `command npm ci`.
Remove: delete the line. Installed node_modules keep working without the store
(clones are independent files); the store is `~/.cache/thinclone/store`.

## Trial: logs, checks, scope

- `thinclone report [--days N]`: what happened (installed / handed to npm and
  why / failed), install times, space not written, store size, free space.
  Every `npm ci` that reaches thinclone appends one line to
  `~/.cache/thinclone/log.jsonl` (`THINCLONE_LOG=0` turns it off).
- `scripts/verify-sample.sh [N]`: re-checks the N most recent thinclone
  installs against a real `npm ci` on copies (npm ci runs twice so build noise
  is told apart); results show up in `thinclone report`.
- Limit the trial to some projects: list path prefixes, one per line, in
  `~/.config/thinclone/paths` (e.g. `~/git/app` also covers `~/git/app-<topic>`
  worktrees). Elsewhere npm ci runs as usual.

Known differences from `npm ci` that do not change the tree: the `npm audit`
summary is not run; the output lines differ.

## Existing node_modules

```sh
scripts/convert-existing.sh ~/git            # lists what it would do
scripts/convert-existing.sh --apply ~/git    # re-installs each project via thinclone
```

Skips projects thinclone would not handle and projects a running process has
its working directory in.

## Store maintenance

- `thinclone store`: where the store is, how many packages and recorded
  projects it holds.
- `thinclone store gc [--dry-run]`: remove the packages no existing project
  uses. A package stays while a project installed from it still has its
  node_modules (every install records its directory and packages in
  `<store>/projects/`; projects installed before records existed are found
  through the trial log). Removing a package never breaks an installed
  node_modules, but disk space comes back only for packages no node_modules
  still shares blocks with, so the real run reports the volume's free space
  before and after. Packages extracted in the last hour are kept. Refused while
  an install is running.
- `thinclone store verify [--fix]`: compare every package with its tarball in
  the npm cache (files, contents, file and directory modes); exit 1 when one
  differs. `--fix` removes those, and the next install re-extracts them. About
  1.5 minutes for 8,700 packages (4.9 GiB): each tarball is extracted again.

## Other commands

- `thinclone init zsh|bash|fish`: print the shell integration (see "Use").
- `thinclone install [dir]`: the install itself, with per-step timings (same
  npm version gate as `npm ci`).
- `thinclone plan [dir]`: what would be placed and what is skipped, and why.
- `thinclone diff A B`: the harness's tree comparison.
- `scripts/verify.sh <project> <work>`: `npm ci` vs thinclone on copies of a
  project (`THINCLONE_VERIFY_VIA=wrapper` goes through the shell function). It
  runs on any npm version and ignores the trial scope file, since it is how a
  new npm version gets verified. `THINCLONE_VERIFY_BYTES=1` also requires the
  hidden lockfile to be byte-identical (CI sets it).
- CI (`.github/workflows/ci.yml`, on pull requests): `cargo test` and
  `test-shells.sh` on Linux; safety, routing, store and the fixture harness on
  macOS.
- `scripts/test-wrapper.sh <work>`: routing tests for the shell integration.
- `scripts/test-store.sh <work>`: project records, `store gc`, `store verify`,
  the store lock and the tmp cleanup, on a store of its own.
- `scripts/test-shells.sh`: the same routing in bash, zsh and fish with stubs,
  in Docker (`docker run --rm -v "$PWD":/src:ro debian:bookworm-slim bash
  /src/scripts/test-shells.sh`).
