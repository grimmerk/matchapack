# lockclone shell integration for zsh and bash.
#
# Routes a bare `npm ci` (and its aliases) through `lockclone npm`, which
# installs from the copy-on-write store when it is safe to and otherwise runs
# the real npm with the same arguments. Every other npm command goes straight
# to npm without starting lockclone.
#
# Install: add to ~/.zshrc or ~/.bashrc
#     [ -f "$HOME/git/lockclone/shell/lockclone.sh" ] && . "$HOME/git/lockclone/shell/lockclone.sh"
# Bypass for one command:  LOCKCLONE=0 npm ci     or     command npm ci
# Remove: delete that line; already installed node_modules keep working.
npm() {
  case "${1-}" in
    ci | clean-install | ic | install-clean | isntall-clean)
      if [ "${LOCKCLONE-1}" != 0 ] && command -v lockclone >/dev/null 2>&1; then
        lockclone npm "$@"
        return $?
      fi
      ;;
  esac
  command npm "$@"
}
