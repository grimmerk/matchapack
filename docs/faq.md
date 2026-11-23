# matchapack FAQ

Questions people ask before they install. How it works is in [architecture.md](architecture.md), how it is tested in [TESTING.md](../TESTING.md), and how to install it in the [README](../README.md#install).

- [Why not pnpm?](#why-not-pnpm) It is another package manager; matchapack keeps npm or Yarn 1.
- [How does it compare with pnpm, Bun and Yarn 2+?](#how-does-it-compare-with-pnpm-bun-and-yarn-2) A table: where the shared copy lives, clone or hard link, and what changes for the team.
- [Why not hard links?](#why-not-hard-links) An edit in one checkout would change all of them, and the tree could not have npm's own file times.
- [Does `du` show the saving?](#does-du-show-the-saving) No: look at the disk's free space or `matchapack report`.
- [What happens on a new npm or Node release?](#what-happens-on-a-new-npm-or-node-release) Mostly nothing; sometimes one slower install, or plain npm until an update.
- [Does it work in scripts, CI and coding agents?](#does-it-work-in-scripts-ci-and-coding-agents) In continuous integration (CI), scripts and agents: only through a shell with the line, or by calling `matchapack npm ci`.
- [Does it work with workspaces?](#does-it-work-with-workspaces) Yes, npm's and Yarn 1's.
- [Does it work on Windows?](#does-it-work-on-windows) No.
- [Does my team have to change anything?](#does-my-team-have-to-change-anything) No: nothing in the project changes.
- [Is anything sent over the network?](#is-anything-sent-over-the-network) Not by matchapack; npm and Yarn download what they would anyway.
- [What if I delete the store, or clean npm's cache?](#what-if-i-delete-the-store-or-clean-npms-cache) Installed projects keep working.
- [Is it safe to try?](#is-it-safe-to-try) It changes only how packages are placed, and every gate falls back to plain npm.
- [Is it written with AI?](#is-it-written-with-ai) Yes, with Claude Code; every change is reviewed and checked against plain npm and Yarn.
- [Why the name?](#why-the-name) "Match a pack", and matcha.

## Why not pnpm?

pnpm keeps one store too, and clones or hard-links from it. If your project can switch to pnpm, that is a good answer. But switching changes the project: another lockfile, another `node_modules` layout (symlinks into a store), and everyone on the team, every CI job and every Dockerfile must use pnpm. matchapack is for projects that stay on npm or Yarn 1: the lockfile, the layout and the tree stay npm's or Yarn's, and only the disk use changes.

## How does it compare with pnpm, Bun and Yarn 2+?

Checked on 2026-11-24 against npm 12.2.3, Yarn 1.22.22, pnpm 12.23.0, Yarn 4.19.0 and Bun 1.4.5, from their documentation and source (links below). "Clone" is a copy-on-write clone; "hard link" is one file in many places.

| Tool | Shared copy | How files reach `node_modules` on macOS (APFS) | On Linux | An edit stays in one checkout | Changes the project | Shared across projects |
|---|---|---|---|---|---|---|
| npm | a cache of downloaded tarballs | full copy: each package is unpacked | full copy | yes | no | downloads only |
| Yarn 1 | a cache of unpacked packages | full copy | full copy; a clone of the cache file on Btrfs and XFS | yes | no | downloads only (Btrfs, XFS: yes) |
| pnpm (default) | a content-addressed store | clone, else hard link, else copy | hard link, else clone, else copy (since pnpm 12; clone first before) | macOS: yes; Linux: no | yes: `pnpm-lock.yaml`, symlinked layout, the whole team on pnpm | yes |
| Yarn 4, `node-modules` linker | a cache of zip archives | full copy (`nmMode: classic`, the default); hard links into a global store with `nmMode: hardlinks-global` | the same | classic: yes; hard links: no | for a project already on Yarn 4, no (a setting); from npm, yes: another package manager and lockfile | classic: downloads only; hard links: yes |
| Yarn 4, Plug'n'Play (its default) | a cache of zip archives | no `node_modules`: packages are read from the cache | the same | (no files to edit in place) | yes: tools must understand Plug'n'Play | yes |
| Bun (default) | a cache of unpacked packages | clone, else copy | hard link, else copy | macOS: yes; Linux: no | yes: `bun.lock`, the whole team on Bun | yes |
| matchapack | its store (for Yarn 1, Yarn's own cache) | clone; when it cannot clone, plain npm or Yarn installs | clone on Btrfs and XFS (npm); other file systems: plain npm | yes | no | yes |

Sources: npm [cache](https://docs.npmjs.com/cli/v12/commands/npm-cache/) and [config](https://docs.npmjs.com/cli/v12/using-npm/config/); Yarn 1 [cache](https://classic.yarnpkg.com/en/docs/cli/cache) and [its copy](https://github.com/yarnpkg/yarn/blob/v1.22.22/src/util/fs-normalized.js#L40-L45) (Node's libuv clones only on Linux: [fs.c](https://github.com/libuv/libuv/blob/v1.53.0/src/unix/fs.c#L1375-L1392)); pnpm [`packageImportMethod`](https://pnpm.io/settings/node-modules#packageimportmethod) and [12.0.0](https://github.com/pnpm/pnpm/releases/tag/v12.0.0); Yarn 4 [`nmMode`, `nodeLinker`](https://yarnpkg.com/configuration/yarnrc) and [linkers](https://yarnpkg.com/features/linkers); Bun [global cache](https://bun.com/docs/pm/global-cache) and [install backends](https://bun.com/docs/pm/cli/install). Each tool has options that change its row (pnpm and Bun can be told to clone or copy on Linux too).

## Why not hard links?

A hard link is one file in many places. An edit in one checkout (a patched package, a build that writes into `node_modules`) would change every checkout and the shared copy, and all places share one set of file times and modes, so a tree could not have the times npm gives it. A copy-on-write clone is an independent file: an edit copies only the changed blocks, into that checkout. The price is a file system that clones: APFS on macOS, Btrfs or XFS on Linux.

## Does `du` show the saving?

No. `du` and Finder count a cloned file in each place it appears, so every `node_modules` still shows its full size. The saving shows in the disk's free space (`df -h .` before and after a second worktree's install) and in `matchapack report`, which shows how much was not written.

## What happens on a new npm or Node release?

Usually nothing visible. Three cases:

- **A new npm major** (13, say): matchapack runs plain npm and says why, until a matchapack release supports it.
- **An npm release whose unpacking code (pacote's `lib/fetcher.js`) changed:** plain npm, until the code is tested and a matchapack release carries its fingerprint. Most npm releases do not change it. A daily job tests every new npm release.
- **A release that changes the tar code npm loads, or Node's libuv version:** the first install after it compares each package once with npm's own extraction, and takes 1.7 to 2.8 times as long as plain `npm ci` (once per package, not per install). `matchapack store verify` does that work ahead of time.

For Yarn 1, a known change in a future Node release would make matchapack run plain Yarn (the tree stays right, nothing is cloned) until it is updated: [architecture.md](architecture.md#the-yarn-1-engine-macos).

## Does it work in scripts, CI and coding agents?

The line in your startup file defines shell functions named `npm` and `yarn` in interactive shells. A script, a Makefile, an editor's task runner, CI, or a coding agent that starts its own non-interactive shell runs plain npm. Call matchapack by name there: `matchapack npm ci`, `matchapack yarn install`. Claude Code sessions started after you added the line do use the functions: they take them from their shell snapshot.

In CI that starts from an empty disk there is usually nothing to share, so matchapack saves little there.

## Does it work with workspaces?

Yes. For npm, `npm ci` inside a workspace member installs into the workspace root's `node_modules`, as npm does, and `-w` / `--workspaces` work as with npm (tested on a workspaces fixture). For Yarn 1, workspaces, `nohoist` and `--focus` are tested against plain Yarn.

## Does it work on Windows?

No. There is no Windows build, and nothing has been built or tested there.

## Does my team have to change anything?

No. matchapack adds nothing to the project: no config file, no `package.json` change, no lockfile change. Each person who wants it installs it for themselves; everyone else keeps running plain npm or Yarn on the same project.

## Is anything sent over the network?

Not by matchapack: the binary has no network code. npm and Yarn download what they would download anyway: a package that is neither in matchapack's store nor in npm's cache is downloaded by npm's own pacote, as with plain npm, and `npm audit` sends its request unless you turn it off. Installing matchapack itself downloads it from GitHub, Homebrew, npm or crates.io.

## What if I delete the store, or clean npm's cache?

Installed `node_modules` keep working: clones are independent files. The next install fills the store again. Removing the store frees only the blocks no project still shares. `matchapack store gc` removes only what no installed project uses, and `matchapack uninstall` removes all of matchapack's state.

## Is it safe to try?

- It changes only how packages are placed in `node_modules`. What npm and Yarn write themselves (`package.json`, the lockfiles) and what install scripts do happen as with plain npm or Yarn. Of its own, matchapack writes only the store, its run log and its settings.
- Every gate it cannot pass falls back to plain npm or Yarn, with nothing changed, and says why.
- `MATCHAPACK=0 npm ci` runs plain npm for one command.
- To try it without the shell line, run `matchapack npm ci` in a spare worktree, then `matchapack report`.
- `matchapack uninstall` is a dry run until you add `--yes`, and installed `node_modules` keep working after it.

## Is it written with AI?

Yes. matchapack is written by one maintainer working with Claude Code. Every pull request is reviewed by two review tools, larger changes also by a separate step-back review, and each finding is checked against the code; every change is tested against plain npm and Yarn, as [TESTING.md](../TESTING.md#independent-reviews) describes.

## Why the name?

It reads two ways. "Match a pack": when an install needs a package (a "pack"), matchapack matches it to a copy already on the disk (in its store, or for Yarn 1 in Yarn's cache) and, when there is one, places a clone of that copy. And matcha: the picture near the top of the README is a matcha layer cake standing for the store, with smaller blocks beside it for the checkouts that take their packages from it.
