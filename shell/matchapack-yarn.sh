# matchapack shell integration for Yarn 1, zsh and bash (macOS only:
# `matchapack init` prints it after shell/matchapack.sh there, and not on
# Linux, where plain Yarn 1 already clones).
#
# Routes through `matchapack yarn` the yarn commands that copy packages into
# node_modules: a bare `yarn`, `yarn install …`, `yarn --<flag> …` (an
# install with flags first, `yarn --frozen-lockfile`), `yarn add`,
# `upgrade`, `remove`, `upgrade-interactive`, and `yarn workspace <name>
# add|remove|upgrade|upgrade-interactive …`. matchapack runs them with
# clones of Yarn's cache when it is safe to and otherwise runs the real yarn
# with the same arguments (a Yarn 2+ project is told from its files, without
# starting node). Every other yarn command (`yarn global …`, `yarn run …`,
# `yarn workspace <name> install`, …; README "What stays plain Yarn"), and
# `yarn --version` / `--help`, goes straight to yarn without starting
# matchapack.
#
# Bypass for one command:  MATCHAPACK=0 yarn     or     command yarn
# From a checkout, source this file after shell/matchapack.sh.
# `function name {` rather than `name() {`, as in shell/matchapack.sh.
function yarn {
  local route=
  case "${1-}" in
    -v | --version | -h | --help) ;;
    "" | install | add | upgrade | remove | upgrade-interactive | upgradeInteractive | -*) route=1 ;;
    workspace)
      case "${3-}" in
        add | remove | upgrade | upgrade-interactive | upgradeInteractive) route=1 ;;
      esac
      ;;
  esac
  if [ -n "$route" ] && [ "${MATCHAPACK-1}" != 0 ] && command -v matchapack >/dev/null 2>&1; then
    matchapack yarn "$@"
    return $?
  fi
  command yarn "$@"
}
