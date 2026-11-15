# matchapack shell integration for fish. Same behavior as matchapack.sh.
# Install: add to ~/.config/fish/config.fish (the binary prints this file)
#     type -q matchapack; and matchapack init | source
# Bypass for one command:  MATCHAPACK=0 npm install     or     command npm install
# On macOS `matchapack init` also prints shell/matchapack-yarn.fish after this.
function npm --wraps npm --description 'npm; npm ci, install, update and uninstall go through matchapack'
    if contains -- "$argv[1]" ci clean-install ic install-clean isntall-clean \
        install i in ins inst insta instal isnt isnta isntal isntall add \
        update up upgrade udpate u \
        uninstall un unlink remove rm r
        and test "$MATCHAPACK" != 0
        and type -q matchapack
        matchapack npm $argv
        return $status
    end
    command npm $argv
end
