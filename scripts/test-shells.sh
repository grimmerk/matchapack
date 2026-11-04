#!/usr/bin/env bash
# Routing tests for the shell integration in bash, zsh and fish, inside a
# container (fish need not be installed on the host):
#   docker run --rm -v "$PWD":/src:ro debian:bookworm-slim bash /src/scripts/test-shells.sh
# `thinclone` and `npm` are stubs: `thinclone init <shell>` prints
# shell/thinclone.sh or shell/thinclone.fish, as the real binary does (it
# embeds those files), and both record the arguments they receive. Each case
# loads the function with the README's rc line, with the shell named (shell
# detection is the real binary's job, tested by test-wrapper.sh), runs one
# command, and checks which of the two received it. The install itself is
# covered by test-wrapper.sh (zsh and bash, with the real binary).
set -uo pipefail
apt-get update -qq >/dev/null && apt-get install -y -qq fish zsh >/dev/null
# A failed install would turn every zsh/fish case into "command not found".
for s in zsh fish; do command -v "$s" >/dev/null || { echo "setup failed: no $s" >&2; exit 2; }; done
bash --version | head -1; zsh --version; fish --version

tc=/tmp/stub-thinclone; np=/tmp/stub-npm; mkdir -p "$tc" "$np"
cat > "$tc/thinclone" <<'STUB'
#!/bin/sh
case "$1" in
  init) case "$2" in
          fish) exec cat /src/shell/thinclone.fish ;;
          zsh | bash) exec cat /src/shell/thinclone.sh ;;
        esac ;;
  npm) shift; printf 'THINCLONE'; for a in "$@"; do printf ' [%s]' "$a"; done; echo; exit "${STUB_RC:-0}" ;;
esac
exit 2
STUB
cat > "$np/npm" <<'STUB'
#!/bin/sh
printf 'NPM'; for a in "$@"; do printf ' [%s]' "$a"; done; echo
STUB
chmod +x "$tc/thinclone" "$np/npm"

pass=0; fail=0
for sh in bash zsh fish; do
  case $sh in
    fish) setup='type -q thinclone; and thinclone init fish | source'; run=(fish --no-config -c); st='$status' ;;
    zsh)  setup='command -v thinclone >/dev/null && eval "$(thinclone init zsh)"'; run=(zsh -f -c); st='$?' ;;
    bash) setup='command -v thinclone >/dev/null && eval "$(thinclone init bash)"'; run=(bash --norc --noprofile -c); st='$?' ;;
  esac
  check() { # name expected-output commands [PATH]
    local out
    out=$(PATH="${4:-$tc:$np:$PATH}" "${run[@]}" "$setup; $3" 2>&1)
    if [ "$out" = "$2" ]; then pass=$((pass+1)); echo "PASS $sh $1"
    else fail=$((fail+1)); echo "FAIL $sh $1: got [$out], want [$2]"; fi
  }
  check ci        "THINCLONE [ci] [--no-audit] [--no-fund]" "npm ci --no-audit --no-fund"
  check aliases   "THINCLONE [clean-install]
THINCLONE [ic]
THINCLONE [install-clean]
THINCLONE [isntall-clean]" "npm clean-install; npm ic; npm install-clean; npm isntall-clean"
  # Arguments arrive unchanged: spaces, an empty one, a glob character.
  check args      "THINCLONE [ci] [--loglevel] [a b] [] [*]" "npm ci --loglevel 'a b' '' '*'"
  check args-npm  "NPM [run] [x y] [] [*]" "npm run 'x y' '' '*'"
  check other     "NPM [install] [lodash]"  "npm install lodash"
  check run       "NPM [run] [ci]"          "npm run ci"
  check bare      "NPM"                     "npm"
  check opt-out   "NPM [ci]"                "THINCLONE=0 npm ci"
  check command   "NPM [ci]"                "command npm ci"
  check status    "THINCLONE [ci]
7"   "STUB_RC=7 npm ci; echo $st"
  # Without thinclone on PATH the rc line does nothing and npm is plain npm.
  check no-binary "NPM [ci]" "npm ci" "$np:$PATH"
done

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
