# matchapack

Makes `npm ci` and Yarn 1 installs place `node_modules` as copy-on-write
clones, so each extra checkout or git worktree of a repo costs almost no disk.
`npm ci` clones every package from one machine-wide store; Yarn 1 (macOS) clones
every file from Yarn's own cache (see "Yarn 1 (classic) projects"). The tree it
builds is the tree plain `npm ci` or `yarn` builds: same paths, contents,
permissions, symlinks, `.bin` links, mtimes, and npm's hidden lockfile or
Yarn's `.yarn-integrity` (see `NOTES.md`
for how that was verified). Nothing is added to the project: no config file, no
package.json change; node_modules is all it writes.

Requires a filesystem with reflinks: APFS (every current Mac), Btrfs or XFS
with `reflink=1`. Elsewhere it hands over to npm or Yarn. On Linux, Yarn 1's own
copy already clones, so matchapack leaves Yarn alone there.

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
matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)
…npm's own output: warnings, script output, "added 347 packages in 1s", the audit summary…
matchapack: added 347 packages from the store in 0.8s (inject engine; npm extracted 0)
```

`npm ci` (or `npm clean-install`, `npm ic`, `npm install-clean`), with any
of npm's flags, is taken over. Every other npm command runs npm directly and
never starts matchapack. A Claude Code session picks the function up from its
shell snapshot, so sessions started after the line was added use it.

Bypass for one command: `MATCHAPACK=0 npm ci`, or `command npm ci`.
Remove: delete the line. Installed node_modules keep working without the store
(clones are independent files); the store is `~/.cache/matchapack/store`.

On macOS the same line also routes a bare `yarn` / `yarn install` in Yarn 1
projects (see "Yarn 1 (classic) projects").

**Node version managers.** matchapack uses the npm (and Yarn) that your
`npm` (and `yarn`) would run in that directory:
- nvm, fnm, Homebrew's `node`, the official installer, and mise with PATH
  activation put npm's own files on PATH; matchapack reads them directly
  (no extra cost).
- volta, asdf, and mise in shim mode put a small launcher (a "shim") on
  PATH, which picks a version per directory (`package.json` `volta`,
  `.tool-versions`, `mise.toml`, ...). matchapack asks the shim which npm or
  Yarn it starts there: it runs `npm --version` (or `yarn --version`)
  through the shim with a small probe loaded into node, which reports the
  file and stops node before npm or Yarn runs. This adds about 40–110 ms to
  each routed install (measured, medians of two runs on a busy machine:
  volta 40–60 ms, mise 80–90 ms, asdf 100–110 ms). Tested with volta 2.0.2, mise 2026.10.24, asdf 0.20.2.
- matchapack then starts npm or Yarn exactly as the shim starts it: the
  same node binary, with the shim's whole environment (its PATH, its own
  variables), so install scripts see what they see with plain npm or Yarn.
  It runs plain npm or Yarn instead, and says why, when it cannot do that
  exactly: the shim gives node command-line options, starts it in another
  directory, changes NODE_OPTIONS, or does not start node within 10 s;
  also for corepack's `npm` launcher, which is not handled.

## How it installs (the inject engine)

matchapack runs npm's own `npm ci`, unchanged, and replaces one thing inside
it: where npm would extract a package's tarball into a directory, matchapack
clones the package from the store instead (the inject engine,
grimmerk/matchapack-private#33; design and measurements in `NOTES.md`,
"Inject engine", "PR C1" and "PR C2"). Scripts, bin links, the hidden
lockfile, the lockfile check, the removal of the old `node_modules`, npm's
flags and settings are all npm's own, so they behave exactly as in `npm ci`.
It is the only engine since PR C2, which removed the classic engine (it ran
the steps of `npm ci` itself); `MATCHAPACK_ENGINE` may be unset, empty or
`inject`, and any other value (`classic` included) is an error for
`matchapack install` and plain npm, with the reason, for `npm ci`.

```sh
matchapack install . -- --no-audit   # directly; after `--`: npm's own arguments
```

In detail:
- Every `npm ci` flag and npm setting is npm's to read (`--omit=dev`,
  `--workspace`, `--workspaces`, `--install-strategy`, `NODE_ENV=production`,
  `.npmrc` settings; each of these is compared with plain `npm ci` on a
  workspaces fixture in `scripts/test-inject.sh`). With
  `--install-strategy=linked` npm extracts every package itself (its
  fetchers there carry no integrity): the tree is npm's, nothing is cloned.
  Packages matchapack cannot clone are extracted by npm, the rest are cloned
  (checked with a `file:` tarball and bundled dependencies; git dependencies
  and a shrinkwrap follow the same rule but have not been run yet).
- A package is cloned only when the result is exactly what npm would extract:
  a registry tarball with an integrity in the lockfile, npm's default file
  modes, umask 022. A package missing from the store is extracted into it by
  npm's own pacote (from npm's cache, or downloaded as npm would), then
  cloned: the store holds exactly what npm extracts.
- Cloned paths get the file group npm's extraction would give them (the
  project directory's on macOS; the process's, or a setgid directory's, on
  Linux), also when the store is under another group (#41).
- The store is checked against npm's extraction: every entry is compared with
  pacote's extraction once under each npm (pacote and tar version) before
  that npm clones it, entries the classic engine made included (stores filled
  before PR C2 still hold them), so the first
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
  in `npm ci`.
- Linux works the same way, with per-file clones (checked on btrfs and XFS in
  Docker, `scripts/linux-test.sh`).

matchapack runs plain npm instead, and prints why, before anything starts,
when:
- the `npm` on PATH cannot be traced to npm's own package, or not started
  exactly as it starts it (a launcher it cannot ask, corepack's `npm`
  launcher, a shim that gives node options; see "Node version managers");
- npm is not a major version this engine has been run on: 10, 11 and 12. It
  has no code per npm version, so every 10.x, 11.x and 12.x passes this check
  (`INJECT_NPM_MAJORS` in src/wrapper.rs; `MATCHAPACK_NPM_VERSIONS=any`, or a
  list such as `13.1`, overrides it; `matchapack report` counts these runs);
- npm's pacote code (the part matchapack replaces) is not code this build of
  matchapack was tested with, as after a new npm release: the message names
  its fingerprint; update matchapack, or set `MATCHAPACK_NPM_FINGERPRINT=any`
  (or that fingerprint) to try it;
- the umask is not 022;
- the project's `node_modules` is a symlink;
- the project is outside the trial scope (`~/.config/matchapack/paths`);
- `--prefix` / `--global` / `--location` point npm at another directory;
- the directory has no `package-lock.json` or `npm-shrinkwrap.json` (inside
  a workspace of a monorepo, `npm ci` installs into the root's
  `node_modules`; #36);
- the command installs nothing (`--dry-run`, `--help`);
- `MATCHAPACK_ENGINE` names another engine (`classic` was removed in PR C2).
When the filesystem cannot clone, npm extracts every package itself, and
matchapack says so: the first clone the filesystem refuses stops cloning for
that install.

Limits that do not change the tree, only what is saved:
- On macOS, in a directory whose group the user does not belong to (a project
  under `/tmp`, group `wheel`), matchapack cannot give placed files the group
  npm's extraction gets from the kernel. Packages it places by moving them in
  from the store (merges) are then extracted by npm and save no disk; clones
  made in place get the group from the kernel and still save it. The tree is
  identical to plain `npm ci` either way (NOTES, "PR C1", #41).
- Removing the old `node_modules` on a reinstall is npm's own step, which
  runs on Node's thread pool (4 threads by default), as with plain `npm ci`.
  To make it faster on a large project, `export UV_THREADPOOL_SIZE=16` (or
  your core count) in your shell. This is npm's own step, so it speeds up
  plain `npm ci` the same way. On a 3,466-package project, in only 4 rounds
  on a busy machine, npm's removal went from about 12 % slower than a
  10-thread removal to about 7 % faster (NOTES, "PR C2"). matchapack passes
  the environment through to npm unchanged, so your install scripts see the
  variable too, as with plain npm.

`matchapack report` shows the runs (and, from logs older than PR C2, how many
the classic engine took), packages cloned, packages npm extracted and why,
the store entries compared with npm's extraction, and a line when npm placed
packages that never reached matchapack's hook (the sign that the hook no
longer fits a new npm: installs stay correct, only slower; the install
itself prints a line too).

## Yarn 1 (classic) projects (macOS)

**What it does.** In a Yarn 1 project, `yarn` runs your own Yarn 1, as it
always did, with one change inside it. Where Yarn would copy a package file
from its cache into `node_modules`, matchapack makes a copy-on-write clone of
the cache file instead. The clone shares its disk blocks with the cache file,
so N worktrees of a project cost about one copy on disk: the one in Yarn's
cache. Everything else is Yarn's own: which files go where, the skipping of
files that are already in place, bin links, install scripts and
`.yarn-integrity`. matchapack's store is not used; Yarn's cache is the
source.

Checked against plain Yarn on three real projects (one package, one with 9
workspaces, and a private service repository), with install scripts and
without: the trees were identical, down to file times in nanoseconds,
owner, group, the names of extended attributes and the bytes of
`.yarn-integrity`. (One install fails in plain Yarn too, in a native
build; up to that point the trees differed only where two plain runs also
differ.) 99.7 % or more of the bytes Yarn takes from its cache shared
blocks with it. It is not faster: a fresh install took about as long as
plain Yarn on one project and about 10 % longer on the 9-workspace one.
Numbers and method: `NOTES.md`, "Yarn 1: summary".

**How to turn it on.** Nothing beyond the line from "Use" (`eval
"$(matchapack init)"`, or fish's). On macOS that line also defines a `yarn`
shell function. A startup file that already has the line gets the function
with the next `cargo install`, in a new terminal. Then type `yarn` as usual:

```
$ yarn install --frozen-lockfile
matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)
…Yarn's own output…
matchapack: 56914 files cloned from Yarn's cache (425.3 MiB), 128 copied by Node, in 25.2s
```

`matchapack yarn <args>` does the same without the function. Sourcing
`shell/matchapack.sh` from a checkout instead of the `init` line? Then also
source `shell/matchapack-yarn.sh` (fish: `shell/matchapack-yarn.fish`) after
it.

**What goes through matchapack.**
- A bare `yarn`, `yarn install …`, and `yarn` with a flag first (`yarn
  --frozen-lockfile`) go to `matchapack yarn`.
- Every other command (`yarn add`, `yarn run`, `yarn build`, `yarn
  --version`, `yarn --help`) runs Yarn directly; matchapack does not start.
- In a Yarn 2+ project (a `.yarnrc.yml`, `packageManager: yarn@2` or later,
  or a Yarn 2+ lockfile) `matchapack yarn` runs plain Yarn at once. It
  decides from those files only, without starting node, and prints and
  logs nothing. This adds a few milliseconds to a `yarn install`.
- A bare `yarn` in a Yarn 1 project that is already installed is still an
  install: Yarn checks `.yarn-integrity`, finds nothing to do and stops.
  matchapack adds about 30–40 ms to that (0.25 s instead of 0.21 s on one
  project).

**Bypass for one command**: `MATCHAPACK=0 yarn`, or `command yarn`.

**When it runs plain Yarn instead.** matchapack decides before Yarn starts
and prints why when:
- the `yarn` on PATH is neither Yarn 1's npm package, Homebrew's `yarn`
  (its launcher for that package), corepack's launcher, nor a launcher (a
  version manager's shim) that starts one of these in that directory (for
  example Yarn 2+ installed globally, or a launcher matchapack cannot
  follow exactly; see "Node version managers"; `MATCHAPACK_YARN_JS` can
  name a `yarn.js`), or corepack chose a Yarn that is not Yarn 1;
- there is no `yarn.lock` in the directory or above it;
- `packageManager` in `package.json` names another package manager (such as
  `npm@…`; plain Yarn 1.22 refuses to install such a project too);
- a `.yarnrc` sets `yarn-path` (Yarn would run another file);
- `node_modules` is a symlink;
- the project is outside the trial scope (`~/.config/matchapack/paths`);
- `--modules-folder`, `--cwd` or `--global-folder` move the install;
- Yarn's code does not have the shape the hook needs, or Node's own copy has
  changed (it keeps a file's time, or clones).

When Yarn's cache is on another volume than the project, nothing can be
cloned: Yarn installs with its own copies, and matchapack says so.

**corepack.** When `yarn` is corepack's (`corepack enable`; corepack comes
with Node 24 and earlier, and is installed with `npm i -g corepack` from Node
25 on), corepack chooses the project's Yarn as it always does (the
`packageManager` field, its defaults, its cache and downloads). If corepack
starts Yarn 1's package, matchapack takes over at that point. Limits:
- Tested with corepack 0.36.0 only (Node 24.22's own, and the package that
  `npm i -g corepack` installs). Other corepack versions have not been run.
- What corepack does before Yarn starts (a download, a prompt, an update of
  its last known good version) happens once, as with plain corepack, even
  when matchapack then runs plain Yarn.
- corepack's `shims/yarn` shell wrapper, and corepack's `yarn` behind a
  version manager's shim (asdf's Node with `corepack enable`), are found
  by asking them, as for any shim (see "Node version managers").

**Linux.** No `yarn` function is defined, and `matchapack yarn` is plain
Yarn: Yarn 1's own copy already clones on Btrfs and XFS.

**Yarn's cache now carries the shared blocks.** A cloned file shares its
blocks with the cache file it came from. `yarn cache clean` breaks nothing (a
clone is an independent file). But it frees nothing while worktrees still
share those blocks, and the next install shares with the new cache entries
only. Clones are not counted in the store's numbers (`matchapack store`,
`store gc` and `store verify` do not look at Yarn's cache).

**Existing `node_modules`.** Yarn skips files that are already in place, so
an existing `node_modules` keeps its own copies. Remove `node_modules` once
before `yarn`, or run `scripts/convert-existing.sh`, which does it with a
backup that is put back on any failure (see "Existing node_modules").

**How the clones stay true to Yarn's copies.**
- Yarn's first 128 copies of an install (its own concurrency, read from
  Yarn's code) are left to Node, so that Yarn's one-time check of whether
  copies keep their times sees real copies. Those files do not share blocks.
- Each clone is made to look like the copy Node would have made: file times
  and group as Node's copy sets them (measured at the start of each install),
  and extra extended attributes removed. One thing cannot be changed: macOS's
  `com.apple.provenance` attribute of a clone carries the value of its cache
  file.

**Report.** `matchapack report` shows a `yarn:` part: installs, why others
went to plain Yarn, files cloned and files Node copied (by reason), and
installs where Yarn linked but matchapack's hook never saw its link step (a
sign that the hook no longer fits that Yarn; the install itself is Yarn's
own and complete).

## Trial: logs, checks, scope

- `matchapack report [--days N]`: what happened (installed / handed to npm and
  why / failed), install times, space not written, store size, free space.
  Every `npm ci` that reaches matchapack appends one line to
  `~/.cache/matchapack/log.jsonl` (`MATCHAPACK_LOG=0` turns it off). Each
  line carries the 1- and 5-minute load average at the start and the end of
  the run (`load_start`, `load_end`); the report's recent runs show the
  1-minute pair (`load 3.1→5.4`), so a slow install can be read against what
  else the machine was doing. Lines that are not readable JSON are skipped
  and counted.
- `scripts/verify-sample.sh [N]`: re-checks the N most recent matchapack
  installs against a real `npm ci` on copies (npm ci runs twice so build noise
  is told apart); results show up in `matchapack report`.
- Limit the trial to some projects: list path prefixes, one per line, in
  `~/.config/matchapack/paths` (e.g. `~/git/app` also covers `~/git/app-<topic>`
  worktrees). Elsewhere npm ci runs as usual.

Known differences from `npm ci`: none in the tree (see "How it installs").

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

matchapack takes every npm 10, 11 and 12 release whose pacote code it was
tested with: it has no code per npm version, and what keeps it right on a new
release is checked while it runs (pacote's shape and its fingerprint before
npm starts, every store entry compared with the new pacote/tar's extraction
before its first use, packages npm placed that the hook never saw, npm reading
what the hook returns). Most npm releases do not change that pacote code. After
an upgrade the harness is still worth running:

```sh
scripts/verify-npm.sh /tmp/verify-npm              # the fixtures
scripts/verify-npm.sh /tmp/verify-npm ~/git/app    # plus a real project (copied, never changed)
```

It runs `npm ci` and matchapack side by side on copies and compares the trees
and the hidden lockfile byte for byte (the workspaces fixture also file
times, owner, group and extended attribute names), and says whether this npm's
pacote fingerprint is on the list. A new npm major goes into
`INJECT_NPM_MAJORS` in src/wrapper.rs once it passes, and a new fingerprint
into `src/npm-fingerprints.json` (after `scripts/test-inject.sh` passes too;
NOTES "G-lite").

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
- `matchapack store verify [--fix]`: compare every package with what npm's
  own pacote (the npm on PATH) extracts from its tarball in the npm cache, the
  way the inject engine has it extracted: files, contents, file and directory
  modes. Exit 1 when one differs; each difference names who made the entry
  (the classic engine's extractor, in stores filled before PR C2; this
  pacote/tar; or another version).
  `--fix` replaces those with pacote's extraction. A matching entry is
  recorded as known to that pacote/tar version, so the next install samples
  it instead of comparing it first. About 3 minutes for 8,779 packages on a
  busy machine (the classic engine's Rust extractor took about 1.5): each
  tarball is extracted again, by node.

## Other commands

- `matchapack init zsh|bash|fish`: print the shell integration (see "Use").
- `matchapack install [dir] [--ignore-scripts] [-- npm args]`: the install
  itself, with its counts and times (same gates as `npm ci`). The classic
  engine's options (`--no-rebuild`, `--mtime`, `--npm-cache`) and `matchapack
  plan` were removed in PR C2: `--ignore-scripts` (npm's) runs no scripts,
  and the `inject:` line and `matchapack report` say what was cloned and why
  npm extracted the rest.
- `matchapack diff A B`: the harness's tree comparison; exit 0 identical, 1
  they differ, 2 an error (nothing was decided). `--meta` also
  compares each path's mtime to the nanosecond, uid, gid and extended
  attribute names (not their values: macOS gives every file a
  `com.apple.provenance` value that a clone keeps and a copy does not), and
  reports each class of difference separately, with a line per
  `.yarn-integrity`. Times written during an install differ between two runs:
  `--a-run START:END` / `--b-run` (ns since the epoch) and `--a-cache` /
  `--b-cache` let those through only when both sides wrote them the same way
  (NOTES, "Yarn 1 harness"). `--normalize-roots` writes each tree's own
  root path, where it appears inside a relative path, as `<root>`: an install
  script that names a file after its own directory (node-gyp's
  `build/Release/.deps/<absolute path>/…`) then gives the same path in both
  trees. Both harnesses compare with it (NOTES, "Tree roots in file names").
- `scripts/verify-yarn1.sh [--cache warm|empty] <project> [yarn args]`: plain
  Yarn 1 twice on copies of a project with a scratch cache, compared with
  `diff --meta`; exit 0 identical, 3 "plain varies" (with the paths). The
  slot for a Yarn 1 engine, which does not exist yet. `scripts/test-yarn1.sh
  <work>`: its tests on the `fixtures/yarn1-*` projects.
- `scripts/verify.sh <project> <work>`: `npm ci` vs matchapack on copies of a
  project (`MATCHAPACK_VERIFY_VIA=wrapper` goes through the shell function). It
  runs on any npm version and ignores the trial scope file, since it is how a
  new npm version gets verified. `MATCHAPACK_VERIFY_BYTES=1` also requires the
  hidden lockfile to be byte-identical (CI sets it); `MATCHAPACK_VERIFY_META=1`
  also compares file times outside the two runs, owner, group and extended
  attribute names (`diff --meta`).
- CI (`.github/workflows/ci.yml`, on pull requests): `cargo test` and
  `test-shells.sh` on Linux; on macOS the safety, routing, store and inject
  tests, the fixture harness and the Yarn 1 harness tests.
- `scripts/test-inject.sh <work>`: the inject engine end to end (the hook
  cloned, a disabled hook and a store miss give the same tree, the gates,
  npm's own failures, the store lock, the project record, the file group
  with a store under another group, npm's flags on the workspaces fixture)
  and the hook's eligibility rules against the real pacote
  (`scripts/test-inject-hook.js`).
- `scripts/test-wrapper.sh <work>`: routing through the shell function
  (aliases, bypasses, the trial scope, hand-overs that leave `node_modules`
  untouched, npm's failures passed on once, the run log).
- `scripts/test-safety.sh <work>`: tampered lockfiles and symlinked
  directories, each against plain `npm ci` on a copy (the same status,
  nothing written outside the project beyond what npm itself writes).
- `scripts/test-store.sh <work>`: project records, `store gc`, `store verify`,
  the store lock and the tmp cleanup, on a store of its own.
- `scripts/test-shells.sh`: the same routing in bash, zsh and fish with stubs,
  in Docker (`docker run --rm -v "$PWD":/src:ro debian:bookworm-slim bash
  /src/scripts/test-shells.sh`). With `--real`, in `rust:1-bookworm` (it
  builds the binary and needs the network), fish also runs the real binary
  with real npm and Yarn 1; on a Linux host with fish and Node 24 (the CI
  job), `MATCHAPACK_BIN=target/release/matchapack scripts/test-shells.sh
  --real` runs those cases against a built binary: no apt and no build, and
  Yarn 1 goes into a scratch prefix. `scripts/test-yarn1.sh` runs the macOS
  yarn function under fish when `MATCHAPACK_FISH` names a fish binary.
