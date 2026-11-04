# thinclone shell integration for zsh and bash.
#
# Routes a bare `npm ci` (and its aliases) through `thinclone npm`, which
# installs from the copy-on-write store when it is safe to and otherwise runs
# the real npm with the same arguments. Every other npm command goes straight
# to npm without starting thinclone.
#
# Install: add to ~/.zshrc or ~/.bashrc (the binary prints this file)
#     command -v thinclone >/dev/null && eval "$(thinclone init)"
# Sourcing this file from a checkout works too:
#     [ -f "$HOME/git/thinclone/shell/thinclone.sh" ] && . "$HOME/git/thinclone/shell/thinclone.sh"
# Bypass for one command:  THINCLONE=0 npm ci     or     command npm ci
# Remove: delete that line; already installed node_modules keep working.
# `function name {` rather than `name() {`: zsh would expand an existing
# `alias npm=…` inside `npm() {` and fail. An alias still wins over this
# function when both exist, so remove such an alias to use thinclone.
function npm {
  case "${1-}" in
    ci | clean-install | ic | install-clean | isntall-clean)
      if [ "${THINCLONE-1}" != 0 ] && command -v thinclone >/dev/null 2>&1; then
        thinclone npm "$@"
        return $?
      fi
      ;;
  esac
  command npm "$@"
}
