# matchapack

matchapack saves disk space when you keep several checkouts of a JavaScript project, such as one git worktree per branch or per AI coding agent. It makes `npm ci`, `npm install` and `yarn` (Yarn 1) place each package as a copy-on-write clone of one shared copy: ten worktrees of a project take about the disk space of one, a fresh npm worktree installs in about 70 % of the time, and every `node_modules` is the one plain npm or Yarn would build. (The name: it matches each pack(age) to a copy already on the disk; and matcha. [More](docs/faq.md#why-the-name).)

Works with npm 10, 11 and 12 on macOS (APFS, the Mac's file system) and on Linux with Btrfs or XFS (with reflink, on by default since xfsprogs 5.1), and with Yarn 1.10 or later on macOS.

**Status:** 0.1.2, new. In its author's daily use on macOS since November 2026. Tested: macOS on Apple silicon (daily use and continuous integration, CI) and Linux x86_64 with Btrfs and XFS (CI only; its install time has not been benchmarked there). Built but not tested: Intel Macs and Linux on arm64.

[Install](#install) · [Use](#use) · [Do you need it?](#do-you-need-matchapack) · [Limits](#limits) · [FAQ](docs/faq.md) · [How it works](docs/architecture.md) · [How it is tested](TESTING.md) · [Install with Claude Code](INSTALL-WITH-CLAUDE.md)

<img align="right" width="240" src="docs/assets/matchapack-hero.png" alt="A large three-layer matcha cake stands for the store. Beside it, two small blocks in the same shade are two worktrees of one project, cloned from the store, and a lower block of one dark layer, the store's bottom layer, is a second project that takes another part of the same store.">

① The first install of a package puts one copy of it in the store (`~/.cache/matchapack/store`), which all your projects share.

② Every later install of that package, in any checkout of any project, places a copy-on-write clone of it: a new file that, at first, points to the same blocks on the disk as the store's copy, so it takes almost no new space.

③ Editing a cloned file writes only the changed blocks, for that checkout alone; the store and the other checkouts do not change.

<br clear="right">

## Why

Every checkout of a JavaScript project needs its own `node_modules`, and each `npm ci` writes another full copy: 2 GiB for a large frontend project, 20 GiB for ten worktrees of it, almost all of it the same files.

## What it does

matchapack runs your own npm or Yarn 1 and changes one step inside it. Where npm would unpack a package, or Yarn would copy a file from its cache, matchapack makes a clone (`clonefile` on APFS, or a reflink on the Linux file systems Btrfs and XFS) of a copy it already has. Everything else is npm's or Yarn's own work: which packages, where they go, install scripts, bin links, `package-lock.json`, `yarn.lock`, npm's hidden lockfile (`node_modules/.package-lock.json`) and Yarn's `.yarn-integrity`.

The tree is the one plain npm or Yarn builds: the same paths, contents, permissions and symlinks, file times set by the same rule as npm's or Yarn's own, and the same lockfiles, byte for byte. [TESTING.md](TESTING.md#what-identical-means) says how, and where, each of these is checked, including byte-for-byte comparisons on two public projects that anyone can repeat. Nothing is added to the project: no config file, no `package.json` change, so nobody else on your team has to change anything.

## Do you need matchapack?

Maybe not:

- **Your project can switch to pnpm, Bun or Yarn 2+.** They keep one shared copy too, and clone or hard-link from it ([how they compare](docs/faq.md#how-does-it-compare-with-pnpm-bun-and-yarn-2)). matchapack is for projects that stay on npm or Yarn 1.
- **You keep one checkout of one project.** There is little to share, although a reinstall still takes about 0.8 of plain npm's time. Several different projects do share the store, for the packages they have in common.
- **Yarn 1 on Linux with Btrfs or XFS:** Yarn's own copy already clones there.
- **Your disk is ext4** (most Linux installs): nothing can be cloned. Windows is not supported.
- **CI or Docker builds that start from an empty disk:** there is nothing to share with. A CI runner that keeps its disk between jobs (a self-hosted runner on APFS, Btrfs or XFS) can share.

| | Changes the project | An edit in one checkout changes the others | Tree identical to npm's |
|---|---|---|---|
| plain npm | no | no | yes |
| hard links into a shared copy | no | yes (one file in many places) | no: one file has one set of times and modes |
| pnpm, Bun | yes: another package manager and lockfile, for the whole team | on macOS no (they clone); on Linux yes (they hard-link by default) | no: their own install |
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

Once the store (for Yarn 1, Yarn's cache) has the packages, a fresh worktree also installs faster: cloning a package is quicker than unpacking it, and for Yarn 1, a little quicker than copying it:

| Project | Plain | matchapack | Time saved |
|---|---|---|---|
| small | 1.54 s | 1.06 s | 31 % |
| medium | 6.57 s | 4.71 s | 28 % |
| large | 17.75 s | 13.02 s | 27 % |
| Yarn 1 | 12.82 s | 11.44 s | 11 % |

What it costs (matchapack's time as a multiple of plain npm's or Yarn's: 1.00× is as fast, above it is slower):

| When | Cost |
|---|---|
| The first install of a package, when the store does not have it yet | 1.04× to 1.27× for a whole install with an empty store (Yarn 1: 1.08× with an empty Yarn cache) |
| The first install after an npm or Node upgrade that changed the code that unpacks packages (pacote's or tar's code, or Node's libuv version) | 1.7× to 2.8× for that one `npm ci`: each package it installs is compared once with npm's own extraction ([why](docs/architecture.md#the-store-and-its-key)). This code changed in 23 of the 73 steps from one npm release to the next, from 10.0.0 (August 2023) to 12.2.2 (November 2026), about once every 7 weeks; each Node release carries its own npm, so a Node upgrade can bring one too |
| Yarn 1 commands with little to do | 0.67 s instead of 0.48 s on an already installed project (1.39×), 3.15 s instead of 2.44 s for an incremental install after a lockfile change (1.29×) |

Conditions: an Apple M4 MacBook Air (10 cores, 32 GiB), macOS 27, APFS, with an endpoint security agent installed; Node 24.22.1, npm 11.19.1, Yarn 1.22.22; each case in 10 interleaved rounds, medians; npm's cache warm on both sides (download time is not measured). The npm rows come from one run and a second, busy run that gave the same conclusions; the Yarn 1 rows from a later run with the current build. The full tables, the method and every run are in [TESTING.md](TESTING.md#benchmark).

## Install

You need: macOS (Apple silicon or Intel) with APFS, or Linux (x86_64 or arm64) with Btrfs or XFS (`reflink=1`); Node with npm 10, 11 or 12 (and Yarn 1.10 or later for Yarn projects on macOS); zsh, bash or fish.

**With Claude Code:** start Claude Code in any folder and paste this prompt:

```text
Install matchapack for me. Read all of
https://raw.githubusercontent.com/grimmerk/matchapack/main/INSTALL-WITH-CLAUDE.md
word for word, follow it, and ask me before each step that changes my machine.
```

Claude checks what you have, then does the steps below and asks before each one that changes your machine. [INSTALL-WITH-CLAUDE.md](INSTALL-WITH-CLAUDE.md) is the file it follows; the same prompt there, starting with "Update" or "Uninstall", updates or removes matchapack.

**By hand:**

1. **Install the binary**, with one of these (run only one; [other ways](#other-ways-to-install) are below):

   Homebrew:

   ```sh
   brew install grimmerk/tap/matchapack
   ```

   The install script (it puts the binary in `~/.matchapack/bin` and adds that directory to PATH):

   ```sh
   curl --proto '=https' --tlsv1.2 -LsSf \
     https://github.com/grimmerk/matchapack/releases/latest/download/matchapack-installer.sh \
     | sh
   ```

   npm (the package is `@grimmerk/matchapack`, since npm refused the name `matchapack` ([#18](https://github.com/grimmerk/matchapack/issues/18)); the command is still `matchapack`):

   ```sh
   npm i -g @grimmerk/matchapack
   ```

   With nvm, fnm, asdf or mise, each Node version has its own global packages: after you switch to another Node version, install matchapack again in that version, or install it with Homebrew or the install script instead, which do not depend on the Node version. The [npm package's README](npm/README.md#with-nvm-fnm-mise-or-another-node-version-manager) has the details for each version manager.

2. **Add matchapack's line at the end of your shell's startup file.** It makes `npm` (and, on macOS, `yarn`) go through matchapack, and defines `mp` as a short name for `matchapack` when that name is free (see Notes below). Run `matchapack init` in a terminal and answer `y`: it adds the line to the end of your shell's startup file (it shows the line and the file first). If a file your shell reads already runs `matchapack init`, it says which line and changes nothing. When it will not write the file (bash on macOS when no login profile loads `~/.bashrc`, or under `sudo` in another user's home), it says why and what to do instead. `matchapack init --dry-run` only shows the file and the line.

   Or add it yourself. Run only the command for your shell (`echo $SHELL` tells you which):

   zsh:

   ```sh
   echo 'command -v matchapack >/dev/null && eval "$(matchapack init)"' >> ~/.zshrc
   ```

   bash:

   ```sh
   echo 'command -v matchapack >/dev/null && eval "$(matchapack init)"' >> ~/.bashrc
   ```

   fish (in fish's own configuration directory, even when XDG_CONFIG_HOME moves it):

   ```fish
   mkdir -p $__fish_config_dir
   echo 'type -q matchapack; and matchapack init | source' >> $__fish_config_dir/config.fish
   ```

   The line must stay below every line that puts matchapack or Node on PATH (the install script's `. "$HOME/.config/matchapack/env.sh"`, Homebrew's `brew shellenv`, a Node version manager's); if something added later goes below it, move the line back to the end. bash login shells (Terminal on macOS, an SSH session) read `~/.bash_profile` or `~/.profile`; make sure that file loads `~/.bashrc`, or put the line there. `matchapack init` warns when a line after its own puts matchapack on PATH (the install script's, `brew shellenv`, cargo's env), but it never moves a line: move it yourself.

3. **Open a new terminal and check:** `type npm` says that `npm` is a shell function.

4. **Optional, to use it only in some projects:** list their directories, one per line, in `~/.config/matchapack/paths`. A line also covers directories whose path starts the same way, so `~/git/app` covers worktrees such as `~/git/app-feature`. Outside them, npm and Yarn run as usual.

Notes:

- **`mp`:** the line also defines `mp` as a short name for `matchapack`, but only when nothing named `mp` exists when the line runs (no alias, function, builtin or command on PATH; checked at every shell start). `matchapack init --alias NAME` picks another name, `--no-alias` none.

### Other ways to install

- **From source**, with Rust's `cargo` (Rust 1.88 or later): `cargo install matchapack --locked` from crates.io, or `cargo install --git https://github.com/grimmerk/matchapack matchapack --locked` for the latest `main`. A build from crates.io prints `(unknown)` for its commit in `matchapack --version`.
- **Without piping a script into `sh`:** download the archive for your platform (`matchapack-<target>.tar.xz`) from the [Releases page](https://github.com/grimmerk/matchapack/releases/latest), check it against its `.sha256` file (`shasum -a 256 -c matchapack-<target>.tar.xz.sha256` in the same folder), extract it (`tar -xf matchapack-<target>.tar.xz`), and put the `matchapack` file from the `matchapack-<target>` folder in a directory on your `PATH`. On macOS a browser marks the download as quarantined, and the binary is not notarized, so clear the mark on the binary where you put it: `xattr -d com.apple.quarantine <that directory>/matchapack`.
- **npm, with mise:** `mise use -g npm:@grimmerk/matchapack` installs the npm package in mise's own directory, apart from the global packages of any Node version.

Then go on with step 2 above.

### Updating

`brew upgrade matchapack`; or run the install script again; or `npm i -g @grimmerk/matchapack` again (in each Node version that has it), or `mise upgrade --bump npm:@grimmerk/matchapack` for mise's npm backend; or `cargo install matchapack --locked --force` (for a `--git` install, the same `--git` line with `--force`); or, for an archive from the Releases page, replace the binary with the new release's and, on macOS, clear its quarantine mark again. The line in your startup file needs no change: new terminals get the new version's shell code.

## Use

Once the line is in your startup file, `npm` and `yarn` go through matchapack by themselves, and `mp` is a short name for the `matchapack` command (`mp report`, `mp store gc`). Type `npm ci` as usual:

```
$ npm ci
matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)
…npm's own output (warnings, "added 347 packages in 1s", the audit summary)…
matchapack: added 347 packages from the store in 0.8s (inject engine; npm extracted 0)
```

The inject engine is matchapack's npm engine, which works inside npm's own process ([how](docs/architecture.md#the-inject-engine-npm)). `npm extracted N` counts the packages npm unpacked itself, such as git dependencies. `npm install`, `npm update` and `npm uninstall` go the same way, more quietly: matchapack adds a line only when it installed a package or has something to say (a hand-over to plain npm, a warning). npm writes `package.json` and `package-lock.json` itself, as always. A fresh `npm install` in a new worktree saves as much as `npm ci`.

On macOS, in a Yarn 1 project, `yarn`, `yarn install`, `yarn add`, `upgrade`, `remove` and `upgrade-interactive` go through matchapack too (the last line is one line, wrapped here):

```
$ yarn install --frozen-lockfile
matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)
…Yarn's own output…
matchapack: 56914 files cloned from Yarn's cache (425.3 MiB), 128 copied by Node
  (Yarn's first copies, left to Node by design), in 25.2s
```

**Where it applies.** The line defines shell functions named `npm` and (on macOS) `yarn` in your interactive shells. What does not run through such a shell runs plain npm: a script, a Makefile, an editor's task runner, CI, or a coding agent that starts its own non-interactive shell. Call matchapack by name there: `matchapack npm ci`, `matchapack yarn install`. Claude Code sessions started after you added the line use the functions (they take them from their shell snapshot). An existing `alias npm=…` wins over the function, so matchapack is skipped. For one command, `MATCHAPACK=0 npm install` or `command npm install` runs plain npm.

**Is it working?** `type npm` says it is a shell function. `du` and Finder still show the full size of every `node_modules`, because they count a cloned file in each place it appears. The saving shows in the free space of the disk (`df -h .` before and after a second worktree's install) and in `matchapack report`. The first install of a package fills the store; every later one clones it.

**Try it first.** Without the shell line: install the binary, run `matchapack npm ci` in a spare worktree of an npm project (`mp` exists only once the line is installed), read `matchapack report`, then remove everything with `matchapack uninstall --yes` and the binary. To check the result on your own project, [`scripts/verify.sh`](scripts/verify.sh) `<project> <work>` installs two copies, plain and through matchapack, and compares them.

| Command | What it does |
|---|---|
| `matchapack report [--days N]` | what happened: installs, clones, hand-overs and why, time, space saved |
| `matchapack store` | where the store is and what it holds |
| `matchapack store gc [--dry-run]` | removes packages no installed project uses; installed `node_modules` never depend on the store |
| `matchapack store adopt <dir>` | records projects installed before records existed, or moved, so that gc keeps their packages |
| `matchapack store verify [--fix]` | compares every package in the store whose tarball is still in npm's cache with npm's own extraction (after an npm or Node upgrade, ahead of the next installs) |
| `matchapack npm …`, `matchapack yarn …` | what the shell functions run, for scripts and CI |
| `matchapack uninstall [--yes]` | removes matchapack's own state ([Uninstall](#uninstall)) |

Existing `node_modules` keep their full copies until they are installed again: run `npm ci` once (for Yarn 1, remove `node_modules` and run `yarn`), or [`scripts/convert-existing.sh`](scripts/convert-existing.sh) for every project under a directory, with a backup.

### When something looks wrong

- **No `matchapack:` line:** check `type npm` in that terminal; open a new one after editing the startup file; check that the line comes after the PATH lines.
- **`running plain npm instead (…)`:** the reason says why and what to do; [docs/architecture.md](docs/architecture.md#when-it-runs-plain-npm-instead) lists every one.
- **Nothing saved:** the project and the store must be on the same file system (two volumes of one disk count as two); `du` shows full sizes anyway (above).
- **After an npm or Node upgrade:** the first install can be slower (Numbers, above); `matchapack store verify` does that work ahead of time.
- **Changed how you installed it:** the install script appends its PATH line at the end of your startup files; if matchapack's line from an earlier install is above it, move matchapack's line below it.
- **A broken `npm install`, `update` or `uninstall`:** matchapack stops with an error and prints the commands to repair the project; run them. (`npm ci` is rebuilt with plain npm by itself.)

## Uninstall

`matchapack uninstall` shows what it would remove (the store, the run log, the settings directory) and how much space that frees; `matchapack uninstall --yes` removes it. It never edits your startup files: it lists the lines that mention matchapack, for you to remove. Its last line is the exact command that removes the binary, from where it really is; for example:

| Installed with | Remove the binary with |
|---|---|
| Homebrew | `brew uninstall matchapack` |
| the install script | `rm ~/.matchapack/bin/matchapack`; then remove the PATH line from your startup files, and only after that the files uninstall lists |
| npm | `npm uninstall -g @grimmerk/matchapack`, in each Node version you installed it in |
| an archive from the Releases page | delete the binary from where you put it |
| cargo | `cargo uninstall matchapack` |

Volta, mise and a cargo `--root` install get their own command. Installed `node_modules` keep working without matchapack, and a later `npm ci` or `yarn` without the line is plain npm or Yarn.

## Limits

| Situation | What matchapack does | What you can do |
|---|---|---|
| A file system without clones (ext4), or the store and the project on two file systems (two volumes of one disk count as two) | hands the install to plain npm or Yarn: the same tree, no saving | keep projects on APFS, Btrfs or XFS, and set `MATCHAPACK_STORE` to a directory on the projects' volume |
| npm other than 10, 11 or 12, or a new npm release whose unpacking code (pacote's `lib/fetcher.js`) this build was not tested with | runs plain npm and says so | update matchapack |
| Yarn 2 and later (Plug'n'Play included); Yarn 1 on Linux | runs plain Yarn | nothing: on Linux Yarn 1 already clones |
| Windows | not supported | |
| Scripts, editors, CI, non-interactive shells | not called (no shell function there): plain npm | call it by name: `matchapack npm ci` |
| npm options before the command word (`npm --prefix app install`) | runs plain npm | write them after it: `npm install --prefix app` |
| `npm dedupe`, `prune`, `audit fix`, `install-test`, `install-ci-test`, `link`, `exec` / `npx`, global installs | runs plain npm | |
| Lockfile entries without `resolved` or `integrity`, git and `file:` dependencies, `--install-strategy=linked` | lets npm extract those packages (with `linked`, all of them) itself: no saving for them | |
| A package that downloads files in its install script (Prisma's engines, for example) | nothing: they are downloaded as with plain npm or Yarn | |
| A small `yarn add` (128 files or fewer) | clones nothing: Yarn's first 128 copies are left to Node by design | |
| A future Node release with a known change in how it copies files (not yet released) | runs Yarn 1 installs as plain Yarn: the tree stays right, nothing is cloned | update matchapack once it supports that Node |
| A umask other than 022 | runs plain npm | |
| An endpoint security agent or antivirus (the security software many companies install) | the Yarn 1 engine clones file by file, and such an agent may then scan the clones in the background: on the test Mac, after an install of 80,000 files, the agent kept 2 to 3 cores busy for about 2 minutes, and for about 90 seconds each start of node took 0.3 s longer. The npm engine clones a package directory at once and did not cause this, and a Mac without such an agent showed no slowdown | nothing: it settles by itself; `MATCHAPACK=0 yarn` for plain Yarn |

**If matchapack is killed in the middle of an install.** A plain `kill -9` (SIGKILL) stops npm too: npm gets SIGTERM and rolls back its half-done install by itself, as plain npm would. If matchapack was first asked to stop and then killed (a supervisor's SIGTERM, then SIGKILL), npm is killed before it can roll back, and the project is left half-installed: run `npm ci` again (with workspaces on npm before 10.8.1, first remove each workspace member's `node_modules`, which that `npm ci` keeps); if the project has no lockfile yet, remove `node_modules` first and run `npm install`. Details: [docs/architecture.md](docs/architecture.md#stopping-signals-and-a-matchapack-that-goes-away).

## More

- [docs/faq.md](docs/faq.md): questions people ask before they install.
- [docs/architecture.md](docs/architecture.md): how it works, with diagrams, and why it is built this way.
- [TESTING.md](TESTING.md): what "identical" is checked on, the test suites, the CI matrix, the benchmark, and what is not tested.
- [INSTALL-WITH-CLAUDE.md](INSTALL-WITH-CLAUDE.md): install and set up with Claude Code.
- [RELEASING.md](RELEASING.md): how a release is published.
- Questions and bugs: [GitHub Issues](https://github.com/grimmerk/matchapack/issues).

## License

MIT OR Apache-2.0, at your option ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
