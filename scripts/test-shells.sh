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
#
# With --real (in an image with cargo and network access), fish also runs
# against the REAL matchapack, built here as scripts/linux-test.sh builds it,
# with real npm and Yarn 1 on Node 24 (fish is not installed on the macOS
# host):
#   docker run --rm -v "$PWD":/src:ro rust:1-bookworm bash /src/scripts/test-shells.sh --real
# The npm function (`matchapack init fish | source`) must take a bare npm ci
# through matchapack and pass npm ci's status back. The yarn function is
# macOS-only, so it is sourced by hand (shell/matchapack-yarn.fish): on Linux
# `matchapack yarn` is plain Yarn by design, so these cases check the route,
# the arguments and the status end to end, not clones.
set -uo pipefail
real=; [ "${1-}" = --real ] && real=1
apt-get update -qq >/dev/null && apt-get install -y -qq fish zsh ${real:+python3 xz-utils} >/dev/null
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

if [ -n "$real" ]; then
  echo "== fish with the real matchapack"
  arch=$(uname -m); case $arch in aarch64) narch=arm64;; x86_64) narch=x64;; esac
  curl -fsSL "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-$narch.tar.xz" | tar -xJ -C /opt
  nodebin=/opt/node-v24.22.1-linux-$narch/bin; export PATH=$nodebin:$PATH
  mkdir -p /build && cp -r /src /build/src && rm -rf /build/src/target
  (cd /build/src && CARGO_TARGET_DIR=/build/target cargo build --release -q) || { echo "build failed" >&2; exit 2; }
  npm i -g --silent yarn@1.22.22 || { echo "setup failed: yarn" >&2; exit 2; }
  yjs=$(npm root -g)/yarn/bin/yarn.js
  # (cd first: the server must not keep this shell's directory busy)
  (cd /src/fixtures/optional-upward/tarballs && python3 -m http.server 48321 --bind 127.0.0.1 >/dev/null 2>&1 &)
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/y1-a-1.0.0.tgz && break; sleep 0.2; done
  # PATH: the real binary, a `yarn` that records how it was started (every
  # argument; MATCHAPACK, which `matchapack yarn` sets to 0 when it hands
  # over to Yarn, unset otherwise) and then runs the real Yarn, node and npm.
  rb=/tmp/real-bin; mkdir -p "$rb"; ln -s /build/target/release/matchapack "$rb/matchapack"
  printf '#!/bin/sh\n{ printf "MATCHAPACK=%%s" "${MATCHAPACK-unset}"; for a in "$@"; do printf " [%%s]" "$a"; done; echo; } >> /tmp/yarn-calls\nexec node "%s" "$@"\n' "$yjs" > "$rb/yarn"
  chmod +x "$rb/yarn"
  export MATCHAPACK_LOG_DIR=/tmp/real-log MATCHAPACK_STORE=/tmp/real-store MATCHAPACK_CONFIG_DIR=/tmp/real-config YARN_DISABLE_SELF_UPDATE_CHECK=true
  unset MATCHAPACK_ENGINE MATCHAPACK_PATHS MATCHAPACK MATCHAPACK_YARN_JS
  rsetup='type -q matchapack; and matchapack init fish | source'
  # rproj <name> npm|yarn [postinstall exit code]: a fresh project.
  rproj() {
    local d=/tmp/rp/$1; rm -rf "${d:?}"; mkdir -p "$d"
    if [ "$2" = npm ]; then cp /src/fixtures/optional-upward/package.json /src/fixtures/optional-upward/package-lock.json "$d/"
    else cp /src/fixtures/yarn1-small/package.json /src/fixtures/yarn1-small/yarn.lock "$d/"; fi
    [ -n "${3-}" ] && python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"exit "+sys.argv[2]}; json.dump(d, open(p,"w"))' "$d/package.json" "$3"
    echo "$d"
  }
  # rfish <dir> <fish commands> [yarn]: output in $out; yarn: also source the
  # macOS yarn function by hand.
  rfish() {
    local s=$rsetup; [ "${3-}" = yarn ] && s="$s; source /src/shell/matchapack-yarn.fish"
    : > /tmp/yarn-calls
    out=$(cd "$1" && PATH="$rb:$PATH" fish --no-config -c "$s; $2" 2>&1)
  }
  logn() { { wc -l < "$MATCHAPACK_LOG_DIR/log.jsonl"; } 2>/dev/null || echo 0; }
  lastrec() { tail -1 "$MATCHAPACK_LOG_DIR/log.jsonl" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }
  rcheck() { # name condition-result detail
    if [ "$2" = 0 ]; then pass=$((pass+1)); echo "PASS fish real $1"
    else fail=$((fail+1)); echo "FAIL fish real $1: $3"; printf '%s\n' "$out" | tail -6 | sed 's/^/    /'; fi
  }
  rfish /tmp 'type -t npm; functions -q yarn; and echo yarn-defined; or echo no-yarn-function'
  [ "$out" = "function
no-yarn-function" ]; rcheck "init: npm is a function, no yarn function on Linux" $? "[$out]"

  d=$(rproj ci npm); n0=$(logn)
  rfish "$d" 'npm ci --no-audit --no-fund; echo "status $status"'
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(logn)" = $((n0+1)) ] && [ "$(lastrec 'r["outcome"], r["args"]')" = "('installed', ['ci', '--no-audit', '--no-fund'])" ] \
    && printf '%s\n' "$out" | grep -q '^matchapack: npm ci with clones' && [ -d "$d/node_modules/lc-r" ]
  rcheck "npm ci → matchapack npm (logged, arguments unchanged), installed, status 0" $? "log $n0 → $(logn)"

  d=$(rproj cmd npm); n0=$(logn)
  rfish "$d" 'command npm ci --no-audit --no-fund; echo "status $status"'
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(logn)" = "$n0" ] && ! printf '%s\n' "$out" | grep -q '^matchapack' && [ -d "$d/node_modules/lc-r" ]
  rcheck "command npm ci → plain npm, matchapack never started" $? "log $n0 → $(logn)"

  # A failing npm ci (the root postinstall exits 7): the function returns
  # what plain npm returns.
  d=$(rproj failplain npm 7); rfish "$d" 'command npm ci --no-audit --no-fund; echo "status $status"'; want=$(printf '%s\n' "$out" | tail -1)
  d=$(rproj fail npm 7); n0=$(logn); rfish "$d" 'npm ci --no-audit --no-fund; echo "status $status"'
  [ "$want" != "status 0" ] && [ "$(printf '%s\n' "$out" | tail -1)" = "$want" ] && [ "$(logn)" = $((n0+1)) ] && [ "$(lastrec 'r["outcome"]')" = npm_failed ]
  rcheck "a failing npm ci: status ($want) passes through the function, as plain npm's" $? "plain $want, got $(printf '%s\n' "$out" | tail -1), outcome $(lastrec 'r["outcome"]' 2>/dev/null)"

  # The yarn function, sourced by hand. Arguments with spaces and quotes
  # reach Yarn unchanged, through `matchapack yarn` (MATCHAPACK=0 seen),
  # and Yarn uses them (the cache folder named so is made and filled).
  d=$(rproj yargs yarn)
  rfish "$d" 'yarn install --frozen-lockfile --cache-folder "/tmp/c d/it'\''s \"q\"" --non-interactive; echo "status $status"' yarn
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(cat /tmp/yarn-calls)" = "MATCHAPACK=0 [install] [--frozen-lockfile] [--cache-folder] [/tmp/c d/it's \"q\"] [--non-interactive]" ] \
    && [ -d "/tmp/c d/it's \"q\"/v6" ] && [ -f "$d/node_modules/.yarn-integrity" ]
  rcheck "yarn install with spaced and quoted arguments → matchapack yarn → Yarn, arguments unchanged, installed" $? "calls [$(cat /tmp/yarn-calls)]"

  d=$(rproj ybare yarn); rfish "$d" 'yarn; echo "status $status"' yarn
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(cat /tmp/yarn-calls)" = "MATCHAPACK=0" ] && [ -f "$d/node_modules/.yarn-integrity" ]
  rcheck "bare yarn → matchapack yarn → Yarn, installed" $? "calls [$(cat /tmp/yarn-calls)]"

  d=$(rproj yflag yarn); rfish "$d" 'yarn --frozen-lockfile --non-interactive; echo "status $status"' yarn
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(cat /tmp/yarn-calls)" = "MATCHAPACK=0 [--frozen-lockfile] [--non-interactive]" ]
  rcheck "yarn --frozen-lockfile → matchapack yarn → Yarn" $? "calls [$(cat /tmp/yarn-calls)]"

  rfish /tmp 'yarn --version; echo "status $status"' yarn
  [ "$out" = "1.22.22
status 0" ] && [ "$(cat /tmp/yarn-calls)" = "MATCHAPACK=unset [--version]" ]
  rcheck "yarn --version → Yarn directly, matchapack never started" $? "calls [$(cat /tmp/yarn-calls)]"

  d=$(rproj yfailplain yarn 7); rfish "$d" 'command yarn install --frozen-lockfile; echo "status $status"' yarn; want=$(printf '%s\n' "$out" | tail -1)
  d=$(rproj yfail yarn 7); rfish "$d" 'yarn install --frozen-lockfile; echo "status $status"' yarn
  [ "$want" != "status 0" ] && [ "$(printf '%s\n' "$out" | tail -1)" = "$want" ] && [ "$(cat /tmp/yarn-calls)" = "MATCHAPACK=0 [install] [--frozen-lockfile]" ]
  rcheck "a failing yarn install: status ($want) passes through the function and matchapack, as plain Yarn's" $? "plain $want, got $(printf '%s\n' "$out" | tail -1), calls [$(cat /tmp/yarn-calls)]"
fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
