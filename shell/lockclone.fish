# lockclone shell integration for fish. Same behavior as lockclone.sh.
# Install: add to ~/.config/fish/config.fish
#     test -f ~/git/lockclone/shell/lockclone.fish; and source ~/git/lockclone/shell/lockclone.fish
# Bypass for one command:  LOCKCLONE=0 npm ci     or     command npm ci
function npm --wraps npm --description 'npm; a bare npm ci goes through lockclone'
    if contains -- "$argv[1]" ci clean-install ic install-clean isntall-clean
        and test "$LOCKCLONE" != 0
        and type -q lockclone
        lockclone npm $argv
        return $status
    end
    command npm $argv
end
