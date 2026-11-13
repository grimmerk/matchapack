#!/usr/bin/env bash
# Routing tests for the shell integration in bash, zsh and fish, inside a
# container (fish need not be installed on the host):
#   docker run --rm -v "$PWD":/src:ro debian:bookworm-slim bash /src/scripts/test-shells.sh
# `matchapack`, `npm` and `yarn` are stubs: `matchapack init <shell>` prints
# shell/matchapack.sh or shell/matchapack.fish, as the real binary does (it
# embeds those files), followed by shell/matchapack-yarn.{sh,fish} when
# STUB_OS=Darwin (the real binary does that on macOS only; this container is
# Linux, so the macOS text is tested by asking the stub for it), and each
# records the arguments it receives. Each case
# loads the function with the README's rc line, with the shell named (shell
# detection is the real binary's job, tested by test-wrapper.sh), runs one
# command, and checks which of the two received it. The install itself is
# covered by test-wrapper.sh (zsh and bash, with the real binary).
set -uo pipefail
apt-get update -qq >/dev/null && apt-get install -y -qq fish zsh >/dev/null
# A failed install would turn every zsh/fish case into "command not found".
for s in zsh fish; do command -v "$s" >/dev/null || { echo "setup failed: no $s" >&2; exit 2; }; done
bash --version | head -1; zsh --version; fish --version

tc=/tmp/stub-matchapack; np=/tmp/stub-npm; mkdir -p "$tc" "$np"
cat > "$tc/matchapack" <<'STUB'
#!/bin/sh
case "$1" in
  init) case "$2" in
          fish) cat /src/shell/matchapack.fish; [ "${STUB_OS-}" = Darwin ] && cat /src/shell/matchapack-yarn.fish; exit 0 ;;
          zsh | bash) cat /src/shell/matchapack.sh; [ "${STUB_OS-}" = Darwin ] && cat /src/shell/matchapack-yarn.sh; exit 0 ;;
        esac ;;
  npm) shift; printf 'MATCHAPACK'; for a in "$@"; do printf ' [%s]' "$a"; done; echo; exit "${STUB_RC:-0}" ;;
  yarn) shift; printf 'MATCHAPACK-YARN'; for a in "$@"; do printf ' [%s]' "$a"; done; echo; exit "${STUB_RC:-0}" ;;
esac
exit 2
STUB
cat > "$np/npm" <<'STUB'
#!/bin/sh
printf 'NPM'; for a in "$@"; do printf ' [%s]' "$a"; done; echo
STUB
cat > "$np/yarn" <<'STUB'
#!/bin/sh
printf 'YARN'; for a in "$@"; do printf ' [%s]' "$a"; done; echo
STUB
chmod +x "$tc/matchapack" "$np/npm" "$np/yarn"

pass=0; fail=0
for sh in bash zsh fish; do
  case $sh in
    fish) setup='type -q matchapack; and matchapack init fish | source'; run=(fish --no-config -c); st='$status' ;;
    zsh)  setup='command -v matchapack >/dev/null && eval "$(matchapack init zsh)"'; run=(zsh -f -c); st='$?' ;;
    bash) setup='command -v matchapack >/dev/null && eval "$(matchapack init bash)"'; run=(bash --norc --noprofile -c); st='$?' ;;
  esac
  check() { # name expected-output commands [PATH]   (STUB_OS: the platform init prints for)
    local out
    out=$(PATH="${4:-$tc:$np:$PATH}" STUB_OS="${os-Linux}" "${run[@]}" "$setup; $3" 2>&1)
    if [ "$out" = "$2" ]; then pass=$((pass+1)); echo "PASS $sh${os:+ $os} $1"
    else fail=$((fail+1)); echo "FAIL $sh${os:+ $os} $1: got [$out], want [$2]"; fi
  }
  os=Linux
  check ci        "MATCHAPACK [ci] [--no-audit] [--no-fund]" "npm ci --no-audit --no-fund"
  check aliases   "MATCHAPACK [clean-install]
MATCHAPACK [ic]
MATCHAPACK [install-clean]
MATCHAPACK [isntall-clean]" "npm clean-install; npm ic; npm install-clean; npm isntall-clean"
  # Arguments arrive unchanged: spaces, an empty one, a glob character.
  check args      "MATCHAPACK [ci] [--loglevel] [a b] [] [*]" "npm ci --loglevel 'a b' '' '*'"
  check args-npm  "NPM [run] [x y] [] [*]" "npm run 'x y' '' '*'"
  check other     "NPM [install] [lodash]"  "npm install lodash"
  check run       "NPM [run] [ci]"          "npm run ci"
  check bare      "NPM"                     "npm"
  check opt-out   "NPM [ci]"                "MATCHAPACK=0 npm ci"
  check command   "NPM [ci]"                "command npm ci"
  check status    "MATCHAPACK [ci]
7"   "STUB_RC=7 npm ci; echo $st"
  # Without matchapack on PATH the rc line does nothing and npm is plain npm.
  check no-binary "NPM [ci]" "npm ci" "$np:$PATH"
  # Linux: no yarn function (plain Yarn 1 already clones there).
  check yarn-plain "YARN
YARN [install] [--frozen-lockfile]" "yarn; yarn install --frozen-lockfile"

  # macOS: the yarn function. Installs go to `matchapack yarn`, with their
  # arguments unchanged; everything else, --version and --help included,
  # straight to yarn without starting matchapack.
  os=Darwin
  check npm-ci-mac  "MATCHAPACK [ci]"                    "npm ci"
  check yarn-bare   "MATCHAPACK-YARN"                    "yarn"
  check yarn-install "MATCHAPACK-YARN [install] [--frozen-lockfile]" "yarn install --frozen-lockfile"
  check yarn-flag   "MATCHAPACK-YARN [--frozen-lockfile] [--cache-folder] [/c d]" "yarn --frozen-lockfile --cache-folder '/c d'"
  check yarn-args   "MATCHAPACK-YARN [install] [a b] [] [*]" "yarn install 'a b' '' '*'"
  check yarn-add    "YARN [add] [x]"                     "yarn add x"
  check yarn-run    "YARN [run] [install]"               "yarn run install"
  check yarn-version "YARN [--version]
YARN [-v]
YARN [--help]
YARN [-h] [install]" "yarn --version; yarn -v; yarn --help; yarn -h install"
  check yarn-opt-out "YARN [install]"                    "MATCHAPACK=0 yarn install"
  check yarn-command "YARN"                              "command yarn"
  check yarn-status "MATCHAPACK-YARN [install]
5"   "STUB_RC=5 yarn install; echo $st"
  check yarn-no-binary "YARN" "yarn" "$np:$PATH"
  unset os
done

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
