# matchapack shell integration for zsh and bash.
#
# Routes `npm ci`, `npm install`, `npm update` and `npm uninstall` (and their
# aliases, as npm itself names them) through `matchapack npm`, which installs
# from the copy-on-write store when it is safe to and otherwise runs the real
# npm with the same arguments. Every other npm command goes straight to npm
# without starting matchapack. The list of first words is the one in
# src/wrapper.rs (`ROUTED`); a test keeps the three lists equal.
#
# Install: add to ~/.zshrc or ~/.bashrc (the binary prints this file)
#     command -v matchapack >/dev/null && eval "$(matchapack init)"
# Sourcing this file from a checkout works too:
#     [ -f "$HOME/git/matchapack/shell/matchapack.sh" ] && . "$HOME/git/matchapack/shell/matchapack.sh"
# Bypass for one command:  MATCHAPACK=0 npm install     or     command npm install
# Routed to the matchapack on PATH (still executable) or to your own shell
# function named matchapack; an alias of that name is not used (not
# expanded inside a function): npm then runs plain.
# Remove: delete that line; already installed node_modules keep working.
# On macOS `matchapack init` also prints shell/matchapack-yarn.sh after this
# file: a `yarn` function for Yarn 1 projects.
# `function name {` rather than `name() {`: zsh would expand an existing
# `alias npm=…` inside `npm() {` and fail. An alias still wins over this
# function when both exist, so remove such an alias to use matchapack.
function npm {
  case "${1-}" in
    ci | clean-install | ic | install-clean | isntall-clean | \
    install | i | in | ins | inst | insta | instal | isnt | isnta | isntal | isntall | add | \
    update | up | upgrade | udpate | u | \
    uninstall | un | unlink | remove | rm | r)
      # Routed to a matchapack that `command -v` names as an executable
      # file (any answer with a slash, a relative PATH entry's `bin/…`
      # too; bash answers from its hash table, so a binary removed since,
      # by `matchapack uninstall`, must still be there), or as a shell
      # function of that name. An alias is not routed: it is not expanded
      # inside this function, and its description is never an executable
      # file. The lookup is an `if` condition, so that a
      # shell under `set -e` without matchapack goes on to plain npm.
      local mp=
      if [ "${MATCHAPACK-1}" != 0 ]; then
        mp=$(command -v matchapack 2>/dev/null) || mp=
      fi
      case $mp in
        */*) [ -x "$mp" ] || mp= ;;
        matchapack) ;;
        *) mp= ;;
      esac
      if [ -n "$mp" ]; then
        matchapack npm "$@"
        return $?
      fi
      ;;
  esac
  command npm "$@"
}
