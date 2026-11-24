# Changelog

<!-- dist copies each version's section into the GitHub Release's description, so links here are absolute. -->

## Unreleased

- `matchapack init` run by hand now asks, and on `y` adds the line at the end of your shell's startup file; it shows the file and the line first, and changes nothing else in the file. When a file your shell reads already runs `matchapack init`, it says which file and line instead. When it cannot ask (stdin is not a terminal) or the answer is no, it prints the command that adds the line. Run by hand it no longer prints the shell code (`--print` does). `--yes` adds the line without asking, `--dry-run` only says what it would add ([#20](https://github.com/grimmerk/matchapack/issues/20), [#22](https://github.com/grimmerk/matchapack/pull/22)).

## 0.1.0

The first public release.

- `npm ci`, `npm install`, `npm update` and `npm uninstall` with npm 10, 11 and 12 place each package as a copy-on-write clone of one shared copy in matchapack's store, on macOS (APFS) and on Linux (Btrfs, or XFS with reflink).
- `yarn` and its install commands with Yarn 1.10 or later, on macOS, clone each file from Yarn's own cache.
- The tree is the one plain npm or Yarn builds; [TESTING.md](https://github.com/grimmerk/matchapack/blob/main/TESTING.md) says how that is checked.
- `matchapack init` for zsh, bash and fish; `report`, `store` (`gc`, `adopt`, `verify`) and `uninstall`.
- Install with Homebrew, the install script or cargo. The npm package is not published yet: npm refused the name ([#18](https://github.com/grimmerk/matchapack/issues/18)).
