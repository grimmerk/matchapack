# matchapack

matchapack makes `npm ci`, `npm install` and Yarn 1 installs share disk space between checkouts. Ten git worktrees of one project take about the disk space of one, a fresh worktree installs in 70 to 80 % of the time, and every `node_modules` is the one plain npm or Yarn would build.

Works with npm 10, 11 and 12 on macOS (APFS) and on Linux with Btrfs or XFS, and with Yarn 1.10 or later on macOS.

**Status:** 0.1.0, new. In its author's daily use on macOS since November 2026. On Linux it runs in continuous integration (CI) only, and its install time has not been benchmarked there.

[Install](#install) · [Use](#use) · [Do you need it?](#do-you-need-matchapack) · [Limits](#limits) · [FAQ](docs/faq.md) · [How it works](docs/architecture.md) · [How it is tested](TESTING.md) · [Install with Claude Code](INSTALL-WITH-CLAUDE.md)

<p align="center"><img src="docs/assets/matchapack-hero.png" width="300" alt="A large three-layer matcha cake stands for the store. Beside it, two small blocks in the same shade are two worktrees of one project, cloned from the store, and a third block in another shade is a second project that takes its packages from the same store."></p>

① The first install of a package puts one copy of it in the store (`~/.cache/matchapack/store`).
② Every later install of that package, in any checkout, places a copy-on-write clone of it: a new file that shares its disk blocks with the store's copy.
③ Editing a cloned file gives that checkout its own copy of the changed blocks; the store and the other checkouts do not change.

## Why

Every checkout of a JavaScript project needs its own `node_modules`. If you keep several git worktrees of one project (one per branch, or one per AI coding agent), each `npm ci` writes another full copy: 2 GiB for a large frontend project, 20 GiB for ten worktrees, almost all of it the same files.

## What it does

matchapack runs your own npm or Yarn 1 and changes one step inside it. Where npm would unpack a package, or Yarn would copy a file from its cache, matchapack makes a clone (`clonefile` on APFS, the Mac's file system, or a reflink on the Linux file systems Btrfs and XFS) of a copy it already has. Everything else is npm's or Yarn's own work: which packages, where they go, install scripts, bin links, `package-lock.json`, `yarn.lock`, npm's hidden lockfile (`node_modules/.package-lock.json`) and Yarn's `.yarn-integrity`.

The tree is the one plain npm or Yarn builds: the same paths, contents, permissions and symlinks, file times set by the same rule as npm's or Yarn's own, and the same lockfiles, byte for byte. [TESTING.md](TESTING.md#what-identical-means) says how, and where, each of these is checked, including byte-for-byte comparisons on two public projects that anyone can repeat. Nothing is added to the project: no config file, no `package.json` change, so nobody else on your team has to change anything.

## Do you need matchapack?

Maybe not:

- **Your project can use pnpm.** pnpm keeps one store too, and clones or hard-links from it. matchapack is for projects that stay on npm or Yarn 1.
- **You keep one checkout per project.** There is nothing to share (a reinstall is still about 0.8 of the time).
- **Yarn 1 on Linux with Btrfs or XFS:** Yarn's own copy already clones there.
- **Your disk is ext4** (most Linux installs), or you use Windows: nothing can be cloned.
- **CI or Docker builds** that start from an empty disk: there is no second checkout to share with.

| | Changes the project | An edit in one checkout changes the others | Tree identical to npm's |
|---|---|---|---|
| plain npm | no | no | yes |
| hard links into a shared copy | no | yes (one file in many places) | no: one file has one set of times and modes |
| pnpm | yes: another package manager, lockfile and layout | no where it clones; yes where it hard-links | no: its own layout |
| matchapack | no | no | yes |

## Numbers

Measured with [`scripts/bench.sh`](scripts/bench.sh) on public projects at pinned commits:

| Project | One `node_modules` | 10 worktrees, plain | 10 worktrees, matchapack | Once per machine |
|---|---|---|---|---|
| small (npm, 347 packages) | 84 MiB | 835 MiB | 2 MiB | the store, 77 MiB |
| medium (npm, zwave-js-ui, 1,612 packages) | 635 MiB | 6.20 GiB | 8 MiB | the store, 619 MiB |
| large (npm, Superset's frontend, 2,709 packages) | 2.07 GiB | 20.68 GiB | 61 MiB | the store, 1.82 GiB |
| Yarn 1 (cornerstone3D, 2,498 packages) | 1.19 GiB | 11.91 GiB | 10 MiB | Yarn's own cache, 1.88 GiB, which plain Yarn keeps too |

The 10-worktree columns are ten times the measured cost of one extra worktree. The matchapack column counts only the files of a worktree's `node_modules` that share no disk blocks with the store (or with Yarn's cache). It is an estimate; [TESTING.md](TESTING.md#benchmark) says how it is made.

Once the store (for Yarn 1, Yarn's cache) has the packages, a fresh worktree also installs faster, because cloning a package is quicker than unpacking it:

| Project | Plain | matchapack | Time saved |
|---|---|---|---|
| small | 1.54 s | 1.06 s | 31 % |
| medium | 6.57 s | 4.71 s | 28 % |
| large | 17.75 s | 13.02 s | 27 % |
| Yarn 1 | 14.23 s | 11.64 s | 18 % |

What it costs (matchapack's time ÷ plain npm's or Yarn's; above 1 is slower):

| When | Cost |
|---|---|
| The first install of a package, when the store does not have it yet | 1.04 to 1.27 for a whole install with an empty store (Yarn 1: 1.05 with an empty Yarn cache) |
| The first install after an npm or Node upgrade that changes npm's extraction code | each package in the store is compared once with npm's own extraction: 1.7 to 2.8 for a whole `npm ci`, once per package. Over npm 10.0.0 to 12.2.2, that code changed in 23 of 73 release steps |
| Yarn 1 commands with little to do | a fixed cost per command: 0.89 s instead of 0.49 s on an already installed project, 3.74 s instead of 2.43 s for an incremental install after a lockfile change |

Conditions: an Apple M4 Mac (10 cores, 32 GiB), macOS 27, APFS; Node 24.22.1, npm 11.19.1, Yarn 1.22.22; each case in 10 interleaved rounds, medians; npm's cache warm on both sides (download time is not measured). A second run while the machine was busy gave the same conclusions. The full tables, the method and both runs are in [TESTING.md](TESTING.md#benchmark).

## Install

You need: macOS (Apple silicon or Intel) with APFS, or Linux (x86_64 or arm64) with Btrfs or XFS; Node with npm 10, 11 or 12 (and Yarn 1.10 or later for Yarn projects on macOS); zsh, bash or fish.

1. **Install the binary**, with Homebrew or with the install script (it puts the binary in `~/.matchapack/bin` and adds that directory to PATH):

   ```sh
   brew install grimmerk/tap/matchapack
   # or
   curl --proto '=https' --tlsv1.2 -LsSf https://github.com/grimmerk/matchapack/releases/latest/download/matchapack-installer.sh | sh
   ```

2. **Add one line at the end of your shell's startup file.** Not sure which shell you use? Run `echo $SHELL`.

   | shell | file | line |
   |---|---|---|
   | zsh | `~/.zshrc` | `command -v matchapack >/dev/null && eval "$(matchapack init)"` |
   | bash | `~/.bashrc` | `command -v matchapack >/dev/null && eval "$(matchapack init)"` |
   | fish | `~/.config/fish/config.fish` | `type -q matchapack; and matchapack init \| source` |

   It must come after every line that puts matchapack or Node on PATH (the install script's `. "$HOME/.config/matchapack/env.sh"`, Homebrew's `brew shellenv`, rustup's, a Node version manager's); if it comes earlier, it does nothing. bash on macOS: Terminal opens login shells, which read `~/.bash_profile`; make sure that file loads `~/.bashrc`, or put the line there.

3. **Open a new terminal and check:** `type npm` says that `npm` is a shell function.

4. **Optional, only some projects:** list path prefixes, one per line, in `~/.config/matchapack/paths` (`~/git/app` also covers `~/git/app-feature`). Elsewhere npm and Yarn run as usual.

Other ways to install: `npm i -g matchapack` (one package with every binary, no install script), or `cargo install matchapack --locked` (builds from source).

Notes:

- **nvm, fnm and other Node version managers:** a global npm package belongs to the Node version that was active. After you switch versions, `matchapack` is not on PATH, and npm runs plain without a message, until you install it there too. Homebrew and the install script do not depend on the Node version; `matchapack init` run by hand says when this applies.
- **Changing how you installed it:** the install script appends its PATH line at the end of your startup files. If an init line from an earlier install is above it, move the init line below it.
- **`mp`:** the line also defines `mp` as a short name for `matchapack` (`mp store gc`), but only when nothing named `mp` exists when the line runs (no alias, function, builtin or command on PATH; checked at every shell start). `--alias NAME` picks another name, `--no-alias` none.
- **Claude Code:** paste the prompt in [INSTALL-WITH-CLAUDE.md](INSTALL-WITH-CLAUDE.md), and Claude installs and sets it up, asking before each step.

### Updating

`brew upgrade matchapack`; or run the install script again; or `npm i -g matchapack@latest` (in each Node version that has it); or `cargo install matchapack --locked --force`. The line in your startup file needs no change: new terminals get the new version's shell code.

## Use

Type `npm ci` as usual:

```
$ npm ci
matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)
…npm's own output: warnings, script output, "added 347 packages in 1s", the audit summary…
matchapack: added 347 packages from the store in 0.8s (inject engine; npm extracted 0)
```

`npm extracted N` counts the packages npm unpacked itself, such as git dependencies. `npm install`, `npm update` and `npm uninstall` go the same way, more quietly: matchapack adds a line only when it installed a package or has something to say (a hand-over to plain npm, a warning). npm writes `package.json` and `package-lock.json` itself, as always. A fresh `npm install` in a new worktree saves as much as `npm ci`.

On macOS, in a Yarn 1 project, `yarn`, `yarn install`, `yarn add`, `upgrade`, `remove` and `upgrade-interactive` go through matchapack too:

```
$ yarn install --frozen-lockfile
matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)
…Yarn's own output…
matchapack: 56914 files cloned from Yarn's cache (425.3 MiB), 128 copied by Node (Yarn's first copies, left to Node by design), in 25.2s
```

**Where it applies.** The line defines shell functions named `npm` and (on macOS) `yarn` in your interactive shells. What does not run through such a shell runs plain npm: a script, a Makefile, an editor's task runner, CI, or a coding agent that starts its own non-interactive shell. Call matchapack by name there: `matchapack npm ci`, `matchapack yarn install`. Claude Code sessions started after you added the line use the functions (they take them from their shell snapshot). An existing `alias npm=…` wins over the function, so matchapack is skipped. For one command, `MATCHAPACK=0 npm install` or `command npm install` runs plain npm.

**Is it working?** `type npm` says it is a shell function. `du` and Finder still show the full size of every `node_modules`, because they count a cloned file in each place it appears. The saving shows in the free space of the disk (`df -h .` before and after a second worktree's install) and in `matchapack report`. The first install of a package fills the store; every later one clones it.

**Try it first.** Without the shell line: install the binary, run `matchapack npm ci` in a spare worktree of an npm project, read `matchapack report`, then remove everything with `matchapack uninstall --yes` and the binary. To check the result on your own project, [`scripts/verify.sh`](scripts/verify.sh) `<project> <work>` installs two copies, plain and through matchapack, and compares them.

| Command | What it does |
|---|---|
| `matchapack report [--days N]` | what happened: installs, clones, hand-overs and why, time, space saved |
| `matchapack store` | where the store is and what it holds |
| `matchapack store gc [--dry-run]` | removes packages no installed project uses; installed `node_modules` never depend on the store |
| `matchapack store adopt <dir>` | records projects installed before records existed, or moved, so that gc keeps their packages |
| `matchapack store verify [--fix]` | compares every package in the store with npm's own extraction (after an npm or Node upgrade, ahead of the next installs) |
| `matchapack npm …`, `matchapack yarn …` | what the shell functions run, for scripts and CI |
| `matchapack uninstall [--yes]` | removes matchapack's own state ([Uninstall](#uninstall)) |

Existing `node_modules` keep their full copies until they are installed again: run `npm ci` once (for Yarn 1, remove `node_modules` and run `yarn`), or [`scripts/convert-existing.sh`](scripts/convert-existing.sh) for every project under a directory, with a backup.

### When something looks wrong

- **No `matchapack:` line:** check `type npm` in that terminal; open a new one after editing the startup file; check that the line comes after the PATH lines.
- **`running plain npm instead (…)`:** the reason says why and what to do; [docs/architecture.md](docs/architecture.md#when-it-runs-plain-npm-instead) lists every one.
- **Nothing saved:** the project and the store must be on the same file system (two volumes of one disk count as two); `du` shows full sizes anyway (above).
- **After an npm or Node upgrade:** the first install can be slower (Numbers, above); `matchapack store verify` does that work ahead of time.
- **After switching Node with nvm or fnm:** see Notes under [Install](#install).
- **A broken `npm install`, `update` or `uninstall`:** matchapack stops with an error and prints the commands to repair the project; run them. (`npm ci` is rebuilt with plain npm by itself.)

## Uninstall

`matchapack uninstall` shows what it would remove (the store, the run log, the settings directory) and how much space that frees; `matchapack uninstall --yes` removes it. It never edits your startup files: it lists the lines that mention matchapack, for you to remove. Its last line is the exact command that removes the binary, from where it really is; for example:

| Installed with | Remove the binary with |
|---|---|
| Homebrew | `brew uninstall matchapack` |
| npm | `npm uninstall -g matchapack` (with nvm or fnm, in each Node version; uninstall prints it with the right `--prefix`) |
| the install script | `rm ~/.matchapack/bin/matchapack`; then remove the PATH line from your startup files, and only after that the files uninstall lists |
| cargo | `cargo uninstall matchapack` |

Volta, mise and a cargo `--root` install get their own command. Installed `node_modules` keep working without matchapack, and a later `npm ci` or `yarn` without the line is plain npm or Yarn.

## Limits

| What | What happens | What to do |
|---|---|---|
| A file system without clones (ext4), or the store and the project on two file systems (two volumes of one disk count as two) | npm or Yarn installs everything itself: the same tree, no saving | keep projects on APFS, Btrfs or XFS, and set `MATCHAPACK_STORE` to a directory on the projects' volume |
| npm other than 10, 11 or 12, or a new npm release whose unpacking code this build was not tested with | plain npm, and it says so | update matchapack |
| Yarn 2 and later (Plug'n'Play included); Yarn 1 on Linux | plain Yarn | nothing: on Linux Yarn 1 already clones |
| Windows | not supported | |
| Intel Macs, Linux on arm64 | release binaries are built, but no test suite runs on them | |
| Scripts, editors, CI, non-interactive shells | plain npm (no shell function there) | `matchapack npm ci` |
| npm options before the command word (`npm --prefix app install`) | plain npm | write them after it: `npm install --prefix app` |
| `npm dedupe`, `prune`, `audit fix`, `install-test`, `install-ci-test`, `link`, `exec` / `npx`, global installs | plain npm | |
| Lockfile entries without `resolved` or `integrity`, git and `file:` dependencies, `--install-strategy=linked` | npm extracts those packages (with `linked`, all of them) itself: no saving for them | |
| A package that downloads files in its install script (Prisma's engines, for example) | downloaded as with plain npm or Yarn | |
| A small `yarn add` (128 files or fewer) | nothing cloned: Yarn's first 128 copies are left to Node by design | |
| A future Node release with a known change in how it copies files (not yet released) | Yarn 1 installs run as plain Yarn: the tree stays right, nothing is cloned | update matchapack once it supports that Node |
| A umask other than 022 | plain npm | |

If matchapack itself is killed with SIGKILL during an install, npm stops too: it rolls back its half-done install, finishing after matchapack has exited; but when matchapack had first been asked to stop (a supervisor's SIGTERM, then SIGKILL), npm is killed without its rollback, and the project needs a new `npm ci`. Details: [docs/architecture.md](docs/architecture.md#stopping-signals-and-a-matchapack-that-goes-away).

## More

- [docs/faq.md](docs/faq.md): questions people ask before they install.
- [docs/architecture.md](docs/architecture.md): how it works, with diagrams, and why it is built this way.
- [TESTING.md](TESTING.md): what "identical" is checked on, the test suites, the CI matrix, the benchmark, and what is not tested.
- [INSTALL-WITH-CLAUDE.md](INSTALL-WITH-CLAUDE.md): install and set up with Claude Code.
- [RELEASING.md](RELEASING.md): how a release is published.
- Questions and bugs: [GitHub Issues](https://github.com/grimmerk/matchapack/issues).

## License

MIT OR Apache-2.0, at your option ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
