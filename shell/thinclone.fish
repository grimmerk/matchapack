# thinclone shell integration for fish. Same behavior as thinclone.sh.
# Install: add to ~/.config/fish/config.fish (the binary prints this file)
#     type -q thinclone; and thinclone init fish | source
# Bypass for one command:  THINCLONE=0 npm ci     or     command npm ci
function npm --wraps npm --description 'npm; a bare npm ci goes through thinclone'
    if contains -- "$argv[1]" ci clean-install ic install-clean isntall-clean
        and test "$THINCLONE" != 0
        and type -q thinclone
        thinclone npm $argv
        return $status
    end
    command npm $argv
end
