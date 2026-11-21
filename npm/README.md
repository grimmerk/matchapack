# matchapack

Makes `npm ci`, `npm install` (also `npm update` and `npm uninstall`) and Yarn 1 installs place `node_modules` as copy-on-write clones, so each extra checkout or git worktree of a repo costs almost no disk. The full documentation is on GitHub: <https://github.com/grimmerk/matchapack>.

This npm package is one of several ways to install the matchapack binary (macOS on Apple silicon and Intel, Linux with glibc on x64 and arm64). It has no install script and downloads nothing: npm installs the binary for your platform as an optional dependency (`@matchapack/<os>-<cpu>`), so it works with `--ignore-scripts`.

```sh
npm i -g matchapack
```

Then add the shell line to your startup file, as the GitHub README describes (zsh or bash):

```sh
command -v matchapack >/dev/null && eval "$(matchapack init)"
```

## With nvm, fnm or another Node version manager

A global npm package belongs to one Node version. After you switch to another Node version (`nvm use`, fnm's automatic switch), `matchapack` is no longer on your `PATH`, and the shell line above then skips it without a message: `npm ci` runs as plain npm and nothing is cloned. Either install matchapack in each Node version you use (`nvm install <version> --reinstall-packages-from=<current>` carries global packages over), or install it with Homebrew or the install script instead, which do not depend on the Node version (see the GitHub README).

## Uninstall

`matchapack uninstall` lists (and with `--yes` removes) matchapack's own data. Then remove the shell line, and the package:

```sh
npm uninstall -g matchapack
```
