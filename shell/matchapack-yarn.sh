# matchapack shell integration for Yarn 1, zsh and bash (macOS only:
# `matchapack init` prints it after shell/matchapack.sh there, and not on
# Linux, where plain Yarn 1 already clones).
#
# Routes a bare `yarn`, `yarn install …` and `yarn --<flag> …` (an install
# with flags first, `yarn --frozen-lockfile`) through `matchapack yarn`,
# which installs with clones of Yarn's cache when it is safe to and otherwise
# runs the real yarn with the same arguments (a Yarn 2+ project is told from
# its files, without starting node). Every other yarn command, and
# `yarn --version` / `--help`, goes straight to yarn without starting
# matchapack.
#
# Bypass for one command:  MATCHAPACK=0 yarn     or     command yarn
# From a checkout, source this file after shell/matchapack.sh.
# `function name {` rather than `name() {`, as in shell/matchapack.sh.
function yarn {
  case "${1-}" in
    -v | --version | -h | --help) ;;
    "" | install | -*)
      if [ "${MATCHAPACK-1}" != 0 ] && command -v matchapack >/dev/null 2>&1; then
        matchapack yarn "$@"
        return $?
      fi
      ;;
  esac
  command yarn "$@"
}
