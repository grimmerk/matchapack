# How matchapack is tested

matchapack's promise is that the `node_modules` it builds is the one plain npm or plain Yarn 1 would build, only made of clones. This document says what "the same" is checked on, which tests check it, where they run, what is checked on real projects, and what is not tested. How matchapack works is in [docs/architecture.md](docs/architecture.md); how to use it is in the [README](README.md).

## Contents

- [What "identical" means](#what-identical-means)
- [The test levels](#the-test-levels)
  - [Unit tests](#unit-tests)
  - [End-to-end suites](#end-to-end-suites)
  - [The fixture harness (npm)](#the-fixture-harness-npm)
  - [The step harness: incremental installs](#the-step-harness-incremental-installs)
  - [The Yarn 1 harness](#the-yarn-1-harness)
  - [The local tarball server](#the-local-tarball-server)
- [Continuous integration](#continuous-integration)
  - [The npm canary and the Node and Yarn release watch](#the-npm-canary-and-the-node-and-yarn-release-watch)
  - [Linux](#linux)
- [Combination sweeps](#combination-sweeps)
- [Mutation checks](#mutation-checks)
- [Independent reviews](#independent-reviews)
- [Real projects](#real-projects)
  - [Sampled verification of real runs](#sampled-verification-of-real-runs)
- [Benchmark](#benchmark)
- [What is not tested](#what-is-not-tested)
- [Running the tests yourself](#running-the-tests-yourself)

## What "identical" means

Every comparison installs the same project twice, on copies, with plain npm (or Yarn) and through matchapack, from the same start, and compares the two trees. `matchapack diff A B` is the comparison (exit 0 identical, 1 different, 2 an error). It checks:

| What | How |
|---|---|
| paths | every path on both sides, and its kind (file, directory, symlink) |
| file contents | byte for byte |
| modes | permission bits of files and directories |
| symlinks | their targets (`.bin` links, workspace links) |
| times | each path's modification time, to the nanosecond (`--meta`) |
| owner and group | uid and gid of each path (`--meta`) |
| extended attributes | their names (`--meta`); not their values, because macOS gives every file a `com.apple.provenance` value that a clone keeps from its source and a copy does not |
| npm's files | `package.json`, `package-lock.json` and the hidden lockfile (`node_modules/.package-lock.json`), byte for byte; whether npm still trusts the hidden lockfile; `npm ls` |
| Yarn's files | `.yarn-integrity`, byte for byte (`--meta` reports it on a line of its own) |

Two rules keep the comparison honest:

- **Times written during an install differ between any two runs.** `--a-run START:END` / `--b-run` (nanoseconds since the epoch) and `--a-cache` / `--b-cache` let a time through only when both sides wrote it the same way: both within their own run, or both equal to the same cache file's time. Anything else is a difference.
- **A tree's own path can appear inside a file name.** node-gyp writes `build/Release/.deps/<absolute path>/…`. `--normalize-roots` writes each tree's own root as `<root>` where it appears inside a relative path, so the same build gives the same path in both trees. Every harness compares with it.

Where plain npm or Yarn itself varies between two runs (install scripts that write timestamps or absolute paths, native builds), the harness runs plain twice and reports "plain varies", with the paths: matchapack must then differ only where plain varies.

## The test levels

### Unit tests

`cargo test` runs about 150 Rust unit tests: the clone server's validation of sources and destinations (with symlinked parents inside and outside the project), both placement paths, concurrent requests over a socket, the store's layout, fill, comparison and decline list, the project records and `store gc`, `uninstall`'s refusals, the routed words (the table in `src/wrapper.rs` against both shell files), the fingerprint list, the run log and the report. On a filesystem that cannot clone (the ext4 disk of the Linux job in continuous integration, CI), the placement tests skip themselves and the validation tests still run.

### End-to-end suites

Each takes a work directory, builds its own store, settings and log there, and never touches the user's.

| Script | What it checks |
|---|---|
| `scripts/test-inject.sh` | the inject engine: the hook cloned, a disabled hook and a store miss give the same tree, every gate, npm's own failures passed on once, the store lock, the project record, the file group with a store under another group, npm's flags on the workspaces fixture (`--omit=dev`, `--workspace`, `--workspaces`, `--install-strategy`, `NODE_ENV=production`, `.npmrc` settings), each against plain `npm ci` |
| `scripts/test-inject-hook.js` | the hook's eligibility rules against npm's real fetcher classes, the fingerprint and the store key, the repair advice ([Combination sweeps](#combination-sweeps)) |
| `scripts/test-wrapper.sh` | routing through the shell function: aliases, bypasses, the trial scope, hand-overs that leave `node_modules` untouched, npm's failures passed on once, the run log, the `mp` short name |
| `scripts/test-safety.sh` | tampered lockfiles and symlinked directories, each against plain `npm ci` on a copy: the same exit status, and nothing written outside the project beyond what npm itself writes |
| `scripts/test-store.sh` | project records, `store gc`, `store adopt`, `store verify` through npm's pacote, the store lock and the clean-up of temporary directories |
| `scripts/test-shells.sh` | the same routing in bash, zsh and fish, with a stub `matchapack` in Docker; with `--real`, fish with the real binary, real npm and Yarn 1 |
| `scripts/test-yarn1.sh` | the Yarn 1 harness and engine on the `fixtures/yarn1-*` projects ([The Yarn 1 harness](#the-yarn-1-harness)) |
| `scripts/test-npm-launcher.sh` | the npm package's `sh` launcher, with stub binaries |
| `scripts/test-npm-canary.js` | the canary's decisions, offline |
| `scripts/test-bench.py` | the benchmark script's own logic, offline: the repair of a torn results line, the work directory's marker, the store markers, resume safety, the isolation check and its report, the end of a run however it stops (a signal during an install, a setup error), the exit status, the clean-up of a timed-out process group, the machine-wide lock |
| `scripts/handover-test.sh` | where nothing can be cloned (ext4; the store and the project on two filesystems): `matchapack npm ci` against plain `npm ci` |

### The fixture harness (npm)

`scripts/verify.sh <project> <work>` installs copies of a project with plain `npm ci` and through matchapack and compares them. `MATCHAPACK_VERIFY_BYTES=1` also requires the hidden lockfile to be byte-identical; `MATCHAPACK_VERIFY_META=1` adds times outside the two runs, owner, group and extended attribute names. It runs on any npm version and ignores the trial scope, since it is how a new npm version gets verified.

`scripts/verify-npm.sh <work> [project]` runs it on every fixture in `fixtures/` (and optionally on a real project, which is copied, never changed), then the step harness cases below. The fixtures cover, among others: a plain project (`small`), workspaces, optional dependencies skipped by platform and by `engines`, an optional package whose install script fails, `--legacy-peer-deps`, bundled dependencies, root lifecycle scripts, odd tarballs (a file listed twice; `.gitignore` before `.npmignore`), the environment install scripts see, and npm 12's patches. Every case must have cloned something, so a hook that stopped firing cannot pass. It also says whether the running npm's pacote fingerprint is on the list.

### The step harness: incremental installs

`scripts/verify-steps.sh <project> <work> [--start bare|ci|nolock] -- <step> [-- <step> …]` is the harness for `npm install`, `npm update` and `npm uninstall`. It runs the same sequence of npm commands with plain npm and through matchapack, from the same start, and compares after every step: the exit status, the whole tree with the hidden lockfile byte for byte, `package.json` and `package-lock.json` byte for byte, whether npm trusts the hidden lockfile, `npm ls`, stale times, and matchapack's log record (the command, the root, the count of packages the hook never saw, the `store gc` record). `CLONES=…` and `UNSEEN=…` in a step are expectations. `verify-npm.sh` runs about 50 such cases: `npm install`, `npm install <pkg>`, `update`, `uninstall`, workspaces, `--prefix`, and flags such as `--force`, `--legacy-peer-deps`, `--save-peer`, `--save-optional`. They use the public registry with pinned versions and npm's `before` setting.

### The Yarn 1 harness

`scripts/verify-yarn1.sh [--cache warm|empty] <project> [yarn args]` installs a project with plain Yarn 1 twice, on copies, with a scratch cache, and compares the two trees with `diff --meta`: exit 0 identical, 3 "plain varies" (with the paths). `--engine yarn1` adds a third copy installed by `matchapack yarn`, which must be identical to the first plain tree, or differ only where plain varies. `--command 'WORDS'` runs a Yarn command (`add`, `upgrade`, `remove`, `workspace … add`) after the install; `--command-fresh` runs it on a copy with no `node_modules`.

`scripts/test-yarn1.sh` runs it on the `fixtures/yarn1-*` projects: a flat project, workspaces, `nohoist`, `--focus`, `file:` dependencies, a package with scripts, upgrades, a project with more than 128 package files (past the first copies left to Node), and an empty-first project whose first copies are empty files. It also checks the gates (Yarn 2+ projects, `packageManager`, `yarn-path`, a symlinked `node_modules`), a cache filled by another Node major (Node 22, then installed with Node 24, and the other way round), a cache on another volume, and the route in fish.

**Plain Yarn's own race.** With Node 22.12 or later, plain Yarn 1 itself sometimes leaves a different modification time on one file between two runs. Node's copy sets a destination's times from the source before it writes the data; Yarn decides once per process, from whichever of its first 128 copies finishes last, whether copies keep times; a thread-pool pause at the wrong moment changes that last writer. Measured: in 300 plain runs with the default 4 pool threads, 8 ended differently; with 1 pool thread, none did (300 plain and 300 matchapack runs). The tests and the sampled verification therefore rerun a difference of this shape with one pool thread, where the race cannot happen, before they decide.

### The local tarball server

The fixtures name their tarballs (`fixtures/optional-upward/tarballs`) by the placeholder host `127.0.0.1:48321`. Each suite that installs them starts its own server on a free port and installs from a copy of `fixtures/` whose URLs (and the hashes of the tarballs that name the host) are rewritten to that port (`scripts/fixture-server.sh`). Suites from several checkouts therefore run at the same time, and none uses a server it did not start: a server that does not answer the copy's token stops the suite (status 2). To install a fixture by hand, serve the tarballs on the placeholder: `python3 -m http.server 48321 --bind 127.0.0.1 --directory fixtures/optional-upward/tarballs`.

## Continuous integration

`.github/workflows/ci.yml` runs on every pull request:

| Job | Runner | What it runs |
|---|---|---|
| `linux` | Ubuntu | `cargo test` and a release build with warnings as errors; `test-shells.sh` in Docker (bash, zsh, fish with stubs); the canary's decisions; the benchmark script's own logic; the npm launcher; fish with the real binary, real npm and Yarn 1; the hand-over where nothing can be cloned (ext4, and `EXDEV` with an empty and a filled store) |
| `linux-npm` | Ubuntu, one job per npm line: 10.9.10, 11.21.2, 12.2.2 | `test-inject.sh` and `verify-npm.sh` with the work directories on a loop-mounted Btrfs, so the hook really clones |
| `macos` | macOS 15, Node 24.22.1 (npm 11.19.1), Node 22.14.0 as a second Node | the safety, routing, store and inject suites; the fixture harness with byte-identical hidden lockfiles; the Yarn 1 harness and engine with Yarn 1.22.22, including the cross-Node and two-volume cases and the route in fish |

The npm versions are pinned, never "the newest", so a new npm release cannot turn an unrelated pull request red. Moving a pin is the canary's job.

### The npm canary and the Node and Yarn release watch

`.github/workflows/npm-canary.yml` runs daily and asks for a person only when something changed.

- **npm.** It reads npm's version list from the registry and tests each new release of the supported majors (10, 11, 12) once, soon after it is published, plus the newest release of a newer major, and once a month the current release of every line. Each runs `test-inject.sh` and `verify-npm.sh` on Linux (Btrfs) with the version and fingerprint gates set to accept it. It records the release's pacote fingerprint and tar code, and diffs the npm functions the hook depends on (pacote's `FetcherBase` class, arborist's caller chain, `ci.js`) against the last verified release. macOS runs too when the fingerprint is new or a major is new.
- **Node and Yarn 1.** The Yarn engine depends on Node's `fs.copyFile`, which is libuv's, and libuv changes inside a Node line (Node 22.11.0 to 22.12.0 changed what a copy keeps). The watch reads Node's release list, the libuv version of each watched line's newest release, and a hash of libuv's copy code (`deps/uv/src/unix/fs.c`) at that release, because Node also picks libuv fixes without changing libuv's version. A new Node major, or a libuv change in a watched line, runs the npm suites on Linux and the Yarn suites on macOS (`scripts/yarn-suites.sh`) on that Node. A new stable Yarn 1 release runs the Yarn suites with it.
- **Results** go to a tracking issue: a table per line, a history, and a comment when a person should look (a new fingerprint, a changed function, a new major, a failed suite). The run is red whenever it commented, so GitHub's failed-run email is the notice.

The run-time checks ([docs/architecture.md](docs/architecture.md#run-time-checks)) are the other half: an npm release whose pacote code is not on the list runs as plain npm until the canary has tested it and the fingerprint is added.

### Linux

- **Btrfs and XFS in CI.** `linux-npm` runs the npm suites on Btrfs on every pull request. `.github/workflows/linux-large.yml` (run by hand, and on pull requests that change it) installs a large public project, zwave-js/zwave-js-ui at a pinned commit (1,686 lockfile entries, 1,611 installed; native addon, esbuild and other install scripts), on Btrfs and on XFS (made with `reflink=1`), each on a loop image: plain `npm ci` against matchapack with an empty store, then fresh copies with the warm store, with byte-identical hidden lockfiles and `diff --meta`.
- **Where nothing can be cloned.** `scripts/handover-test.sh` (in the `linux` job) runs `matchapack npm ci` against plain `npm ci` with the store and projects on ext4, and with the store on Btrfs and the projects on ext4 (`EXDEV`), empty and filled; and a lockfile out of sync, where npm's own status must come back and `node_modules` stay untouched.
- **Docker.** `scripts/linux-test.sh` runs the harness on Btrfs and XFS in Docker, with the negative cases (overlayfs, two filesystems).

## Combination sweeps

Some rules are checked against every combination a tool defines, not a few examples.

- **The repair advice against npm's own parser.** When a run of `npm install` / `update` / `uninstall` breaks, matchapack prints the commands to repair it, with your options written out ([docs/architecture.md](docs/architecture.md#the-repair-advice)). A property test in `scripts/test-inject-hook.js` takes every option npm defines, plain and negated, followed by several kinds of word, builds the command line `install --<option> <word> --no-audit pkg`, lets npm's own config parser read it, builds the advice's options, and parses them again with npm's parser. The two readings must be equal, or the advice must say it is unsure. Measured: 2,790 command lines on npm 10.5.2, 3,258 on 11.19.1, 3,438 on 12.2.2; no wrong option list, and "unsure" only where npm's own reading of the line is odd. An everyday set of 27 command lines is never unsure.
- **The routed words** run through bash, zsh and fish (`scripts/test-shells.sh` reads the table from the source).

## Mutation checks

A test that cannot fail reads as coverage and proves nothing. Every guard (a gate, a check of a request, a comparison rule) gets a mutation check when it is written: the guard is broken on purpose in a scratch copy, the named test must go red, and the copy is thrown away. Each test is first run unmutated, to show that its filter matches. Examples of the scale: the inject engine's first pull request had 21 mutations and a control (20 caught, 1 equivalent, the control survived); the sampled verification's Yarn part was checked with a mutation that clones Yarn's first copies, over 40 samples. When a mutation survives, either a test is added or the guard is shown to be redundant, and the result is written down.

## Independent reviews

Every pull request is reviewed by tools that did not write it, run locally on the branch: the Codex CLI and the CodeRabbit CLI. Larger changes also get a step-back review of the whole design, not only of the diff. Every finding is checked against the code; the conclusion for each, fixed or not and why, goes into the pull request.

## Real projects

The fixtures are small on purpose. The harness (`scripts/verify.sh`, or `scripts/verify-npm.sh` with a project) has also been run on real projects throughout development, on copies, never changing the project. Some of these runs were made with the earlier engine that the inject engine replaced; the current engine's everyday runs are checked by the sampled verification below.

- public open-source projects for npm: compass (4,701 lockfile entries, 76 workspace links, native builds), eventsource, vscode-git-graph and rollup-starter-app (version 2 lockfiles, sha1-only integrities), and the Linux job's zwave-js-ui ([Linux](#linux));
- private repositories of the maintainer's: 14 in the harness's list of subjects, the largest an npm workspaces monorepo with about 3,600 lockfile entries in 6 workspaces (plain `npm ci` itself fails on 4 of them, so those cannot be compared). They are not named here;
- for Yarn 1, three real projects (a single package, a 9-workspace project, and a private service repository), with install scripts and without: identical to plain Yarn down to file times in nanoseconds, owner, group, extended attribute names and the bytes of `.yarn-integrity`. One install fails in plain Yarn too, in a native build; up to that point the trees differed only where two plain runs also differ. 99.7 % or more of the bytes Yarn takes from its cache shared blocks with it.

matchapack has been in the maintainer's daily use since November 2026, on npm and Yarn 1 projects; `matchapack report` shows what happened in those runs.

### Sampled verification of real runs

Clean statistics in `matchapack report` show that matchapack ran and cloned; they do not show that the tree equals plain npm's. `scripts/verify-sample.sh [N]` (npm) and `scripts/verify-sample.sh --yarn [N]` (Yarn 1) check that on real runs, after the fact. For each of the N (default 2) most recent projects in the run log, one logged run is replayed on copies, plain and through matchapack, and the trees are compared. One line per project goes to `~/.cache/matchapack/verify.jsonl`, and `matchapack report` shows the samples.

- **What is replayed.** Full installs (`npm ci`; `yarn` / `yarn install`), from the files as the run left them. And commands that started with no `node_modules`, as in a fresh clone (`npm install` / `update` / `uninstall`, `yarn add` / `upgrade` / `remove`), from the files before the run: the run log keeps a SHA-256 of the lockfile and of each manifest before and after every run, and the files before come from the project while they still have those hashes, else from its git HEAD.
- **When it says IDENTICAL.** Only when the replay's inputs equal what the logged run recorded (the files it hashed, the project's rc files, the words it logged, and the environment's npm or Yarn settings, compared by hash), matchapack's run in the replay cloned as many files or packages as the logged run, and every check of the comparison passed. When the sampler cannot be sure the replay repeated the run, it does not say IDENTICAL.
- **The other results.** DIFFERENT (the trees differ, a check failed, or matchapack failed where plain installed); ERROR (the replay could not test anything or did not repeat the run); SKIPPED, with the reason (the project or its files changed since, a local `file:` dependency, a secret in the arguments, a record from an older build); and for Yarn RACE-SHAPED: a difference with the shape of plain Yarn's own race, which is run again with one pool thread. Still there, it is DIFFERENT; gone, it stays RACE-SHAPED: neither a pass nor a fail, sample that project again.
- **Limits.** It replays commands run on a tree that already had `node_modules` only as full installs; incremental commands on an existing tree are not sampled, since their result depends on that tree. npm samples compare paths, contents, modes and `npm ls`, not times, owners or extended attributes (Yarn samples do). The user's and the global rc files (`~/.npmrc`, `~/.yarnrc`) are neither logged nor hashed. Each sample installs the project three or four times (a Yarn sample that needs the one-thread run, seven or eight).

**Latest samples (2026-11-22, a build of the main branch of that day):**

- **npm:** two samples of `npm ci` on a large private monorepo (two worktrees). Plain npm took 95–99 s, matchapack 65–72 s. `package.json` and `package-lock.json` were byte-identical, `npm ls` was the same as plain, and two plain runs differed in 0 paths. The verdict was BUILD-OUTPUT-ONLY (the trees differ only outside `node_modules`): 1–2 files differed, all in the build output of one workspace package of the project (`dist/*.d.mts` and `*.d.cts`, written by the project's own build at install time; the same size, other content). No installed package differed. The cause is not established. A plausible one is that the build tool's output depends on the order in which it lists a directory, which can differ between a cloned directory and one npm extracted; either way it touches only files the project's own build writes.
- **Yarn 1:** no new replay this time: every logged run was skipped (its manifests had changed since, or its directory was gone). The latest real-project evidence for Yarn is a comparison of 2026-11-20 on a copy of grimmerk/codev, an Electron app with native modules: `scripts/verify-yarn1.sh --engine yarn1` (plain Yarn twice, matchapack once), 64,552 paths compared, identical except `node_modules/.prisma/client/index.js`, which embeds each tree's absolute path and differs between the two plain runs too. matchapack cloned 56,914 files, in 15.4 s against about 18.7 s for plain Yarn.

## Benchmark

`scripts/bench.sh` measures disk and time against plain npm and Yarn 1 on public projects pinned to a commit. Everything it uses is under its work directory (a new or empty directory, which it marks as its own): its own store, settings, logs and trial scope, npm's and Yarn's caches, and empty npmrc and yarnrc files, so the user's are neither read nor used; the real caches and matchapack state are compared entry by entry, a few levels deep, before and after. One benchmark runs per machine, and a benchmark resumes after an interruption only with the same tools and settings.

**Method.** Each case runs in 10 interleaved rounds (plain first in odd rounds, matchapack first in even ones), with 10 s of settle time before each run; times are medians with the minimum and maximum. Wall time and CPU time (user plus system) of the install command and every process it waited for. npm's cache is warm for both sides in every case (primed by a plain install first, the only install that may download; network is not measured). Yarn 1 runs use `--ignore-scripts` (one project's install scripts fail in plain Yarn too), and Yarn's tarballs come from an offline mirror primed first, so an empty Yarn cache means extracting, not downloading. Disk is a first-extent estimate: a file in `node_modules` counts as shared when its first extent is the first extent of a file in the store or Yarn's cache (`scripts/shared-blocks.py`), and "new bytes" are the space allocated by the files that share no first extent (a shared file whose later blocks were rewritten would not be counted there); metadata (inodes, directories) is not counted.

The two runs below were made with an earlier version of the script, in which the timed npm runs used `--prefer-offline` and the Yarn runs had neither `--offline` nor `--no-default-rc`. Checked afterwards: every package came from the warm cache (npm's debug logs show no download), the benchmark machine's `~/.yarnrc` sets nothing that changes an install (no `yarn-path`, install, cache or mirror setting), and neither run was interrupted or resumed. The script now runs every install after the priming one with `--offline`, so a cache or mirror miss fails the run instead of timing a download, and gives every Yarn command `--no-default-rc`.

**Machine.** An Apple M4 Mac (10 cores, 32 GiB), macOS 27.0.1, APFS; Node 24.22.1, npm 11.19.1, Yarn 1.22.22; matchapack 0.1.0.

**Projects.**

| Project | Repository @ commit | Packages installed | `node_modules` |
|---|---|---|---|
| small (npm) | this repository's `fixtures/small` | 347 | 6,036 files, 84 MiB |
| medium (npm) | zwave-js/zwave-js-ui @ `7af2ab653895` | 1,612 | 45,843 files, 632 MiB |
| large (npm) | apache/superset @ `90cf89255c21`, `superset-frontend` | 2,709 | 172,399 files, 2.09 GiB |
| yarn (Yarn 1) | cornerstonejs/cornerstone3D @ `d0e0e37510e2` | 2,498 | 81,847 files, 1.19 GiB |

**Time, the quiet run** (wall time, medians, with min–max; the ratio is matchapack's median over plain's):

| Project | Case | Plain | matchapack | Ratio | Plain CPU | matchapack CPU |
|---|---|---|---|---|---|---|
| small | empty store (npm's cache warm) | 1.60 s (1.11–2.39) | 1.66 s (1.36–1.87) | 1.04× | 2.07 s | 3.30 s |
| small | first use after an npm change (store filled by npm 11.10.1) | 1.57 s (1.51–1.75) | 2.70 s (2.56–3.42) | 1.72× | 2.00 s | 5.72 s |
| small | a fresh worktree, warm store | 1.54 s (1.44–2.00) | 1.06 s (0.83–2.25) | 0.69× | 1.99 s | 1.49 s |
| small | reinstall over an existing `node_modules` | 1.62 s (1.24–1.87) | 1.36 s (1.02–1.63) | 0.84× | 2.81 s | 2.31 s |
| medium | empty store | 6.54 s (6.33–7.34) | 8.30 s (8.13–9.40) | 1.27× | 13.32 s | 20.41 s |
| medium | first use after an npm change | 6.61 s (6.46–7.11) | 13.86 s (13.63–14.60) | 2.09× | 13.10 s | 42.06 s |
| medium | a fresh worktree, warm store | 6.57 s (6.31–7.45) | 4.71 s (4.46–5.81) | 0.72× | 13.11 s | 8.12 s |
| medium | reinstall | 8.79 s (8.62–10.49) | 6.64 s (5.64–7.92) | 0.76× | 19.15 s | 14.38 s |
| large | empty store | 17.40 s (17.06–18.95) | 21.77 s (21.33–23.16) | 1.25× | 46.92 s | 62.85 s |
| large | first use after an npm change | 17.64 s (17.21–19.59) | 48.82 s (47.00–50.15) | 2.77× | 47.21 s | 173.85 s |
| large | a fresh worktree, warm store | 17.75 s (17.16–18.42) | 13.02 s (12.21–14.53) | 0.73× | 47.43 s | 34.99 s |
| large | reinstall | 27.94 s (27.42–29.57) | 21.28 s (20.29–21.55) | 0.76× | 73.12 s | 58.30 s |
| yarn | empty Yarn cache (tarballs in the offline mirror) | 19.52 s (18.90–20.34) | 20.58 s (19.71–22.52) | 1.05× | 59.08 s | 74.64 s |
| yarn | a fresh worktree, warm cache | 14.23 s (12.03–16.34) | 11.64 s (11.22–12.50) | 0.82× | 38.48 s | 47.79 s |
| yarn | already installed (nothing to do) | 0.49 s (0.30–0.75) | 0.89 s (0.87–1.03) | 1.80× | 0.46 s | 0.56 s |
| yarn | incremental install after a lockfile change | 2.43 s (2.36–2.49) | 3.74 s (3.55–3.89) | 1.54× | 5.16 s | 7.30 s |

**Disk per extra worktree** (fresh worktree, warm store or cache; new bytes as defined above, a first-extent estimate):

| Project | Files sharing a first extent | New bytes per worktree: plain | matchapack | Store or cache, once per machine |
|---|---|---|---|---|
| small | 6,020 of 6,021 (99.8 % of bytes) | 84 MiB | 164 KiB | store 77 MiB |
| medium | 45,784 of 45,786 (99.9 %) | 635 MiB | 772 KiB | store 619 MiB |
| large | 171,992 of 172,313 (99.7 %) | 2.07 GiB | 6 MiB | store 1.82 GiB |
| yarn | 81,602 of 81,731 (99.9 %) | 1.19 GiB | 1 MiB | Yarn's cache 1.88 GiB, which plain Yarn keeps too |

**Two runs: the results hold under load.** The same benchmark ran twice on the same machine on the same day: once while the machine did other work, and once with it otherwise idle. The table shows the ratio of the medians in both, and how many of each case's 20 runs started with a 1-minute load average above 3.0 (such runs are counted, not dropped).

| Project | Case | Ratio, quiet run | Ratio, busy run | Runs above load 3.0, quiet / busy |
|---|---|---|---|---|
| small | empty store | 1.04× | 1.07× | 2 / 9 |
| small | first use after an npm change | 1.72× | 1.65× | 0 / 13 |
| small | a fresh worktree | 0.69× | 0.78× | 0 / 13 |
| small | reinstall | 0.84× | 0.88× | 0 / 6 |
| medium | empty store | 1.27× | 1.24× | 3 / 2 |
| medium | first use after an npm change | 2.09× | 2.28× | 7 / 17 |
| medium | a fresh worktree | 0.72× | 0.73× | 3 / 8 |
| medium | reinstall | 0.76× | 0.70× | 3 / 2 |
| large | empty store | 1.25× | 1.24× | 1 / 16 |
| large | first use after an npm change | 2.77× | 2.40× | 14 / 17 |
| large | a fresh worktree | 0.73× | 0.72× | 6 / 17 |
| large | reinstall | 0.76× | 0.72× | 1 / 15 |
| yarn | empty cache | 1.05× | 1.07× | 6 / 9 |
| yarn | a fresh worktree | 0.82× | 0.75× | 8 / 16 |
| yarn | already installed | 1.80× | 1.58× | 9 / 19 |
| yarn | incremental install | 1.54× | 1.44× | 11 / 18 |

In total, 74 of the quiet run's 320 runs started above load 3.0, against 197 of the busy run's. Every ratio stays on the same side of 1.0 in both runs, the main case (a fresh worktree) is within 0.09 between them, and the disk numbers are the same. So the conclusions do not depend on an idle machine; the exact ratios move by up to about 0.4 in the cases with the most work per entry (first use after an npm change).

To run it yourself (macOS, APFS; the quiet run took 5.5 hours with every project and 10 rounds, unattended):

```sh
cargo build --release
scripts/bench.sh --work ~/matchapack-bench.noindex --rounds 10
```

The results are in `<work>/results/summary.md`. A work directory whose name ends in `.noindex` is not indexed by Spotlight. `python3 scripts/bench.py --help` lists the options (projects, cases, rounds, load limits).

## What is not tested

- **corepack** only at 0.36.0 (Node 24's own copy, and the package `npm i -g corepack` installs). corepack on Node 25 or later, which no longer bundles it, has not been run.
- **Version managers** other than nvm, fnm, Homebrew's `node`, the official installer, volta, mise and asdf (all on macOS): nodenv, nvs, proto, pkgx, Nix wrappers, Homebrew's `node` behind a shim, and version managers on Linux have not been run. Unknown launchers run plain npm or Yarn by design.
- **An old store entry against a new libuv's code.** The release watch runs with a fresh store, and the store key names libuv's version only, not its code; the one-in-50 sample and `store verify` are the safety net there.
- **`scripts/convert-existing.sh`'s npm "nothing cloned" path** is tested only with a stand-in `matchapack`.
- **Windows** is not supported, and nothing runs there.
- **Linux** has been run on CI runners and in Docker only, not on a desktop machine. Yarn on Linux is plain Yarn by design.
- **Commands not routed** (`npm dedupe`, `prune`, `audit fix`, `install-test`, `install-ci-test`) are not tested through matchapack, because they run plain npm.

## Running the tests yourself

The suites need macOS with APFS (or Linux with Btrfs or XFS for the npm suites), Node, and for the Yarn suites Yarn 1. They install from the local tarball server and, for the step harness, from the public registry. Give each suite a scratch work directory, and a scratch npm cache, so that your own cache is not touched:

```sh
cargo build --release
cargo test --release
export npm_config_cache=/tmp/mp-npm-cache
scripts/test-inject.sh /tmp/mp-inject
scripts/test-wrapper.sh /tmp/mp-wrapper
scripts/test-safety.sh /tmp/mp-safety
scripts/test-store.sh /tmp/mp-store
scripts/verify-npm.sh /tmp/mp-verify-npm              # the fixtures and the step harness
scripts/verify-npm.sh /tmp/mp-verify-npm ~/code/app   # plus a real project (copied, never changed)
scripts/test-yarn1.sh /tmp/mp-yarn1                   # needs Yarn 1 (MATCHAPACK_YARN_JS can name its yarn.js)
```

After an npm upgrade, `scripts/verify-npm.sh` is the routine: it says whether the new npm's pacote fingerprint is on the list. A new npm major goes into `INJECT_NPM_MAJORS` in `src/wrapper.rs`, and a new fingerprint into `src/npm-fingerprints.json`, only after `scripts/test-inject.sh` and `scripts/verify-npm.sh` pass with it; the list is compiled into the binary, so rebuild matchapack afterwards.
