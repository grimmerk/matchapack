# matchapack's short name (zsh and bash), e.g. `mp npm ci`, `mp store gc`:
# `mp` unless `matchapack init --alias NAME` picks another or --no-alias
# none. Defined only when nothing of that name exists yet (an alias,
# function, builtin or command on PATH), checked at every shell start.
# Removed with the init line.
command -v __MATCHAPACK_ALIAS__ >/dev/null 2>&1 || alias __MATCHAPACK_ALIAS__=matchapack
