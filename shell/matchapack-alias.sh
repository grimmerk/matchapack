# matchapack's short name (zsh and bash), e.g. `mp npm ci`, `mp store gc`:
# `mp` unless `matchapack init --alias NAME` picks another or --no-alias
# none. A shell function, defined only when nothing of that name exists yet
# (an alias, function, builtin or command on PATH), checked at every shell
# start. A function, not an alias: a later `mp() { … }` in the startup file
# then replaces it (an alias would be expanded in that line). The `function`
# keyword: an alias `mp` defined earlier is not expanded in the name, which
# `mp() {` would be, a parse error. Removed with the init line.
command -v __MATCHAPACK_ALIAS__ >/dev/null 2>&1 || function __MATCHAPACK_ALIAS__ { matchapack "$@"; }
