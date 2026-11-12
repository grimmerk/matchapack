# matchapack

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
cargo install --path .          # puts matchapack in ~/.cargo/bin
```

Then add one line to your shell's startup file. Not sure which shell you use?
Run `echo $0` (macOS has used zsh by default since 10.15).

| shell | file | line |
|---|---|---|
| zsh | `~/.zshrc` | `command -v matchapack >/dev/null && eval "$(matchapack init)"` |
| bash | `~/.bashrc` | `command -v matchapack >/dev/null && eval "$(matchapack init)"` |
| fish | `~/.config/fish/config.fish` | `type -q matchapack; and matchapack init \| source` |

Put it after the line that puts `~/.cargo/bin` on PATH (rustup usually does
that in `~/.zshenv` or `~/.profile`); if it comes earlier, it does nothing.
`matchapack init` detects the shell that runs it; `matchapack init zsh` (or
`bash`, `fish`) names it explicitly. The function comes from the binary, so no
checkout of this repository is needed and it updates with `cargo install`.
Sourcing `shell/matchapack.sh` from a checkout still works. Open a new terminal,
then type `npm ci` as usual:

```
$ npm ci
matchapack: npm ci from the store (MATCHAPACK=0 npm ci for plain npm)
…lifecycle script output…
matchapack: added 3383 packages from the store in 23.6s
```

Only a bare `npm ci` (or `npm clean-install`, `npm ic`, `npm install-clean`,
with output-only flags such as `--no-audit`) is taken over. Every other npm
command runs npm directly and never starts matchapack. A Claude Code session
picks the function up from its shell snapshot, so sessions started after the
line was added use it.

matchapack runs plain npm instead, and prints why, when:
- a flag changes the tree (`--omit=dev`, `--workspace`, …), or an npm setting
  is changed (in an `.npmrc` or the environment) that is not on matchapack's
  list of settings known to be safe (registry, credentials, cache, output,
  `legacy-peer-deps`, … ; `SAFE_CONFIG` in src/wrapper.rs), or
  `NODE_ENV=production` makes npm omit dev dependencies;
- a directory matchapack would write into resolves outside the project through
  a symlink (the project's node_modules, a workspace or `file:` directory);
- npm is a version the harness has not been run against (currently 11.19.x
  and 11.17.x, so not npm 10 / Node 22; `MATCHAPACK_NPM_VERSIONS=any`
  overrides; `matchapack report` counts these runs). After an npm upgrade, see
  "After an npm upgrade";
- the project is outside the trial scope (`~/.config/matchapack/paths`);
- the lockfile has entries it does not support (git dependencies, `file:`
  tarballs), or is out of sync with package.json;
- the filesystem cannot clone.
All of these are decided before anything in the project changes. A failing
install script is reported like npm would report it, not retried.

Bypass for one command: `MATCHAPACK=0 npm ci`, or `command npm ci`.
Remove: delete the line. Installed node_modules keep working without the store
(clones are independent files); the store is `~/.cache/matchapack/store`.

On macOS the same line also routes a bare `yarn` / `yarn install` in Yarn 1
projects (see "Yarn 1: `matchapack yarn`").

## The inject engine (try it with `MATCHAPACK_ENGINE=inject`)

A second way to install, built to replace the one above (grimmerk/matchapack-private#33;
design and measurements in `NOTES.md`, "Inject engine"). The engine above (the
"classic" engine) runs the steps of `npm ci` itself, calling npm for pieces.
The inject engine runs npm's own `npm ci`, unchanged, and replaces one thing
inside it: where npm would extract a package's tarball into a directory,
matchapack clones the package from the store instead. Scripts, bin links, the
hidden lockfile, the lockfile check, npm's flags and settings are all npm's
own, so they behave exactly as in `npm ci`, on any npm version.

```sh
MATCHAPACK_ENGINE=inject npm ci       # one command, through the shell function
export MATCHAPACK_ENGINE=inject       # or for the whole shell (classic: unset it)
MATCHAPACK_ENGINE=inject matchapack install . -- --no-audit  # after `--`: npm's own arguments
```

```
$ MATCHAPACK_ENGINE=inject npm ci
matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)
…npm's own output: warnings, script output, "added 347 packages in 1s", the audit summary…
matchapack: added 347 packages from the store in 0.8s (inject engine; npm extracted 0)
```

What differs from the classic engine:
- Every `npm ci` flag and npm setting is npm's to read (`--omit=dev`,
  `--workspace`, `NODE_ENV=production`, `.npmrc` settings; `--omit=dev` is in
  the tests, the others take the same path). Projects the
  classic engine hands to npm are installed too: the packages matchapack
  cannot clone are extracted by npm, the rest are cloned (checked with a
  `file:` tarball and bundled dependencies; git dependencies and a
  shrinkwrap follow the same rule but have not been run yet).
- A package is cloned only when the result is exactly what npm would extract:
  a registry tarball with an integrity in the lockfile, npm's default file
  modes, umask 022. A package missing from the store is extracted into it by
  npm's own pacote (from npm's cache, or downloaded as npm would), then
  cloned: the store holds exactly what npm extracts.
- The store is checked against npm's extraction: every entry is compared with
  pacote's extraction once under each npm (pacote and tar version) before
  that npm clones it, entries the classic engine made included, so the first
  install after an npm upgrade, or the first one under another Node's npm, is
  slower for the entries it uses; after that one in 50 at random
  (`MATCHAPACK_INJECT_CHECK=all`, `off` or another number). An entry that
  differs is replaced by pacote's extraction, which is what gets installed,
  and matchapack says so. Details: `NOTES.md`, "Store fill through pacote".
- `npm ci`'s own output comes back, the `npm audit` summary included (and its
  network request; `--no-audit` turns it off, as with npm).
- npm's own errors (lockfile out of sync, a failing script) are printed once,
  by npm, with npm's exit status.
- Root `predependencies` / `dependencies` / `postdependencies` scripts run, as
  in `npm ci` (the classic engine does not run them).
- Linux works the same way, with per-file clones (checked on btrfs and XFS in
  Docker, `scripts/linux-test.sh`).

matchapack still runs plain npm instead, before anything starts, when: npm is
not a major version this engine has been run on (10, 11 and 12; it has no code
per npm version, so every 10.x, 11.x and 12.x is taken, unlike the classic
engine's list
of verified minors; `MATCHAPACK_NPM_VERSIONS` overrides it as for the classic
engine), the umask is not 022, the project's
`node_modules` is a symlink, the project is outside the trial scope,
`--prefix` / `--global` point npm at another directory, the directory has no
`package-lock.json` or `npm-shrinkwrap.json` (inside a workspace of a
monorepo, `npm ci` installs into the root's `node_modules`; #36), or the
command installs nothing (`--dry-run`, `--help`).

`matchapack report` shows a line for this engine: packages cloned, packages npm
extracted and why, and a second line when npm placed packages that never
reached matchapack's hook (the sign that the hook no longer fits a new npm:
installs stay correct, only slower; the install itself prints a line too).

## Yarn 1: `matchapack yarn` (macOS)

The same idea for Yarn 1 (classic) projects, without matchapack's store:
Yarn's own cache is the source. `matchapack yarn <args>` runs the user's Yarn
1, unchanged, and replaces one thing inside its link step: where Yarn would
copy a file from its cache into `node_modules`, matchapack makes a
copy-on-write clone of the cache file instead. Which files go where, the
skipping of unchanged files, bin links, install scripts and
`.yarn-integrity` are Yarn's own (design and measurements: `NOTES.md`, "Yarn
1 engine").

```sh
matchapack yarn                                    # as `yarn` (an install)
matchapack yarn install --frozen-lockfile          # any yarn install flags
```

```
$ matchapack yarn install --frozen-lockfile
matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)
…Yarn's own output…
matchapack: 56914 files cloned from Yarn's cache (425.3 MiB), 128 copied by Node, in 25.2s
```

**Typing `yarn` (macOS).** The line from "Use" (`eval "$(matchapack
init)"`, or fish's) also defines a `yarn` function on macOS. Nothing else
needs to change: a startup file that already has the line gets the function
with the next `cargo install`, in a new terminal. Then:

- a bare `yarn`, `yarn install …`, and `yarn` with a flag first (`yarn
  --frozen-lockfile`) go to `matchapack yarn`;
- every other command (`yarn add`, `yarn run`, `yarn --version`, `yarn
  --help`) runs Yarn directly and never starts matchapack;
- in a Yarn 2+ project (a `.yarnrc.yml`, `packageManager: yarn@2` or later,
  a Yarn 2+ lockfile) `matchapack yarn` runs plain Yarn at once, decided from
  those files only (node is not started; it adds about 5 ms to a `yarn
  install`), and prints and logs nothing;
- bypass for one command: `MATCHAPACK=0 yarn`, or `command yarn`.

On Linux no `yarn` function is defined. From a checkout, source
`shell/matchapack-yarn.sh` (fish: `shell/matchapack-yarn.fish`) after
`shell/matchapack.sh`.

**corepack.** When `yarn` is corepack's (`corepack enable`; corepack comes
with Node 24 and earlier, and is installed with `npm i -g corepack` from Node
25 on), corepack chooses the project's Yarn as it always does (the
`packageManager` field, its defaults, its cache and downloads), and
matchapack takes over when corepack starts that Yarn, if it is Yarn 1's
package.

- Each clone is made to look exactly like the copy Node would have made:
  what Node's copy keeps (file times, the group) is measured when each
  install starts, and extra extended attributes are removed. Yarn's first 128
  copies (its own concurrency, read from Yarn's code) are left to Node, so
  Yarn's one-time check of whether copies keep their times sees real copies;
  those files do not share blocks.
- Only `yarn` and `yarn install` are handled; every other command (`yarn add`,
  `yarn run`, `yarn --version`) goes to Yarn as it is.
- On Linux, `matchapack yarn` is plain Yarn: Yarn 1's copy already clones
  there (btrfs, XFS).

matchapack runs plain Yarn instead, before Yarn starts, and says why when: the
`yarn` on PATH is neither Yarn 1's npm package nor corepack's launcher (Yarn
2+ installed globally, another launcher; `MATCHAPACK_YARN_JS` names a
`yarn.js` instead), corepack chose a Yarn that is not Yarn 1's package, there
is no `yarn.lock` in the directory or above, `packageManager` names another
package manager, a `.yarnrc` sets `yarn-path` (Yarn would run another file),
`node_modules` is a symlink, the project is outside the trial scope,
`--modules-folder` / `--cwd` / `--global-folder` move the install, Yarn's code
does not have the shape the hook needs, or Node's own copy has changed (it
keeps a file's time, or clones). When Yarn's cache is on another volume than
the project nothing can be cloned: Yarn installs with its own copies, and
matchapack says so.

**Yarn's cache now carries the shared blocks.** A cloned file shares its
blocks with the cache file it came from. `yarn cache clean` breaks nothing (a
clone is an independent file) and frees nothing while worktrees still share
those blocks; the next install shares with the new cache entries only.
Clones are not counted in the store's numbers (`matchapack store`, `store gc`
and `store verify` do not involve Yarn's cache). An existing `node_modules` is
not converted: Yarn skips files that are already in place, so remove
`node_modules` once to share it. One detail differs from a copy and cannot be
changed: macOS's `com.apple.provenance` attribute of a clone carries the value
of its cache file.

`matchapack report` shows a `yarn:` part: installs, why others went to plain
Yarn, files cloned and files Node copied (by reason), and installs where Yarn
linked but matchapack's hook never saw its link step (the sign that the hook no
longer fits a Yarn; the install itself is Yarn's own and complete).

## Trial: logs, checks, scope

- `matchapack report [--days N]`: what happened (installed / handed to npm and
  why / failed), install times, space not written, store size, free space.
  Every `npm ci` that reaches matchapack appends one line to
  `~/.cache/matchapack/log.jsonl` (`MATCHAPACK_LOG=0` turns it off).
- `scripts/verify-sample.sh [N]`: re-checks the N most recent matchapack
  installs against a real `npm ci` on copies (npm ci runs twice so build noise
  is told apart); results show up in `matchapack report`.
- Limit the trial to some projects: list path prefixes, one per line, in
  `~/.config/matchapack/paths` (e.g. `~/git/app` also covers `~/git/app-<topic>`
  worktrees). Elsewhere npm ci runs as usual.

Known differences from `npm ci` that do not change the tree: the `npm audit`
summary is not run; the output lines differ.

## Existing node_modules

```sh
scripts/convert-existing.sh ~/git            # lists what it would do
scripts/convert-existing.sh --apply ~/git    # re-installs each project via matchapack
```

npm projects (`package-lock.json`) are re-installed with `matchapack npm ci`.
Yarn 1 projects (`yarn.lock`, macOS) have their `node_modules` (the root's
and each workspace's) removed and re-installed with `matchapack yarn install
--frozen-lockfile`; install scripts run again (workspaces as `yarn
workspaces info` lists them). Each `node_modules` is backed up as a
copy-on-write copy and the copy checked against it before anything is
removed; it is put back if matchapack declines, the install fails, or the
script is interrupted (Ctrl-C). Skips projects whose backup cannot be made,
projects matchapack would not handle, Yarn 2+ projects, projects with
packages the lockfile does not list (`npm link`) or that are symlinks other
than workspaces (`yarn link`), and projects a running process has its
working directory in.

## After an npm upgrade

matchapack uses npm's own internals (the ideal tree, rebuild, config), so it
handles only npm versions the harness was run against. On any other version
every `npm ci` runs plain npm: nothing breaks, nothing is saved. To verify the
new npm and turn matchapack back on for it:

```sh
scripts/verify-npm.sh /tmp/verify-npm              # the fixtures
scripts/verify-npm.sh /tmp/verify-npm ~/git/app    # plus a real project (copied, never changed)
```

It runs `npm ci` and matchapack side by side on copies and compares the trees
and the hidden lockfile byte for byte. When every case is identical it
records the npm major.minor in `~/.config/matchapack/npm-verified`, which the
version gate reads next to the built-in list (`VERIFIED_NPM` in
src/wrapper.rs, where a version goes for every machine). When a case differs
it records nothing and shows the log.

## Store maintenance

- `matchapack store`: where the store is, how many packages and recorded
  projects it holds.
- `matchapack store gc [--dry-run]`: remove the packages no existing project
  uses. A package stays while a project installed from it still has its
  node_modules (every install records its directory and packages in
  `<store>/projects/`; projects installed before records existed are found
  through the trial log). Removing a package never breaks an installed
  node_modules, but disk space comes back only for packages no node_modules
  still shares blocks with, so the real run reports the volume's free space
  before and after. Packages extracted in the last hour are kept. Refused while
  an install is running.
- `matchapack store verify [--fix]`: compare every package with its tarball in
  the npm cache (files, contents, file modes, directories); exit 1 when one
  differs. `--fix` removes those, and the next install re-extracts them. About
  1.5 minutes for 8,700 packages (4.9 GiB): each tarball is extracted again.

## Other commands

- `matchapack init zsh|bash|fish`: print the shell integration (see "Use").
- `matchapack install [dir]`: the install itself, with per-step timings (same
  npm version gate as `npm ci`).
- `matchapack plan [dir]`: what would be placed and what is skipped, and why.
- `matchapack diff A B`: the harness's tree comparison; exit 0 identical, 1
  they differ, 2 an error (nothing was decided). `--meta` also
  compares each path's mtime to the nanosecond, uid, gid and extended
  attribute names (not their values: macOS gives every file a
  `com.apple.provenance` value that a clone keeps and a copy does not), and
  reports each class of difference separately, with a line per
  `.yarn-integrity`. Times written during an install differ between two runs:
  `--a-run START:END` / `--b-run` (ns since the epoch) and `--a-cache` /
  `--b-cache` let those through only when both sides wrote them the same way
  (NOTES, "Yarn 1 harness").
- `scripts/verify-yarn1.sh [--cache warm|empty] <project> [yarn args]`: plain
  Yarn 1 twice on copies of a project with a scratch cache, compared with
  `diff --meta`; exit 0 identical, 3 "plain varies" (with the paths). The
  slot for a Yarn 1 engine, which does not exist yet. `scripts/test-yarn1.sh
  <work>`: its tests on the `fixtures/yarn1-*` projects.
- `scripts/verify.sh <project> <work>`: `npm ci` vs matchapack on copies of a
  project (`MATCHAPACK_VERIFY_VIA=wrapper` goes through the shell function). It
  runs on any npm version and ignores the trial scope file, since it is how a
  new npm version gets verified. `MATCHAPACK_VERIFY_BYTES=1` also requires the
  hidden lockfile to be byte-identical (CI sets it). `MATCHAPACK_ENGINE=inject`
  makes B (and `scripts/verify-npm.sh`'s whole list, plus the cases only that
  engine handles) use the inject engine; nothing is recorded then.
- CI (`.github/workflows/ci.yml`, on pull requests): `cargo test` and
  `test-shells.sh` on Linux; safety, routing, store, the inject engine's tests,
  the fixture harness with both engines and the Yarn 1 harness tests on macOS.
- `scripts/test-inject.sh <work>`: the inject engine end to end (the hook
  cloned, a disabled hook and a store miss give the same tree, the gates,
  npm's own failures, the store lock, the project record) and the hook's
  eligibility rules against the real pacote (`scripts/test-inject-hook.js`).
- `scripts/test-wrapper.sh <work>`: routing tests for the shell integration.
- `scripts/test-store.sh <work>`: project records, `store gc`, `store verify`,
  the store lock and the tmp cleanup, on a store of its own.
- `scripts/test-shells.sh`: the same routing in bash, zsh and fish with stubs,
  in Docker (`docker run --rm -v "$PWD":/src:ro debian:bookworm-slim bash
  /src/scripts/test-shells.sh`).
