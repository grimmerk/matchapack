# matchapack shell integration for fish. Same behavior as matchapack.sh.
# Install: add to ~/.config/fish/config.fish (the binary prints this file)
#     type -q matchapack; and matchapack init | source
# Bypass for one command:  MATCHAPACK=0 npm ci     or     command npm ci
function npm --wraps npm --description 'npm; a bare npm ci goes through matchapack'
    if contains -- "$argv[1]" ci clean-install ic install-clean isntall-clean
        and test "$MATCHAPACK" != 0
        and type -q matchapack
        matchapack npm $argv
        return $status
    end
    command npm $argv
end
