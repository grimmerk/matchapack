# matchapack shell integration for Yarn 1, fish (macOS only; see
# shell/matchapack-yarn.sh). Same behavior as that file.
# Bypass for one command:  MATCHAPACK=0 yarn     or     command yarn
function yarn --wraps yarn --description 'yarn; installs and yarn add/upgrade/remove go through matchapack'
    if not contains -- "$argv[1]" -v --version -h --help
        and begin
            test (count $argv) -eq 0
            or contains -- "$argv[1]" install add upgrade remove upgrade-interactive upgradeInteractive
            or string match -q -- '-*' "$argv[1]"
            or begin
                test "$argv[1]" = workspace
                and contains -- "$argv[3]" add remove upgrade
            end
        end
        and test "$MATCHAPACK" != 0
        and type -q matchapack
        matchapack yarn $argv
        return $status
    end
    command yarn $argv
end
