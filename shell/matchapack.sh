# matchapack shell integration for zsh and bash.
#
# Routes a bare `npm ci` (and its aliases) through `matchapack npm`, which
# installs from the copy-on-write store when it is safe to and otherwise runs
# the real npm with the same arguments. Every other npm command goes straight
# to npm without starting matchapack.
#
# Install: add to ~/.zshrc or ~/.bashrc (the binary prints this file)
#     command -v matchapack >/dev/null && eval "$(matchapack init)"
# Sourcing this file from a checkout works too:
#     [ -f "$HOME/git/matchapack/shell/matchapack.sh" ] && . "$HOME/git/matchapack/shell/matchapack.sh"
# Bypass for one command:  MATCHAPACK=0 npm ci     or     command npm ci
# Remove: delete that line; already installed node_modules keep working.
# On macOS `matchapack init` also prints shell/matchapack-yarn.sh after this
# file: a `yarn` function for Yarn 1 projects.
# `function name {` rather than `name() {`: zsh would expand an existing
# `alias npm=…` inside `npm() {` and fail. An alias still wins over this
# function when both exist, so remove such an alias to use matchapack.
function npm {
  case "${1-}" in
    ci | clean-install | ic | install-clean | isntall-clean)
      if [ "${MATCHAPACK-1}" != 0 ] && command -v matchapack >/dev/null 2>&1; then
        matchapack npm "$@"
        return $?
      fi
      ;;
  esac
  command npm "$@"
}
