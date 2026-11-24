# Install matchapack with Claude Code

You can ask Claude Code to install and set up matchapack for you. Start Claude Code in any folder and paste this prompt:

```text
Install matchapack for me. Read all of
https://raw.githubusercontent.com/grimmerk/matchapack/main/INSTALL-WITH-CLAUDE.md
word for word, follow it, and ask me before each step that changes my machine.
```

Claude reads this file, checks what you have, and walks you through the same steps as [Install](https://github.com/grimmerk/matchapack#install) in the README. It asks before each step that changes your machine, and you can stop at any point.

The same file covers updating and removing matchapack: start the prompt with "Update matchapack for me" or "Uninstall matchapack for me" instead.

The rest of this file is written for Claude.

---

## Instructions for Claude

Read this whole file as it is, word for word (for example `curl -fsSL <the URL above>`), not as a summary: the commands and the rules below must be followed exactly.

You are installing matchapack, a command-line tool that makes `npm ci`, `npm install` and Yarn 1 installs place `node_modules` as copy-on-write clones of one shared copy of each package, so that extra git worktrees of a project cost almost no disk. It works through a line in the user's shell startup file that defines `npm` (and on macOS `yarn`) shell functions. Read the [README](https://github.com/grimmerk/matchapack#readme) if the user asks what it does.

Rules for the whole install:

- **Ask before every step that changes the machine** (steps 3 to 5 below). Say in one or two plain sentences what the step does and what it changes, then wait for a yes. Checking commands (steps 1, 2 and 6) need no question.
- **Show the exact line and the exact file before you change a shell startup file**, and add to the end of the file only. Never reorder, rewrite or delete the user's existing lines without asking about each one.
- **Never use `sudo`**, and never type a password or token for the user.
- **If a command is blocked or denied** by Claude Code's permissions (an edit of a shell startup file often is, since it changes every future shell), do not look for another way to run it. Tell the user which command it was and give two choices: approve it (in auto mode: `/permissions`, then "Recently denied", then `r` on that command) or run it themselves by typing `! <command>` in the prompt.
- **Your own shell does not pick up the new line.** Claude Code runs commands in a shell started from a snapshot taken when the session began. Until the user opens a new terminal or session, call `matchapack` by its full path where needed, and check the new line with a fresh interactive shell of the user's kind (`zsh -i -c '…'`, `bash -i -c '…'`, `fish -c '…'`), as step 6 does.
- Run one command per tool call, so each prompt the user sees is about one thing.

### Step 1: check the machine

Run these and summarise the result in a few lines:

```sh
uname -sm
echo "$SHELL"
command -v matchapack && matchapack --version || echo "matchapack: not installed"
command -v brew || echo "brew: not installed"
command -v node && node --version || echo "node: not installed"
command -v npm && npm --version || echo "npm: not installed"
command -v yarn && yarn --version || echo "yarn: not installed"
```

The `|| echo` keeps a missing tool from showing up as a failed command; tell the user that "not installed" is an expected answer.

- matchapack runs on macOS (Apple silicon and Intel) and Linux (x86_64 and arm64). On anything else (Windows, or a Windows drive under the Windows Subsystem for Linux), stop and say so.
- Node and npm (or Yarn 1) must already be installed; matchapack uses them, it does not replace them.
- matchapack works with npm 10, 11 and 12, and with Yarn 1.10 or later (Yarn on macOS only). With another npm, or Yarn 2 or later, it runs plain npm or Yarn and saves nothing: tell the user before going on.
- If matchapack is already installed, ask whether the user wants an update (the section "If the user asks to update", near the end of this file) or to check the setup (go on with step 2 and skip step 3).

### Step 2: check that the disk can clone

matchapack saves disk only on a filesystem with copy-on-write clones, and only when its store (`~/.cache/matchapack/store`) and the user's projects are on the same filesystem. Ask where the user keeps their projects (for example `~/git` or `~/code`), then check that folder and the home folder:

- macOS: `diskutil info / | grep -i personality` should say APFS. If the projects are on another volume, `df <project folder>` and `df ~` must name the same device.
- Linux: `stat -f -c %T <project folder>` should say `btrfs` or `xfs` (for XFS, `xfs_info <mount point> | grep reflink` should show `reflink=1`), and `df <project folder>` and `df ~` must name the same device.

If the disk cannot clone (ext4, say), tell the user that matchapack would still work but save nothing: every install would be plain npm. Ask whether they want to go on. If only the device differs, the store can live on the projects' volume (`MATCHAPACK_STORE`); explain that and ask before setting it.

### Step 3: install the binary

Ask first, and explain the choices:

- **Homebrew**, when `brew` is installed: `brew install grimmerk/tap/matchapack`. Recommended: it does not depend on the Node version, and `matchapack` is on the `PATH` at once.
- **The install script**, which downloads the prebuilt binary into `~/.matchapack/bin`:

  ```sh
  curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/grimmerk/matchapack/releases/latest/download/matchapack-installer.sh \
    | sh
  ```

  Tell the user that this runs an install script from matchapack's GitHub releases page, and what it changes to put `~/.matchapack/bin` on the `PATH`: it appends a line, `. "$HOME/.config/matchapack/env.sh"`, to each shell startup file it finds (`~/.profile`, which it creates if missing, and those of `~/.bashrc`, `~/.bash_profile`, `~/.bash_login` and `~/.zshrc` that exist), and writes a fish file in `~/.config/fish/conf.d/`. In this session, call the binary as `~/.matchapack/bin/matchapack` until a new terminal is open.
- **npm**: there is no npm package yet (npm refused the name `matchapack`, https://github.com/grimmerk/matchapack/issues/18). Do not run `npm i -g matchapack`: it fails. If the user asks for npm, say so and offer Homebrew or the install script.
- **cargo**, when Rust is installed: `cargo install matchapack --locked` (builds from source).

Check with `matchapack --version` (or the full path).

### Step 4: add the line to the shell startup file

Pick the file from the user's shell (`$SHELL`):

- zsh: `~/.zshrc` (or `$ZDOTDIR/.zshrc` when `ZDOTDIR` is set).
- bash: `~/.bashrc`. On macOS, Terminal opens login shells, which read `~/.bash_profile`: check that it loads `~/.bashrc`, and if it does not, tell the user and ask which file to use.
- fish: `config.fish` in fish's configuration directory, which `fish -c 'echo $__fish_config_dir'` prints (usually `~/.config/fish`; it follows `XDG_CONFIG_HOME`).

The line for zsh and bash:

```sh
command -v matchapack >/dev/null && eval "$(matchapack init)"
```

The line for fish:

```fish
type -q matchapack; and matchapack init | source
```

First, without asking, look at the file:

```sh
grep -n -e matchapack -e nvm -e fnm -e mise -e volta -e asdf -e 'brew shellenv' -e cargo ~/.zshrc
tail -n 5 ~/.zshrc
```

(with the user's file in place of `~/.zshrc`). Then:

- **If the file already has a matchapack init line**, do not add a second one. If any line that sets up the `PATH` or the Node version comes after it (the install script's `. "$HOME/.config/matchapack/env.sh"`, `brew shellenv`, rustup's, or a Node version manager's), the init line must move below them: show the user both lines and ask before moving it.
- **Otherwise**, show the line and ask, then append it at the end of the file, after a blank line. For zsh (bash: the same with `~/.bashrc`):

  ```sh
  printf '\n%s\n' 'command -v matchapack >/dev/null && eval "$(matchapack init)"' >> ~/.zshrc
  ```

  For fish, with `<dir>` the directory fish printed above:

  ```sh
  mkdir -p "<dir>"
  printf '\n%s\n' 'type -q matchapack; and matchapack init | source' >> "<dir>/config.fish"
  ```

The line must be the last line that matters: it does nothing if it runs before matchapack is on the `PATH`. Read the end of the file again afterwards and show it to the user.

Tell the user that the line also defines `mp` as a short name for `matchapack`, only when nothing named `mp` exists; `matchapack init` run by hand says whether it would. If the user does not want it, the line takes `--no-alias` (`eval "$(matchapack init --no-alias)"`; fish: `matchapack init --no-alias | source`).

### Step 5 (optional): limit it to some folders

Ask whether matchapack should handle every project, or only projects under some folders (to try it first on a few). For only some, write one path prefix per line, absolute or starting with `~/`, to `~/.config/matchapack/paths` (`~/git/app` also covers `~/git/app-feature`). Ask first, and show the file afterwards.

### Step 6: check, and finish

Check that a new shell of the user's kind has the function:

```sh
zsh -i -c 'type npm'                                  # zsh
bash -i -c 'type npm'                                 # bash (on macOS also: bash -l -i -c 'type npm')
fish -c 'functions -q npm; and echo npm is a function'  # fish
```

It should say that `npm` is a shell function. Then check that a routed command reaches matchapack, without changing anything: in one of the user's npm projects with a `package-lock.json` (ask which one), a dry run is handed to plain npm with matchapack's reason, and npm's own dry run installs nothing (`--ignore-scripts`, so that no script of the project, such as a root `prepare` or `postinstall`, can run and change its files):

```sh
zsh -i -c 'cd "<project>" && npm ci --dry-run --ignore-scripts'      # zsh
bash -i -c 'cd "<project>" && npm ci --dry-run --ignore-scripts'     # bash
fish -c 'cd "<project>"; and npm ci --dry-run --ignore-scripts'      # fish
```

The first line should be `matchapack: running plain npm instead (option --dry-run installs nothing)`. Then tell the user, in a few lines:

- to open a new terminal (and start new Claude Code sessions) so that `npm` and `yarn` go through matchapack;
- that the first `npm ci` in a project fills the store, and every later install of the same packages (a new worktree) clones them;
- that `matchapack report` shows what happened, and `MATCHAPACK=0 npm ci` runs plain npm for one command;
- that existing `node_modules` keep their full copies until they are installed again (one `npm ci`).

---

### If the user asks to update

Ask first. Then, by how matchapack was installed (`command -v matchapack` shows where it is):

- Homebrew (under `brew --prefix`): `brew upgrade matchapack`;
- the install script (`~/.matchapack/bin/matchapack`): run the install script of step 3 again;
- cargo: `cargo install matchapack --locked --force`.

Show `matchapack --version` afterwards. The shell line needs no change: it prints the shell code of the binary that is installed. Open shells use the new binary at once; new shells also get its shell code.

### If the user asks to uninstall

Ask before each part, and let the user choose:

1. Run `matchapack uninstall` first, without `--yes`: it changes nothing and lists what it would remove (the store, the run log, the settings directory), the space that frees, the lines of the startup files that mention matchapack, and the command that removes the binary. Show the user its output. Then, if they agree, `matchapack uninstall --yes`. Installed `node_modules` keep working without the store: clones are independent files.
2. Remove every line uninstall listed, from each file it named (show each line, ask): the matchapack init line and, for an install-script install, the line `. "$HOME/.config/matchapack/env.sh"`, which is usually in several files (`~/.profile`, `~/.bashrc`, `~/.bash_profile`, `~/.zshrc`). Only after the last of those lines is gone, remove the files uninstall listed (`~/.config/matchapack/env.sh`, `env.fish`, the receipt, `~/.config/fish/conf.d/matchapack.env.fish`): removing a file a startup file still loads prints an error at every shell start.
3. Remove the binary with the command uninstall printed: `brew uninstall matchapack`, `cargo uninstall matchapack`, or `rm ~/.matchapack/bin/matchapack` for the install script.

---

## What to expect from permission prompts

This file was written before matchapack's first public release, and has not yet been run end to end against a published release. Expect a permission prompt, or a denial in auto mode, at the step that edits the shell startup file (step 4), since that edit affects every future shell; the rules above say what to do then.
