# matchapack

Makes `npm ci`, `npm install` (also `npm update` and `npm uninstall`) and Yarn 1 installs place `node_modules` as copy-on-write clones, so each extra checkout or git worktree of a repo costs almost no disk. The full documentation is on GitHub: <https://github.com/grimmerk/matchapack>.

This npm package is one of several ways to install the matchapack binary. It holds the binaries for macOS (Apple silicon and Intel) and Linux (x86_64 and arm64; static, so any Linux distribution), and its `matchapack` command is a small shell script that runs the one for your machine. It has no install script and downloads nothing, so it works with `--ignore-scripts`. Install it globally:

```sh
npm i -g matchapack
```

Then add matchapack's line to your shell startup file, as the GitHub README describes (zsh or bash), as the last line, below anything that sets up your `PATH` or your Node version:

```sh
command -v matchapack >/dev/null && eval "$(matchapack init)"
```

The global install is the supported way to use this package. matchapack is a tool for your shell, not a dependency of a project; a project install may work, but Yarn's Plug'n'Play installs are not supported.

## With nvm, fnm, mise or another Node version manager

A global npm package belongs to one Node version. After you switch to another Node version, `matchapack` is no longer on your `PATH`, and the shell line above then skips it without a message: `npm ci` runs as plain npm and nothing is cloned. So:

- **nvm:** add `matchapack` to `$NVM_DIR/default-packages` (one package name per line), and nvm installs it with every new Node version. For the versions you already have, run `npm i -g matchapack` in each.
- **fnm:** install it in each Node version you use (`npm i -g matchapack` after `fnm use <version>`).
- **mise:** `mise use -g npm:matchapack` installs it in mise's own directory, apart from the global packages of any Node version.
- Or install matchapack with Homebrew or the install script instead, which do not depend on the Node version (see the GitHub README).

In the startup file, matchapack's line must come after the version manager's own lines (`. "$NVM_DIR/nvm.sh"`, `eval "$(fnm env)"`, `eval "$(mise activate zsh)"`), since they put the Node version, and with it `matchapack`, on your `PATH`.

## Uninstall

`matchapack uninstall` lists (and with `--yes` removes) matchapack's own data. Then remove the shell line, and the package (with a version manager, in each Node version you installed it in):

```sh
npm uninstall -g matchapack
```
