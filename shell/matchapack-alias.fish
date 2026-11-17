# `matchapack init --alias NAME` (fish): NAME is a short name for
# matchapack, e.g. `mp npm ci`, `mp store gc`. Removed with the init line.
function __MATCHAPACK_ALIAS__ --wraps matchapack --description 'matchapack'
    matchapack $argv
end
