# Changelog

<!-- dist copies each version's section into the GitHub Release's description, so links here are absolute. -->

## 0.1.0

The first public release.

- `npm ci`, `npm install`, `npm update` and `npm uninstall` with npm 10, 11 and 12 place each package as a copy-on-write clone of one shared copy in matchapack's store, on macOS (APFS) and on Linux (Btrfs, or XFS with reflink).
- `yarn` and its install commands with Yarn 1.10 or later, on macOS, clone each file from Yarn's own cache.
- The tree is the one plain npm or Yarn builds; [TESTING.md](https://github.com/grimmerk/matchapack/blob/main/TESTING.md) says how that is checked.
- `matchapack init` for zsh, bash and fish; `report`, `store` (`gc`, `adopt`, `verify`) and `uninstall`.
- Install with Homebrew, the install script, npm or cargo.
