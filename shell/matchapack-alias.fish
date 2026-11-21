# matchapack's short name (fish), e.g. `mp npm ci`, `mp store gc`: `mp`
# unless `matchapack init --alias NAME` picks another or --no-alias none.
# Defined only when nothing of that name exists yet (a function, builtin or
# command on PATH), checked at every shell start. Removed with the init line.
type -q __MATCHAPACK_ALIAS__; or function __MATCHAPACK_ALIAS__ --wraps matchapack --description 'matchapack'
    matchapack $argv
end
