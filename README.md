# matchapack

matchapack makes `npm ci`, `npm install` and Yarn 1 fill `node_modules` with copy-on-write clones of one shared copy of each package, so ten git worktrees of a project take about the disk space of one, and each tree is the one plain npm or Yarn would build.

[Install](#install) · [Use](#use) · [Limits](#limits) · [How it works](docs/architecture.md) · [How it is tested](TESTING.md) · [Install with Claude Code](INSTALL-WITH-CLAUDE.md)

<!-- PICTURE: the before/after chart goes here (also used as the social preview). -->

## Why

Every checkout of a JavaScript project needs its own `node_modules`. If you keep several git worktrees of one project (one per branch, or one per AI coding agent), each `npm ci` writes another full copy: 2 GiB for a large frontend project, 20 GiB for ten worktrees, almost all of it the same files.

## What it does

matchapack runs your own npm or Yarn 1 and changes one step inside it. Where npm would unpack a package, or Yarn would copy a file from its cache, matchapack makes a copy-on-write clone (`clonefile` on APFS, the Mac's file system, or a reflink on the Linux file systems Btrfs and XFS) of a copy it already has. A clone takes no new disk space until one side is changed, and it is an independent file: editing a file in one worktree never changes another.

Everything else is npm's or Yarn's own work: which packages, where they go, install scripts, bin links, `package-lock.json`, `yarn.lock`, npm's hidden lockfile and Yarn's `.yarn-integrity`. The tree it builds is the tree plain npm or Yarn builds: the same paths, contents, permissions, symlinks and file times, and the same lockfiles, byte for byte ([how that is checked](TESTING.md#what-identical-means)). Nothing is added to the project: no config file, no `package.json` change.

You keep typing `npm ci`, `npm install` or `yarn`: a line in your shell's startup file routes those commands through matchapack. Every other command goes straight to npm or Yarn.

## Numbers

Ten worktrees of one project, measured with [`scripts/bench.sh`](scripts/bench.sh) on public projects at pinned commits:

| Project | Packages | One `node_modules` | 10 worktrees, plain | 10 worktrees, matchapack |
|---|---|---|---|---|
| small (npm) | 347 | 84 MiB | 835 MiB | 2 MiB + the store, 77 MiB |
| medium (npm, zwave-js-ui) | 1,612 | 635 MiB | 6.20 GiB | 8 MiB + the store, 619 MiB |
| large (npm, Superset's frontend) | 2,709 | 2.07 GiB | 20.68 GiB | 61 MiB + the store, 1.82 GiB |
| Yarn 1 (cornerstone3D) | 2,498 | 1.19 GiB | 11.91 GiB | 10 MiB (Yarn's own cache, 1.88 GiB, is there with plain Yarn too) |

The store is matchapack's one copy of every package, shared by all projects on the machine.

Once the store (for Yarn 1, Yarn's cache) has the packages, installing a fresh worktree is also faster, because cloning a package is quicker than unpacking it:

| Project | Plain | matchapack | Ratio |
|---|---|---|---|
| small | 1.54 s | 1.06 s | 0.69× |
| medium | 6.57 s | 4.71 s | 0.72× |
| large | 17.75 s | 13.02 s | 0.73× |
| Yarn 1 | 14.23 s | 11.64 s | 0.82× |

What it costs (ratios of matchapack's time to plain npm's or Yarn's):

| When | Cost |
|---|---|
| The first install of a package, when the store does not have it yet | 1.04× to 1.27× for a whole install with an empty store (Yarn 1: 1.05× with an empty Yarn cache) |
| The first install after an npm or Node upgrade that changes npm's extraction code | each package in the store is compared once with npm's own extraction: up to 2.8× for a whole `npm ci` (1.72× to 2.77× measured), once per package, not on every install |
| Yarn 1 commands with little to do | a fixed cost per command: 0.89 s instead of 0.49 s on an already installed project, 3.74 s instead of 2.43 s for an incremental install after a lockfile change |

Conditions: an Apple M4 Mac (10 cores, 32 GiB), macOS 27, APFS; Node 24.22.1, npm 11.19.1, Yarn 1.22.22; each case in 10 interleaved rounds, medians; npm's cache warm on both sides (download time is not measured). A second run while the machine was busy gave the same conclusions. The full tables, the method and both runs are in [TESTING.md](TESTING.md#benchmark).

## Install

One of these (macOS on Apple silicon or Intel; Linux on x86_64 or arm64):

```sh
# Homebrew
brew install grimmerk/tap/matchapack
# the install script (puts it in ~/.matchapack/bin and that on PATH)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/grimmerk/matchapack/releases/latest/download/matchapack-installer.sh | sh
# npm
npm i -g matchapack
# crates.io (builds it from source)
cargo install matchapack --locked
```

The install script appends its PATH line to the end of your startup files. If one of them already has matchapack's init line (from a Homebrew, npm or cargo install), move that line below the one the script added: otherwise, once that other install is gone, it runs before `~/.matchapack/bin` is on PATH, and matchapack silently stops routing npm and Yarn.

With nvm or fnm, a global npm package belongs to the Node version that was active: after you switch versions, `matchapack` is not on PATH until you install it there too (Homebrew and the install script do not depend on the Node version; `matchapack init` run by hand says so when this applies).

Then add one line at the end of your shell's startup file. Not sure which shell you use? Run `echo $0` (macOS has used zsh by default since 10.15).

| shell | file | line |
|---|---|---|
| zsh | `~/.zshrc` | `command -v matchapack >/dev/null && eval "$(matchapack init)"` |
| bash | `~/.bashrc` | `command -v matchapack >/dev/null && eval "$(matchapack init)"` |
| fish | `~/.config/fish/config.fish` | `type -q matchapack; and matchapack init \| source` |

It must come after the line that puts matchapack on PATH, so put it at the end of the file: the install script appends its PATH line, `. "$HOME/.config/matchapack/env.sh"` (fish: a file in `~/.config/fish/conf.d`), at the end of the file, and Homebrew's `brew shellenv`, rustup's and a Node version manager's lines must run first too. If it comes earlier, it does nothing. `matchapack init` detects the shell that runs it; `matchapack init zsh` (or `bash`, `fish`) names it explicitly. The function comes from the binary, so no checkout of this repository is needed and it updates with the binary.

The line also defines `mp` as a short name for `matchapack` (`mp store gc`), but only when nothing named `mp` exists on your machine; `matchapack init --alias NAME` picks another name and `--no-alias` defines none. Run `matchapack init` by hand to see whether `mp` would be defined, and why not.

Prefer to let Claude Code do it? Paste the prompt in [INSTALL-WITH-CLAUDE.md](INSTALL-WITH-CLAUDE.md).

## Use

Open a new terminal, then type `npm ci` as usual:

```
$ npm ci
matchapack: npm ci with clones from the store (MATCHAPACK=0 npm ci for plain npm)
…npm's own output: warnings, script output, "added 347 packages in 1s", the audit summary…
matchapack: added 347 packages from the store in 0.8s (inject engine; npm extracted 0)
```

`npm install`, `npm update` and `npm uninstall` go the same way, quietly: they print a line of matchapack's only when a package was placed, and npm writes `package.json` and `package-lock.json` itself, as always. A fresh `npm install` in a new worktree saves as much as `npm ci`.

On macOS, in a Yarn 1 project, `yarn`, `yarn install`, `yarn add`, `upgrade`, `remove` and `upgrade-interactive` go through matchapack too:

```
$ yarn install --frozen-lockfile
matchapack: yarn with clones of Yarn's cache (MATCHAPACK=0 yarn for plain Yarn)
…Yarn's own output…
matchapack: 56914 files cloned from Yarn's cache (425.3 MiB), 128 copied by Node (Yarn's first copies, left to Node by design), in 25.2s
```

Useful to know:

- **Plain npm or Yarn for one command:** `MATCHAPACK=0 npm install`, or `command npm install`.
- **Claude Code** sessions started after you added the line use it too: they take the shell function from their shell snapshot.
- **When matchapack cannot help**, it runs plain npm or Yarn and says why (a filesystem without clones, a global install, an npm version it was not tested with). The result is always the tree you would have had without it.
- **What happened:** `matchapack report` shows recent runs, what was cloned, what was handed to npm and why, and how much space was not written.
- **Only some projects:** list path prefixes, one per line, in `~/.config/matchapack/paths` (`~/git/app` also covers `~/git/app-feature`). Elsewhere npm and Yarn run as usual.
- **Existing `node_modules`** keep their full copies until they are installed again. Run `npm ci` once (for Yarn 1, remove `node_modules` and run `yarn`). [`scripts/convert-existing.sh`](scripts/convert-existing.sh) in this repository does that for every project under a directory, with a backup.
- **Cleaning the store:** `matchapack store gc` removes packages that no installed project uses any more. Installed `node_modules` never depend on the store: clones are independent files.
- **After an npm or Node upgrade**, `matchapack store verify` does the one-time comparison for the whole store ahead of time, instead of during your next installs.

The commands routed, the cases where plain npm runs, and every option are in [docs/architecture.md](docs/architecture.md).

## Uninstall

`matchapack uninstall` shows what it would remove (the store, the run log, the settings directory) and how much space that frees; `matchapack uninstall --yes` removes it. It never edits your startup files: it lists the lines that mention matchapack, for you to remove. Its last line is the command that removes the binary the way you installed it:

| Installed with | Remove the binary with |
|---|---|
| Homebrew | `brew uninstall matchapack` |
| npm | `npm uninstall -g matchapack` (with nvm or fnm, in each Node version you installed it in) |
| cargo | `cargo uninstall matchapack` |
| the install script | `rm ~/.matchapack/bin/matchapack`, then the files and the PATH line uninstall lists |

Installed `node_modules` keep working without matchapack, and a later `npm ci` or `yarn` without the line is plain npm or Yarn.

## Limits

- **macOS (APFS) and Linux (Btrfs, or XFS made with `reflink=1`) only.** On other filesystems, such as ext4, or when the store and the project are on two different disks, npm installs everything itself: the same result as plain npm, without the savings. There is no Windows version.
- **Yarn 1 on macOS only.** On Linux, Yarn 1's own copy already clones on Btrfs and XFS, so matchapack leaves Yarn alone there. Yarn 2 and later, Plug'n'Play installs included, are not supported: those projects run plain Yarn.
- **npm options before the command word are not routed.** `npm --prefix app install` runs plain npm; write `npm install --prefix app` instead. The shell function looks only at the first word.
- **Some npm commands are not routed** and run plain npm: `npm dedupe`, `npm prune`, `npm audit fix`, `npm install-test`, `npm link`, `npx`, and global installs (`npm install -g`).
- **With nvm, fnm or another Node version manager**, a matchapack installed with `npm i -g` belongs to one Node version; after you switch, npm runs plain until you install it there too. Homebrew and the install script do not have this problem.
- **Packages that download files in their install script** (Prisma's engines, for example) do exactly what they do with plain npm or Yarn: those files are downloaded, not cloned.
- **A small `yarn add` clones nothing.** Yarn's first 128 file copies of each command are left to Node by design, so that Yarn's own check of file times sees real copies.
- **`matchapack uninstall` keeps the install script's files** (`~/.config/matchapack/env.sh` and its PATH line), since a startup file still loads them; it prints the commands to remove them. It refuses to run when the run log or the settings directory is inside the store (move them out first).
- **If matchapack is killed with SIGKILL** (or crashes) during an install, npm still undoes its half-done install, but it finishes doing so after matchapack has exited.
- **The umask must be 022** (the usual default); otherwise npm runs plain.

## More

- [docs/architecture.md](docs/architecture.md): how it works, with diagrams, and why it is built this way.
- [TESTING.md](TESTING.md): what "identical" is checked on, the test suites, the continuous integration (CI) matrix, the benchmark, and what is not tested.
- [INSTALL-WITH-CLAUDE.md](INSTALL-WITH-CLAUDE.md): install and set up with Claude Code.
- [RELEASING.md](RELEASING.md): how a release is published.

## License

MIT OR Apache-2.0, at your option ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
