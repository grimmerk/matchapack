# matchapack

Makes `npm ci`, `npm install` (also `npm update` and `npm uninstall`) and
Yarn 1 installs place `node_modules` as copy-on-write clones, so each extra
checkout or git worktree of a repo costs almost no disk. npm clones every
package it installs from one machine-wide store; Yarn 1 (macOS) clones
every file from Yarn's own cache (see "Yarn 1 (classic) projects"). The tree it
builds is the tree plain npm or `yarn` builds: same paths, contents,
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
`bash`, `fish`) names it explicitly. For a short name, e.g. `--alias mp`
(`eval "$(matchapack init --alias mp)"`, fish `matchapack init --alias mp |
source`): `mp` then runs matchapack (`mp store gc`); an alias in zsh and
bash, a function in fish. The name must not be a builtin of the shell, nor
npm, yarn or matchapack (refused: the line still defines the npm function,
without the alias); a command of that name on PATH is only warned about.
The function comes from the binary, so no
checkout of this repository is needed and it updates with `cargo install`.
Sourcing `shell/matchapack.sh` from a checkout still works. Open a new terminal,
then type `npm ci` as usual:

```
$ npm ci
matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)
…npm's own output: warnings, script output, "added 347 packages in 1s", the audit summary…
matchapack: added 347 packages from the store in 0.8s (inject engine; npm extracted 0)
```

`npm install`, `npm update` and `npm uninstall` go the same way, quietly:
they print one line of matchapack's only when a package was placed (most
runs on an installed project place nothing), and npm writes `package.json`
and `package-lock.json` itself, as always:

```
$ npm install left-pad@1.3.0
…npm's own output: "added 1 package, and audited 349 packages in 1s"…
matchapack: 1 package cloned from the store, npm extracted 0 (inject engine; MATCHAPACK=0 npm install for plain npm)
```

Taken over, with any of npm's flags: `npm ci` (and `clean-install`, `ic`,
`install-clean`, `isntall-clean`), `npm install` (and `i`, `add`, `in`,
`ins`, `inst`, `insta`, `instal`, `isnt`, `isnta`, `isntal`, `isntall`),
`npm update` (and `up`, `upgrade`, `udpate`, and `u` from npm 11) and
`npm uninstall` (and `un`, `unlink`, `remove`, `rm`, `r`): the first words
npm itself has for these commands (`ROUTED` in src/wrapper.rs). Every other
npm command runs npm directly and never starts matchapack. A Claude Code
session picks the function up from its shell snapshot, so sessions started
after the line was added use it.

Not taken over: these go straight to plain npm (never through matchapack,
or declined with the reason before npm's command runs):
- `npm install-test` / `it`, `npm install-ci-test` / `cit`: an install
  followed by `npm test`; the same engine path, not routed yet (no harness
  cases; the owner's decision, 2026-11-15).
- `npm dedupe`, `npm prune`, `npm audit fix`: they rewrite the tree too; not
  routed yet, for the same reason.
- `npm link`, `npm exec` / `npx`: they install into npm's global directory
  or npx's cache, not into the project.
- Abbreviations npm also accepts (`npm upd`, `npm uninst`): not on the list
  of first words; plain npm. So is a command line that starts with a flag
  (`npm --prefix x install`).
- Global installs: `-g`, `--global`, `--location=global` on the command
  line, and `global=true` / `location=global` in the environment or any
  npmrc (the project's included): they install outside any project.
- `--dry-run`, `--package-lock-only`, `--help` / `--usage`: they install
  nothing.
- `npm u` on npm 10, where it is no command: npm prints its own error.

Bypass for one command: `MATCHAPACK=0 npm install`, or `command npm install`.
Remove: `matchapack uninstall` lists what it would remove (the store, the
trial logs, the settings directory), the space that frees, the lines of your
startup files that mention matchapack (file:line; it never edits them) and
the command that removes the binary (`cargo uninstall matchapack`);
`matchapack uninstall --yes` removes matchapack's state. Installed
node_modules keep working without the store (clones are independent files),
and a later `npm ci` or `yarn` without the line is plain npm or Yarn.

On macOS the same line also routes a bare `yarn`, `yarn install`, `yarn add`,
`upgrade`, `remove` and `upgrade-interactive` in Yarn 1 projects (see "Yarn 1
(classic) projects").

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
- Only these launchers are asked, each recognised by its install layout:
  volta's shim, mise's shims, asdf's shims for its nodejs plugin, and
  corepack's own `yarn` wrapper. They pass your arguments to npm or Yarn
  unchanged, whatever the command. Any other launcher or wrapper script on
  PATH is not run by matchapack: it runs plain npm or Yarn and names the
  launcher. A question with `--version` cannot show what such a wrapper
  would add to an install (`--ignore-scripts`, say). The shim must also
  start npm's own `bin/npm-cli.js` or Yarn 1's `bin/yarn.js` (or
  corepack's launcher) itself, not another script beside it.
- matchapack then starts npm or Yarn exactly as the shim starts it: the
  same node binary, with the shim's whole environment (its PATH, its own
  variables), so install scripts see what they see with plain npm or Yarn.
  It runs plain npm or Yarn instead, and says why, when it cannot do that
  exactly: the shim gives node command-line options, adds arguments of
  its own to npm's or Yarn's, starts node in another directory, changes
  NODE_OPTIONS, or does not start node within 10 s; also for corepack's
  `npm` launcher, which is not handled.
- With npm's or Yarn's own files on PATH (nvm and the others above), on
  macOS matchapack starts node without `DYLD_*` variables when npm or
  Yarn would reach node through `#!/usr/bin/env node` (or another script
  run by a system binary), because macOS removes them there; so install
  scripts see the same environment as with plain npm or Yarn.

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
- Nothing of npm is loaded before npm's own start-up code runs: the helper
  that starts npm reads pacote's `lib/fetcher.js` as a file (its fingerprint,
  below), and installs its replacement of the extraction only when npm itself
  has loaded pacote (#59). So npm's process is plain npm's, `fs` patches and
  module load order included.
- Where npm installs is npm's own answer: when npm starts the command, after
  it loaded its config, it reports its install root (`npm.prefix`), and
  matchapack runs its checks on that directory (the trial scope, a
  symlinked `node_modules`, a global install) and clones only inside it
  (#36). So `npm ci` inside a workspace of a monorepo clones into the
  workspace root's `node_modules`, and `npm ci --prefix <dir>` into that
  directory.
- Every `npm ci` flag and npm setting is npm's to read (`--omit=dev`,
  `--workspace`, `--workspaces`, `--install-strategy`, `NODE_ENV=production`,
  `.npmrc` settings; each of these is compared with plain `npm ci` on a
  workspaces fixture in `scripts/test-inject.sh`). With
  `--install-strategy=linked` npm extracts every package itself (its
  fetchers there carry no integrity): the tree is npm's, nothing is cloned.
  Packages matchapack cannot clone are extracted by npm, the rest are cloned.
  Checked with a `file:` tarball, bundled dependencies and a git dependency
  (the git dependency on npm 10.5.2, 10.9.10, 11.19.1 and 12.2.2, #74): the
  hook takes only packages that pacote fetches as registry tarballs (its
  `RemoteFetcher` and `RegistryFetcher` classes, `src/inject.js`
  `ineligible`), which a git dependency and a `file:` tarball are not (nor
  have they a registry integrity, the store key), so npm's own pacote
  extracts them (a git dependency may run its `prepare` script), and
  everything else is cloned; the tree is npm's. A shrinkwrap follows the
  same rule but has not been run yet.
- A package is cloned only when the result is exactly what npm would extract:
  a registry tarball with an integrity in the lockfile, npm's default file
  modes, umask 022. A package missing from the store is extracted into it by
  npm's own pacote (from npm's cache, or downloaded as npm would), then
  cloned: the store holds exactly what npm extracts.
- Cloned paths get the file group npm's extraction would give them (the
  project directory's on macOS; the process's, or a setgid directory's, on
  Linux), also when the store is under another group (#41).
- The store is checked against npm's extraction: every entry is compared with
  pacote's extraction once under each extraction code before that code
  clones it, entries the classic engine made included (stores filled before
  PR C2 still hold them); after that one in 50 at random
  (`MATCHAPACK_INJECT_CHECK=all`, `off` or another number: what each does
  and costs is in "How matchapack checks itself while it runs", below; `off`
  is not just fewer samples). An entry that
  differs is replaced by pacote's extraction, which is what gets installed,
  and matchapack says so. The extraction code is named by its content, not by
  version numbers (see "After an npm or Node upgrade" below). Details:
  `NOTES.md`, "Store fill through pacote" and "PR C1: the key is the code".
- `npm ci`'s own output comes back, the `npm audit` summary included (and its
  network request; `--no-audit` turns it off, as with npm).
- npm's own errors (lockfile out of sync, a failing script) are printed once,
  by npm, with npm's exit status.
- Root `predependencies` / `dependencies` / `postdependencies` scripts run, as
  in `npm ci`.
- Linux works the same way, with per-file clones (checked on btrfs and XFS in
  Docker, `scripts/linux-test.sh`).

`npm install`, `npm update` and `npm uninstall` run through the same engine
(NOTES, "The install family"): npm decides what to add, change or remove,
writes `package.json`, `package-lock.json` and the hidden lockfile, and
leaves alone what it does not change; only its extraction of the packages it
adds or changes is replaced by clones. So:
- a fresh `npm install` (no `node_modules` yet, as in a new worktree) saves
  as much as `npm ci`; `npm install <pkg>` and `npm update` save the
  packages they add or change; `npm uninstall` and an `npm install` with
  nothing to do place nothing (and print nothing of matchapack's);
- packages that were in `node_modules` before stay as they are: a
  `node_modules` built by plain npm keeps its full copies (convert it with
  one `npm ci`, or `scripts/convert-existing.sh`);
- every routed command starts matchapack, takes the store's shared lock and
  writes one log line (it waits while `matchapack store gc` or `store verify
  --fix` holds the store); the cost is about one more process, within the
  noise of npm's own run (NOTES);
- the project's record for `store gc` follows npm's hidden lockfile: when
  that file names where every installed package came from, the record is
  exactly its packages (so a record that fell behind, for example after a
  Ctrl-C, is completed by the next routed command, and entries of removed
  or replaced packages drop out); when it does not (a project without
  `package-lock.json` whose hidden lockfile npm stopped trusting), nothing
  is dropped;
- the "npm placed packages the hook never saw" check compares the packages
  of the hidden lockfile whose own `package.json` this run wrote (the
  packages npm extracted, whether new, changed or restored) with what the
  hook saw, so it stays silent on an incremental install, also where npm
  keeps a package the hidden lockfile did not list (only the root's
  `node_modules` removed in a workspaces project), where a kept package
  gets a nested `node_modules`, and with npm 10's
  `--install-strategy=linked`. It is a warning only (log and report; an
  install script or another npm can also place packages). On a filesystem
  with 1-second timestamps the count can vary between runs; nothing is
  cloned there anyway;
- if matchapack breaks during one of these commands (npm read something of
  the hook's result that pacote's own result does not have, or matchapack
  itself failed), it does not repair anything by itself: running the
  command again would not rebuild what is in place, and `npm ci` is a
  rebuild from the lockfile, not what plain `npm install` would have left
  (NOTES). It stops with a non-zero status and prints the repair that fits,
  from npm's own settings for that run: a sentence or two, then each command
  on a line of its own. Every form first removes the package directories
  matchapack itself placed in that run outside the root's `node_modules` (a
  workspace member's own packages, a package under a `file:` dependency):
  `npm ci` before npm 10.8.1, and an install, keep what they find there.
  Nothing else: not what another tool or an earlier install put there, and
  nothing behind a symlink to outside the project. Each such directory is
  on a line of its own; more than 20 are listed in a file in the run log's
  directory (readable by you only; lists older than a day are removed),
  removed by one command. Of a package directory that has a
  `node_modules` (npm keeps nested packages there and moves them back
  under a package it replaces), only the other entries are removed, and
  only while it is still the directory matchapack placed (npm puts its own
  back when it rolls back after a failed install script):
  - usually: check `git diff` of `package.json` and `package-lock.json`,
    then `MATCHAPACK=0 npm ci <the same arguments>`;
  - no lockfile to rebuild from (no `package-lock.json` or
    `npm-shrinkwrap.json`, or `--no-package-lock` / `package-lock=false`),
    or `--no-save` (not for `npm update`): `npm ci` would refuse, install an
    old lockfile or leave out what the run added, so: remove
    `node_modules` and run the same command with plain npm;
  - a command filtered to some workspaces (`-w`, `--workspaces`, or started
    inside a member): `npm ci` with the filter would remove the other
    workspaces' packages, so: rebuild the whole project,
    `MATCHAPACK=0 npm ci --prefix <root> <its options>`, without the filter.
    The options are the ones npm parsed from your command line, written out
    one per value as npm holds them: full option names, absolute paths, ISO
    dates, `--audit=false` for `--no-audit` (so abbreviations such as
    `--prefi` are gone too). Each such list is parsed again by npm's own
    parser and given only when npm reads it exactly as it read your command
    line; with `env -u` for a filter set in the
    environment (in any case: `NPM_CONFIG_workspace` too); a filter set in an
    npmrc file is named, since a command line cannot unset it. `--prefix
    <root>` run from where you are is close to, not the same as, running at
    the root: install scripts see `INIT_CWD` as the directory you ran it in
    and `npm_config_prefix=<root>` (which matters only to a script that runs
    a global npm command), and a relative path in an npmrc value resolves
    against the directory you ran it in. When npm's own parser does not read
    the options back as it read your command line (a line npm reads oddly,
    such as a list option given `false`), or npm reported none, the command
    is given without any of them, and the advice says so before it, naming
    every option it could not repeat (shown as the log shows them: secrets
    as `***`);
  - both: remove `node_modules`, reinstall the whole project the same way
    (`npm install` instead of `npm ci`), then run the command again.

  The advice works the same after a panic of matchapack (npm's settings for
  the run are kept apart). When a run breaks before npm reported its install
  root, matchapack had placed nothing, and the advice is only the same
  command again with plain npm. Credentials typed on the command line
  (`--//host/:_authToken=…`, `--_password=…`, npm's client key `--key=…`,
  a secret in a URL's query such as `?private_token=…`; not the user name
  of an ssh URL) are shown as `***`, as in the run log, and the advice says to add them back or put them in `.npmrc`.
  (`npm ci` itself is rebuilt with plain `npm ci` automatically, as
  before, after matchapack removes the package directories it placed in
  that run outside the root's `node_modules`; the root's own is left to
  `npm ci`, which empties it after checking the lockfile.) For tests, `MATCHAPACK_TEST_BROKE=1` makes a run of the family
  end as if its child status was lost, and `MATCHAPACK_TEST_BROKE=panic`
  as if matchapack panicked, after npm ran, and `MATCHAPACK_TEST_BROKE=no-server`
  as if the clone server's thread could not start (matchapack then stops
  npm before it answers).

matchapack runs plain npm instead, and prints why, when (before npm starts,
unless marked "npm's root": those are decided on the root npm reports, after
npm loaded its config and before its command ran, so the project is
unchanged; one npm debug log of about 1 KB stays in npm's `_logs`):
- the `npm` on PATH cannot be traced to npm's own package, or not started
  exactly as it starts it (a launcher it cannot ask, corepack's `npm`
  launcher, a shim that gives node options or arguments of its own; see
  "Node version managers");
- npm is not a major version this engine has been run on: 10, 11 and 12. It
  has no code per npm version, so every 10.x, 11.x and 12.x passes this check
  (`INJECT_NPM_MAJORS` in src/wrapper.rs; `MATCHAPACK_NPM_VERSIONS=any`, or a
  list such as `13.1`, overrides it; `matchapack report` counts these runs);
- npm's pacote code (the part matchapack replaces: its `lib/fetcher.js`) is
  not code this build of matchapack was tested with, as after a new npm
  release: the message names its fingerprint; update matchapack, or set
  `MATCHAPACK_NPM_FINGERPRINT=any` (or that fingerprint) to try it;
- the umask is not 022;
- npm installs globally: `-g`, `--global`, `--location=global` on the command
  line, or (npm's root) `global=true` in the environment or an npmrc;
- the install root's `node_modules` is a symlink (npm's root);
- the install root is outside the trial scope (`~/.config/matchapack/paths`;
  the start directory is checked before npm starts unless `--prefix` was
  typed, then npm's root). The entries are path prefixes: `~/git/app`
  covers `~/git/app-feature`, and a prefix that is no directory itself, or
  a directory removed since, is a good entry (its parent's symlinks are
  resolved, so `/tmp/app` matches `/private/tmp/app-feature`). A scope
  with no entry that can match (an empty or commented-out file, entries
  under directories that do not exist) and one with an entry that can never
  match (`$HOME/git` typed literally, a relative path) say so in the
  reason;
- the install root does not exist (`--prefix` naming a new directory; npm's
  root);
- the command installs nothing (`--dry-run`, `--package-lock-only`,
  `--help`; npm's root also catches them set in an npmrc);
- npm maps the routed word to another command (npm's root: `npm u` on npm
  10, where it is no command);
- `MATCHAPACK_ENGINE` names another engine (`classic` was removed in PR C2).

For `npm install`, `update` and `uninstall` (and their first words) three of
these print nothing: a global install, a project outside the trial scope
(but not a scope with an entry that can never match: that may not be the
scope meant, so it is said), and a command that
installs nothing. These commands are typed often
(`npm install -g <pkg>` above all), and the line would repeat what the user
already knows. The run is still logged as declined, with the reason
(`matchapack report` counts it). `npm ci` prints the line for every reason,
and the install family prints it for every other reason (the owner's
decision, 2026-11-15).

When the filesystem cannot clone, npm extracts every package itself, and
matchapack says so: the first clone the filesystem refuses stops cloning for
that install. The same happens, said the same way, when npm's root cannot be
read (an npm whose `Npm` class changed) or npm's pacote turns out, once npm
has loaded it, not to have the shape the replacement needs. A directory
without `package.json` or a lockfile is npm's to judge: npm finds its root
above it, or `npm ci` prints its own error (logged as `npm_failed`).

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

### How matchapack checks itself while it runs (npm)

Before every run, the fingerprint gate: pacote's `lib/fetcher.js` must be on
the list this build was tested with, or the run is plain npm (above). Then,
per package:

| Situation | What happens | How often |
|---|---|---|
| A. The package is not in the store | npm's own pacote extracts it, and that extraction becomes the store entry (after the check of its times and file kinds, "After an npm or Node upgrade") | every run, for new packages |
| B. The entry is not yet known under the extraction code that runs (the store key: pacote's `lib/fetcher.js`, the tar code Node loads, libuv's version) | pacote extracts it and it is compared file by file with the entry: equal, the key is recorded and the entry cloned; different, the entry is replaced by npm's extraction | once per entry and key: after an npm or Node upgrade that changed that code, not on every run |
| C. The entry is known under the running key | cloned without a comparison, except one in 50 at random, which goes through B's comparison (`MATCHAPACK_INJECT_CHECK`) | every run |
| D. The result trap: npm reads a field of pacote's result that a clone may not give | the run counts as broken: `npm ci` is rebuilt with plain npm; the install family stops with the repair advice | every run |

Why C does not compare everything: the same tarball (fixed by its
integrity) through the same extraction code gives the same files, and B
has already shown them equal to the entry, so a second comparison adds
nothing. The one-in-50 sample is a safety net for what the key does not
cover (a store file changed or damaged since, Node's own file-system layer,
zlib, the OS; #72), not a speed-up. To check the whole store at once after
an upgrade, run `matchapack store verify` ([After an npm or Node
upgrade](#after-an-npm-or-node-upgrade), [Store
maintenance](#store-maintenance)).

`MATCHAPACK_INJECT_CHECK` (`should_check` in src/serve.rs): a number N (50
when unset) compares one in N entries known under the running key (C) and
always compares an entry not yet known under it (B); `all` compares every
entry; `off` compares nothing at all, B included, so after an upgrade that
changed the extraction code nothing is compared before it is cloned. To
make the sample cheaper, set a larger number (`MATCHAPACK_INJECT_CHECK=200`),
not `off`. What it costs day to day (a large project, about 3,800
packages, warm store, NOTES "PR C2: a recorded digest per entry
(shelved)"): plain `npm ci` 14.6 s; matchapack with no comparison 7.5 s;
the default one-in-50 sample about 0.4 s (5 %) more per reinstall (7.8 to
8.0 s). The fingerprint gate and the result trap cost milliseconds; B
happens once per entry after such an upgrade, and `store verify` can do it
ahead of time.

The Yarn 1 engine has no such comparison while it runs: it clones from
Yarn's own cache at Yarn's link step, with no store and no extraction to
compare with. That is why the sampled verification of real runs
(`scripts/verify-sample.sh --yarn`, "Trial: logs, checks, scope") matters
more for Yarn.

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
- The commands that copy packages into `node_modules` go to `matchapack
  yarn`: a bare `yarn`, `yarn install …`, `yarn` with a flag first (`yarn
  --frozen-lockfile`), `yarn add`, `yarn upgrade`, `yarn remove`, `yarn
  upgrade-interactive`, and `yarn workspace <name>
  add|remove|upgrade|upgrade-interactive …` (the Yarn that `yarn workspace`
  starts for the command runs with the hook too). Each prints matchapack's two lines and costs about 0.25 s of CPU
  more than plain Yarn (measured on `yarn add` of a 3-file package). A
  `yarn workspace` command costs more, not measured: it starts two hooked
  Nodes, probes twice, and asks `yarn cache dir` only after the
  workspace's Yarn has started.
- Every other command runs Yarn directly; matchapack does not start. Which
  ones and why: "What stays plain Yarn" below.
- Git dependencies go through Yarn's cache like any package, and are
  cloned at the link step like any package (a real project with a git
  dependency came out identical to plain Yarn: `NOTES.md`, "Yarn 1 trial
  on real projects", the codev row).
- `file:` dependencies, a directory or a tarball, also go through Yarn's
  cache and are cloned at the link step. Yarn copies a `file:` directory
  into a new cache entry on every install (plain Yarn's own behaviour: the
  cache gains one entry per install), and unpacks a `file:` tarball into
  an entry keyed by its sha1, which later installs reuse. Checked: trees
  identical to plain Yarn for a directory, a tarball, both together with
  registry packages, and a workspace that depends on a sibling through
  `file:../lib` (`fixtures/yarn1-file` tests the first two, the directory
  and the tarball; the other two were checked once, on scratch fixtures,
  NOTES "Yarn 1 `file:` dependencies"). Plain Yarn ignores
  edits to a `file:` directory while `.yarn-integrity` still matches
  ("Already up-to-date"); so does matchapack.
- matchapack leaves Yarn's first 128 file copies of each command to Node
  (so that Yarn's own time check sees real copies). A `yarn add` that
  copies 128 files or fewer therefore clones nothing, however large the
  files are (`typescript`: 117 files, 30.9 MiB, none cloned; `date-fns`:
  4,783 files, 94 % of its bytes cloned). On a full install the 128 are
  noise.
- The first `yarn add` in a new project, before it has a `yarn.lock`, is
  plain Yarn (the gate below), so its packages are real copies until the
  next install over an empty `node_modules`. Unless the project sits
  below another project's `yarn.lock`: matchapack finds the project by the
  nearest `yarn.lock` at or above the directory, so then it runs, with
  clones (the tree is still Yarn's).
- In a Yarn 2+ project (a `.yarnrc.yml`, `packageManager: yarn@2` or later,
  or a Yarn 2+ lockfile) `matchapack yarn` runs plain Yarn at once. It
  decides from those files only, without starting node, and prints and
  logs nothing. This adds a few milliseconds to a `yarn install`.
- A bare `yarn` in a Yarn 1 project that is already installed is still an
  install: Yarn checks `.yarn-integrity`, finds nothing to do and stops.
  matchapack adds about 30–40 ms to that (0.25 s instead of 0.21 s on one
  project).

**What stays plain Yarn, and why.**
- `yarn global add|remove|upgrade …`, and `yarn create` (which runs `yarn
  global add`): they install into Yarn's global folder, outside any project;
  matchapack clones only into a project's `node_modules`.
- `yarn workspace <name> install`: Yarn installs at the workspace root, the
  same install as `yarn install` there, which is routed. The other `yarn
  workspace <name> …` commands (`run`, a script, `why`) copy no package.
- `yarn workspaces …`, `yarn run` and scripts, `yarn why`, `info`, `list`,
  `outdated`, `cache`, `link` / `unlink` (a symlink), `pack`, `publish`,
  `import`, `check`, `autoclean`, `policies`: they copy no package into
  `node_modules`.
- `yarn --version`, `yarn --help`, and any command with `--help` / `-h`.
- Routed, but handed to plain Yarn by the checks in "When it runs plain Yarn
  instead": among them the first `yarn add` of a project that has no
  `yarn.lock` yet (matchapack finds the project by its lockfile, the nearest
  one at or above: a project below another one's `yarn.lock` is not
  declined), and a Yarn older than 1.10.0 (it asks for no clone).

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
- the Yarn is older than 1.10.0, the first Yarn 1 that asks for a clone
  (an older one would cost a start-up for nothing);
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
  Every routed npm command that reaches matchapack appends one line to
  `~/.cache/matchapack/log.jsonl` (`MATCHAPACK_LOG=0` turns it off), with
  npm's command (`command`: ci, install, update, uninstall) and npm's install
  root (`root`); the report counts runs per command, and gives install
  times for `npm ci` and for the install family apart (a no-op
  `npm install` takes a fraction of an `npm ci`). Each
  line carries the 1- and 5-minute load average at the start and the end of
  the run (`load_start`, `load_end`); the report's recent runs show the
  1-minute pair (`load 3.1→5.4`), so a slow install can be read against what
  else the machine was doing. Lines that are not readable JSON are skipped
  and counted.
- `scripts/verify-sample.sh [N]` (npm) and `scripts/verify-sample.sh --yarn
  [N]` (Yarn 1): sampled verification of real runs. For each of the N
  (default 2) most recent projects in the run log, one logged run is
  replayed on copies, plain and through matchapack, and the trees are
  compared: the project's newest replayable run that cloned something
  (else its newest, which is then skipped). Projects that are skipped do
  not count: it goes on to older ones until N replays ran or every logged
  project was looked at (at most 20 skips are recorded per run; the search
  goes on past them). One line per project goes to
  `~/.cache/matchapack/verify.jsonl`, and `matchapack report` shows npm's
  and Yarn's samples side by side, with the command each replayed; it
  counts samples, not distinct runs (a logged run is sampled again on every
  run of the sampler until a newer one of its project replaces it). Which
  runs:
  - full installs: `npm ci` with its logged words (npm ci twice on copies
    of the whole project, so build noise is told apart, and `matchapack npm
    ci` once); `yarn` / `yarn install`, with or without `--frozen-lockfile`
    (with the logged run's Yarn, node, cache folder and flags; plain Yarn
    three times, one of them to make sure the cache holds every package,
    and `matchapack yarn` once; compared as `scripts/verify-yarn1.sh
    --engine yarn1` does; an install that ran over an existing
    `node_modules` is replayed from scratch and says "replayed fresh");
  - commands that started with no `node_modules` (the root's and every
    workspace's), as in a fresh clone: `npm install` / `update` /
    `uninstall`, `yarn add` / `upgrade` / `remove` (and `yarn workspace
    <name> …`), replayed with the same words from the files before the
    run. The run log keeps a SHA-256 of the lockfile and of each manifest
    before and after every run, and whether `node_modules` was there
    before; the files before come from the project while they still have
    those hashes, else from its git HEAD when it has them.

  A full install is replayed from the files as the run left them, a
  command from the files before it; the set of files must be the run's
  too (the same workspaces, all of them found). The copies hold the
  manifests (`package.json` files, the lockfile, `.npmrc`; for Yarn also
  `.yarnrc` and `.yarnclean`), not the rest of the project (except for
  `npm ci`); they are removed after each comparison. A root script that
  needs the project's sources then fails in plain npm or Yarn too: that
  sample is an ERROR with the reason; when plain runs pass, the comparison
  stands.

  A sample says IDENTICAL only when the replay's inputs equal what the
  logged run recorded (the files it hashed, the project's rc files among
  them, the words it logged, and the environment's npm or Yarn settings),
  matchapack's run in the replay
  cloned as many files (Yarn) or packages (npm) as the logged run, and
  every check of the comparison passed. When the sampler cannot be sure
  the replay repeated the run, it does not say IDENTICAL. The environment
  variables npm or Yarn read as settings (`npm_config_*` in any case; for
  Yarn also `YARN_*`; `NODE_ENV`) are logged as their names and one
  SHA-256 over their values, never the values (they can hold tokens); a
  sampler whose environment has other ones SKIPs the run and names them.
  The project's own rc files are hashed with the manifests (npm: the
  root's `.npmrc`; Yarn: `.yarnrc` and `.npmrc` in the root and in each
  workspace): one changed since the run is a skip. The user's and the
  global rc files (`~/.npmrc`, `~/.yarnrc`, npm's global npmrc) and a file
  named by `--use-yarnrc` are neither logged nor hashed: a limit of the
  sample. An equal clone count catches some changes there (a replay that
  installs more or fewer packages), not all (`ignore-scripts` changes no
  count). Only a Yarn install that ran over an existing `node_modules`
  ("replayed fresh") may clone more. The results:
  - IDENTICAL; for Yarn also when two plain runs differ and matchapack's
    tree is the first plain run's, or when, run again with one libuv pool
    thread, plain Yarn still varies at every place where matchapack
    differed in the first run, and matchapack differs only where plain
    varies (the paths are kept);
  - DIFFERENT: the trees differ, a check failed (npm: a stale time, `npm
    ls`, the bytes of `package.json` or the lockfile), matchapack failed
    where plain npm or Yarn installed, or (Yarn) the engine's own record
    breaks its rule of leaving Yarn's first copies to Node;
  - ERROR, with the reason: the replay could not test anything or did not
    repeat the run (plain npm or Yarn failed in the copies; matchapack
    declined; matchapack cloned another number of files or packages than
    the logged run did);
  - RACE-SHAPED (Yarn): a difference with the shape of plain Yarn's own
    probe race (#57: mtimes of non-empty package files only). It is run
    once more with one libuv pool thread, where that race cannot happen:
    still there, DIFFERENT; gone, it stays RACE-SHAPED (a faulty engine
    need not repeat itself either), with the second verdict in the record.
    So does a first run in which matchapack differed only where plain Yarn
    varied, when with one thread plain Yarn does not vary and matchapack is
    identical (a difference of matchapack's own could have hidden there).
    Neither a pass nor a fail: sample that project again;
  - SKIPPED, with the reason. Both engines: the project or the directory
    the run started in is gone; its lockfile or a manifest changed since
    (for a command: and git HEAD does not have the files before), or its
    workspaces did, or they cannot all be found, with the reason (a
    workspace pattern with character classes, extended globs (`@(a|b)`;
    a plain `@`, `+` or `!` in a name, as in `packages/@scope/*`, is
    fine) or braces other than `{a,b}`, a
    negated pattern, one outside the project, a workspace that is a
    symlink, a listing that failed, or the listing budget reached); an
    argument was hidden in the log (a secret); the logged run cloned
    nothing; a dependency on a local path (`file:`, `link:`, `portal:`,
    `./`, `../`, `/`, `~/`: the copies do not hold what it names); a record
    from an older build. After installing a new build, every older record
    is skipped this way ("does not say whether its file set is complete"),
    until the projects are installed again. npm only: npm on
    PATH is another version; an argument is an absolute path or holds a
    line break; the run started inside the project (a workspace member,
    `--prefix`). Yarn only: node or Yarn is gone or another version; a
    relative `--use-yarnrc`; an `.npmrc` / `.yarnrc` above the project
    (Yarn reads them where it runs, not in the copies); Yarn's cache
    unusable in the run; `upgrade-interactive` (its choices are not
    logged).

  Cost: each sample installs the project three or four times (a Yarn
  sample that needs the one-thread run, seven or eight). The replays use
  the caches and the store the runs used: plain npm and Yarn may add to
  them as a real install would, and matchapack's runs fill the store with
  packages it lacks and may replace an entry that differs from npm's
  extraction, as any install does. Not covered: commands run in a tree
  that already had `node_modules` (their result depends on that tree:
  issue grimmerk/matchapack-private#77 part 2); npm samples compare paths, contents, modes and
  `npm ls`, not file times, owners or extended attributes (Yarn samples
  do). The install family is also verified by `scripts/verify-steps.sh`
  (below).
- Limit the trial to some projects: list path prefixes, one per line, in
  `~/.config/matchapack/paths` (e.g. `~/git/app` also covers `~/git/app-<topic>`
  worktrees). Elsewhere npm runs as usual.

Known differences from plain npm: none in the tree, `package.json` or the
lockfiles (see "How it installs").

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

## After an npm or Node upgrade

**The first install can be slower.** Each store entry records the
extraction code it is known to match: pacote's `lib/fetcher.js`, the tar
files Node loads (with tar's dependencies), and libuv's version (Node's
file-system library). When an upgrade changes any of them, every entry is
compared once with npm's own extraction before it is cloned again: about
twice a plain `npm ci` for the packages that install uses, once per entry
(a second project using the same packages compares nothing), and the
summary says so (`this npm's extraction code was new to the store: N
packages were compared …`). `matchapack store verify` after the upgrade does
the same work ahead of time for the whole store. An npm release that changes
none of that code (often only pacote's version number moves) compares
nothing; switching back and forth between two npm or Node versions compares
nothing again; Node's own version is not part of it. The first build with
this rule compares every entry once, since older records name versions
(`pacote@… tar@…`), not code. If pacote's extraction ever stops being what a
clone reproduces (a file or directory more than an hour older than the
install, or something other than files and directories), matchapack stops
cloning in that install and says so: when something had already been
cloned, plain `npm ci` rebuilds the install, and `npm install`, `update`
and `uninstall` stop with the repair advice (as when a run of them breaks,
above); when nothing had, the install is npm's own and stands. Either way the code goes on the store's decline
list, and later installs under it are npm's own (the message names the file
to delete). To try that code again, delete its file: `<store>/declined/` holds one
file per declined code, the key on its first line and the reason on the
second (`grep -lx '<key>' <store>/declined/*` finds it; deleting the
directory clears them all). `matchapack report` lists the tar code each run loaded.

matchapack takes every npm 10, 11 and 12 release whose pacote code it was
tested with: it has no code per npm version, and what keeps it right on a new
release is checked while it runs (pacote's fingerprint before npm starts and
its shape when npm loads it, every store entry compared with the new extraction code's extraction
before its first use, packages npm placed that the hook never saw, npm reading
what the hook returns). Most npm releases do not change that pacote code. After
an upgrade the harness is still worth running:

```sh
scripts/verify-npm.sh /tmp/verify-npm              # the fixtures
scripts/verify-npm.sh /tmp/verify-npm ~/git/app    # plus a real project (copied, never changed)
```

It runs `npm ci` and matchapack side by side on copies and compares the trees
and the hidden lockfile byte for byte (the workspaces fixture also file
times, owner, group and extended attribute names), then the install family:
about 40 cases of `scripts/verify-steps.sh`, each a sequence of npm commands
(`npm install`, `npm install <pkg>`, `update`, `uninstall`, workspaces,
`--prefix`, the flags `--force`, `--legacy-peer-deps`, `--save-peer`,
`--save-optional`, …) run with plain npm and through matchapack from the same
start and compared after every step, `package.json` and `package-lock.json`
byte for byte included. It says whether this npm's
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
  through the trial log, or recorded with `store adopt`, below). Removing a
  package never breaks an installed node_modules, but disk space comes back
  only for packages no node_modules still shares blocks with, so the real run
  reports the volume's free space before and after. Packages extracted in the
  last hour are kept. Refused while an install is running.
- `matchapack store adopt [--dry-run] [--verbose] <dir>...`: record the npm
  projects under each `<dir>` so that gc keeps their packages. For projects
  installed before installs wrote records (builds before PR grimmerk/matchapack-private#29): it finds
  every directory with npm's hidden lockfile (`node_modules/.package-lock.json`)
  and records it with the store packages that lockfile lists, as an install
  would. A project installed by plain npm has a hidden lockfile too and is
  recorded as well: its packages are kept, which only means less space comes
  back. An existing record is added to, never narrowed; running it twice
  changes nothing. It follows no symlink, never enters a `node_modules`, a
  `.git` or the store, skips a symlinked `node_modules` and what it cannot
  read. `--dry-run` lists each project and what it would record. Yarn
  projects need no record (the Yarn engine clones from Yarn's cache, not the
  store). About 8 s for `~/git` here (217,000 directories, 81 projects).
- `matchapack store verify [--fix]`: compare every package with what npm's
  own pacote (the npm on PATH) extracts from its tarball in the npm cache, the
  way the inject engine has it extracted: files, contents, file and directory
  modes. Exit 1 when one differs; each difference names who made the entry
  (the classic engine's extractor, in stores filled before PR C2; this
  extraction code; or other extraction code).
  `--fix` replaces those with pacote's extraction. A matching entry is
  recorded as known to that extraction code, so the next install samples
  it instead of comparing it first. About 3 minutes for 8,779 packages on a
  busy machine (the classic engine's Rust extractor took about 1.5): each
  tarball is extracted again, by node.

Before the first real gc on a store filled by older builds:

```sh
matchapack store adopt --dry-run ~/git   # what would be recorded
matchapack store adopt ~/git             # every place you keep npm projects
matchapack store gc --dry-run            # check what would go
matchapack store gc
```

A wrong gc decision never breaks a project: its files are clones, independent
of the store entry they came from (tested: `scripts/test-store.sh` removes the
entries of a project nobody recorded and its files read the same). It only
loses the block sharing, and that project's next install extracts those
packages again.

## Other commands

- `matchapack init zsh|bash|fish [--alias NAME]`: print the shell
  integration (see "Use"); `--alias NAME` also defines NAME for matchapack.
- `matchapack uninstall [--yes]`: remove matchapack's own state. Without
  `--yes` a dry run that writes nothing. It removes only what it recognises
  as matchapack's, resolved as every other command resolves it
  (`MATCHAPACK_STORE`, `MATCHAPACK_LOG_DIR`, `MATCHAPACK_CONFIG_DIR`, else
  `~/.cache/matchapack/store`, `~/.cache/matchapack`,
  `~/.config/matchapack`), and only the names matchapack makes in them. It
  refuses `/`, the home directory or a parent of it, a symbolic link, a
  store without its `v2/` layout directory, and a directory one of those
  variables names that holds anything else; in a default directory other
  names are left in place. Any refusal removes nothing. Refused too while an
  install holds the store lock or a matchapack process holds a directory in
  the store's `tmp/` (the Yarn engine's helper files). Space: removing the
  store frees only the blocks no project shares; the dry run estimates it
  from the project records, the real run prints the volume's free space
  before and after. `~/.cache/matchapack/backup` (backups
  `scripts/convert-existing.sh` kept) is left for you to check and remove.
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
  `diff --meta`; exit 0 identical, 3 "plain varies" (with the paths).
  `--engine yarn1` adds a third copy installed by `matchapack yarn`;
  `--command 'WORDS'` runs a Yarn command after the install (with
  `--command-fresh`, instead of it, on a copy with no `node_modules`);
  `--cache-dir`, `--args-as-given` (no default `--frozen-lockfile`) and
  `--engine-min-clones any` are what `verify-sample.sh --yarn` uses; the
  script's header lists every option. `scripts/test-yarn1.sh <work>`: its
  tests on the `fixtures/yarn1-*` projects (`TEST_YARN1_PART=sample`: the
  sampled verification's).
- `scripts/verify.sh <project> <work>`: `npm ci` vs matchapack on copies of a
  project (`MATCHAPACK_VERIFY_VIA=wrapper` goes through the shell function). It
  runs on any npm version and ignores the trial scope file, since it is how a
  new npm version gets verified. `MATCHAPACK_VERIFY_BYTES=1` also requires the
  hidden lockfile to be byte-identical (CI sets it); `MATCHAPACK_VERIFY_META=1`
  also compares file times outside the two runs, owner, group and extended
  attribute names (`diff --meta`). `MATCHAPACK_VERIFY_ARGS_FILE=<file>`
  runs the npm command in the file (its arguments, each ended by a NUL
  byte, `ci` included) instead of plain `npm ci`, through `matchapack npm`
  in B, and also requires `package.json` and the lockfile to be
  byte-identical (what `verify-sample.sh` uses for every replay).
- CI (`.github/workflows/ci.yml`, on pull requests): `cargo test` and
  `test-shells.sh` on Linux; on macOS the safety, routing, store and inject
  tests, the fixture harness and the Yarn 1 harness tests.
- The local tarballs (`fixtures/optional-upward/tarballs`): the fixtures name
  them by the placeholder host `127.0.0.1:48321`. Each suite that installs
  them (test-inject, verify-npm, test-store, test-yarn1, verify-yarn1,
  test-shells `--real`, linux-test) starts its own server on a free port and
  installs from a copy of `fixtures/` in its work directory whose URLs (and
  the hashes of the tarballs that name the host) are rewritten to that port
  (`scripts/fixture-server.sh`). Suites from several checkouts therefore run
  at the same time with no lock, and none uses a server it did not start
  (`verify-yarn1.sh` run by `test-yarn1.sh` uses its parent's): a server
  that does not answer the copy's token, or a copy made from another
  checkout's fixtures, stops the suite (status 2).
  The server stops when its suite exits. To install a fixture by hand, serve
  the tarballs on the placeholder: `python3 -m http.server 48321 --bind
  127.0.0.1 --directory fixtures/optional-upward/tarballs`.
- `scripts/verify-steps.sh <project> <work> [--start bare|ci|nolock] [--pre
  cmd] -- <step> [-- <step> …]`: the install family's harness. The same npm
  commands (each `[@subdirectory] [NAME=value …] <npm arguments>`) with plain
  npm and through `matchapack npm`, from the same start, compared after every
  step: exit status, the whole tree with the hidden lockfile byte for byte,
  `package.json` and `package-lock.json` (each by name), whether npm trusts
  the hidden lockfile, `npm ls`, stale times, and matchapack's log record
  (command, root, unseen count, the `store gc` record). `CLONES=…` and
  `UNSEEN=…` in a step are expectations. verify-npm.sh runs its cases.
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
