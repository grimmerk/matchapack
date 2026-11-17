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
# With --real, fish also runs against the REAL matchapack, with real npm and
# Yarn 1.22.22 (installed into a scratch prefix) on Node 24. Two ways:
# - in a container with cargo and network access (fish is not installed on
#   the macOS host); the binary is built here as scripts/linux-test.sh
#   builds it, Node 24.22.1 is downloaded:
#     docker run --rm -v "$PWD":/src:ro rust:1-bookworm bash /src/scripts/test-shells.sh --real
# - on a Linux host (the CI job) with MATCHAPACK_BIN naming a built binary,
#   and fish, python3, node and npm already on PATH: nothing is installed
#   with apt, nothing is built, and only the real cases run (the stub cases
#   are the container run's):
#     MATCHAPACK_BIN=target/release/matchapack scripts/test-shells.sh --real
#   Its files go to a new folder under RUNNER_TEMP (else TMPDIR, /tmp);
#   npm's and Yarn's caches are pointed there too.
# The npm function (`matchapack init fish | source`) must take a bare npm ci
# through matchapack and pass npm ci's status back. The yarn function is
# macOS-only, so it is sourced by hand (shell/matchapack-yarn.fish): on Linux
# `matchapack yarn` is plain Yarn by design, so these cases check the route,
# the arguments and the status end to end, not clones.
set -uo pipefail
real=; [ "${1-}" = --real ] && real=1
host=; [ -n "$real" ] && [ -n "${MATCHAPACK_BIN-}" ] && host=1
src=$(cd "$(dirname "$0")/.." && pwd)
pass=0; fail=0
if [ -z "$host" ]; then
apt-get update -qq >/dev/null && apt-get install -y -qq fish zsh ${real:+python3 xz-utils} >/dev/null
# A failed install would turn every zsh/fish case into "command not found".
for s in zsh fish; do command -v "$s" >/dev/null || { echo "setup failed: no $s" >&2; exit 2; }; done
bash --version | head -1; zsh --version; fish --version

tc=/tmp/stub-matchapack; np=/tmp/stub-npm; mkdir -p "$tc" "$np"
cat > "$tc/matchapack" <<'STUB'
#!/bin/sh
case "$1" in
  init) case "$2" in
          fish) cat /src/shell/matchapack.fish; [ "${STUB_OS-}" = Darwin ] && cat /src/shell/matchapack-yarn.fish; ext=fish ;;
          zsh | bash) cat /src/shell/matchapack.sh; [ "${STUB_OS-}" = Darwin ] && cat /src/shell/matchapack-yarn.sh; ext=sh ;;
          *) exit 2 ;;
        esac
        # --alias NAME: the binary appends shell/matchapack-alias.{sh,fish}
        # with the name in place of the placeholder (it checks the name).
        [ "${3-}" = --alias ] && sed "s/__MATCHAPACK_ALIAS__/$4/g" "/src/shell/matchapack-alias.$ext"
        exit 0 ;;
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

# The routed first words, from the one table (src/wrapper.rs ROUTED).
routed=$(grep -Eo '^    \("[^"]+", "' /src/src/wrapper.rs | cut -d'"' -f2)
[ "$(echo "$routed" | wc -w)" -ge 28 ] || { echo "setup failed: could not read ROUTED from src/wrapper.rs" >&2; exit 2; }
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
  check install   "MATCHAPACK [install] [lodash]" "npm install lodash"
  # Every first word of src/wrapper.rs's ROUTED table goes to matchapack
  # (wrapper.rs's unit test keeps the table and both shell files equal;
  # this runs the functions themselves).
  for w in $routed; do
    check "routed-$w" "MATCHAPACK [$w] [x]" "npm $w x"
  done
  # Commands that are not routed (npm runs them itself; NOTES "The install
  # family"), and a flag first: npm directly.
  check not-routed "NPM [run] [build]
NPM [test]
NPM [--version]
NPM [exec] [x]
NPM [link]
NPM [dedupe]
NPM [prune]
NPM [audit] [fix]
NPM [install-test]
NPM [it]
NPM [cit]
NPM [upd]
NPM [--prefix] [x] [install]" "npm run build; npm test; npm --version; npm exec x; npm link; npm dedupe; npm prune; npm audit fix; npm install-test; npm it; npm cit; npm upd; npm --prefix x install"
  check opt-out-install "NPM [install]"      "MATCHAPACK=0 npm install"
  check command-install "NPM [uninstall] [x]" "command npm uninstall x"
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
  # The commands that copy packages into node_modules (NOTES "Yarn 1
  # commands"); `workspace <name>` with add, remove, upgrade or
  # upgrade-interactive only.
  check yarn-add    "MATCHAPACK-YARN [add] [x] [--dev]"  "yarn add x --dev"
  check yarn-cmds   "MATCHAPACK-YARN [upgrade]
MATCHAPACK-YARN [upgrade] [x] [--latest]
MATCHAPACK-YARN [remove] [x]
MATCHAPACK-YARN [upgrade-interactive] [--latest]
MATCHAPACK-YARN [upgradeInteractive]
MATCHAPACK-YARN [workspace] [w] [add] [x]
MATCHAPACK-YARN [workspace] [w] [remove] [x]
MATCHAPACK-YARN [workspace] [w] [upgrade]
MATCHAPACK-YARN [workspace] [w] [upgrade-interactive] [--latest]
MATCHAPACK-YARN [workspace] [w] [upgradeInteractive]" "yarn upgrade; yarn upgrade x --latest; yarn remove x; yarn upgrade-interactive --latest; yarn upgradeInteractive; yarn workspace w add x; yarn workspace w remove x; yarn workspace w upgrade; yarn workspace w upgrade-interactive --latest; yarn workspace w upgradeInteractive"
  check yarn-not-routed "YARN [global] [add] [x]
YARN [workspace] [w] [run] [add]
YARN [workspace] [w] [install]
YARN [workspace] [w]
YARN [workspaces] [run] [add]
YARN [why] [add]" "yarn global add x; yarn workspace w run add; yarn workspace w install; yarn workspace w; yarn workspaces run add; yarn why add"
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

  # The binary removed after the functions were loaded (`matchapack
  # uninstall`, then `cargo uninstall`): npm and yarn go to plain npm and
  # Yarn, also where the shell hashed matchapack's path on an earlier call
  # (bash: `command -v` answers from that table).
  rb=/tmp/stub-removable; mkdir -p "$rb"; cp "$tc/matchapack" "$rb/"
  os=Linux
  check removed-binary "MATCHAPACK [ci]
NPM [ci]" "npm ci; rm -f $rb/matchapack; npm ci" "$rb:$np:$PATH"
  cp "$tc/matchapack" "$rb/"
  os=Darwin
  check removed-binary-yarn "MATCHAPACK-YARN [install]
YARN [install]" "yarn install; rm -f $rb/matchapack; yarn install" "$rb:$np:$PATH"
  cp "$tc/matchapack" "$rb/"

  # Your own shell function called matchapack (no binary on PATH): npm and
  # yarn route to it (only a path is checked with -x).
  plain_setup=$setup
  case $sh in
    fish) setup="function matchapack; $tc/matchapack \$argv; end; $plain_setup" ;;
    *) setup="function matchapack { $tc/matchapack \"\$@\"; }; $plain_setup" ;;
  esac
  os=Linux
  check own-function "MATCHAPACK [ci] [a b]" "npm ci 'a b'" "$np:$PATH"
  os=Darwin
  check own-function-yarn "MATCHAPACK-YARN [install]" "yarn install" "$np:$PATH"
  setup=$plain_setup

  # bash and zsh (fish has no set -e): a shell under `set -e` whose
  # matchapack is gone runs plain npm and yarn, it does not exit; and an
  # alias called matchapack defined after the integration is not routed to
  # (an alias is not expanded inside the functions): plain npm and yarn.
  if [ "$sh" != fish ]; then
    os=Linux
    check set-e "NPM [ci]
after" "set -e; rm -f $rb/matchapack; npm ci; echo after" "$rb:$np:$PATH"
    cp "$tc/matchapack" "$rb/"
    os=Darwin
    check set-e-yarn "YARN [install]
after" "set -e; rm -f $rb/matchapack; yarn install; echo after" "$rb:$np:$PATH"
    cp "$tc/matchapack" "$rb/"
    # A relative PATH entry: `command -v` answers `bin/matchapack`, which is
    # routed (an executable file).
    mkdir -p /tmp/relpath/bin; cp "$tc/matchapack" /tmp/relpath/bin/
    plain_setup=$setup
    setup="cd /tmp/relpath; $plain_setup"
    os=Darwin
    check relative-path "MATCHAPACK [ci]
MATCHAPACK-YARN [install]" "npm ci; yarn install" "bin:$np:$PATH"
    setup=$plain_setup
    # Two aliases: one whose text holds a path, one that does not (only a
    # path that `command -v` prints is checked with -x); bash expands
    # aliases with expand_aliases, as an interactive shell does.
    ea=; [ "$sh" = bash ] && ea='shopt -s expand_aliases; '
    check alias-later "NPM [ci]
YARN [install]
NPM [ci]
YARN [install]" "${ea}rm -f $rb/matchapack; alias matchapack=/nonexistent/matchapack; npm ci; yarn install; alias matchapack=nosuchcommand; npm ci; yarn install" "$rb:$np:$PATH"
    cp "$tc/matchapack" "$rb/"
  fi

  # `init --alias mp`: mp runs matchapack, with its arguments unchanged;
  # without --alias no such name is defined. bash expands aliases only in
  # interactive shells (a startup file's case) or with expand_aliases, and
  # only in commands parsed after the definition, as typed ones are: hence
  # the newline (bash parses a -c string line by line) and eval (zsh parses
  # the whole string first).
  plain_setup=$setup
  case $sh in
    fish) setup='type -q matchapack; and matchapack init fish --alias mp | source'; defined='type -q mp; and echo defined; or echo none' ;;
    zsh)  setup='command -v matchapack >/dev/null && eval "$(matchapack init zsh --alias mp)"'; defined='whence -w mp >/dev/null && echo defined || echo none' ;;
    bash) setup='shopt -s expand_aliases; command -v matchapack >/dev/null && eval "$(matchapack init bash --alias mp)"'; defined='type mp >/dev/null 2>&1 && echo defined || echo none' ;;
  esac
  nl='
'
  check alias-mac   "MATCHAPACK [ci] [a b] []
MATCHAPACK-YARN [install] [--frozen-lockfile]
MATCHAPACK [ci]" "${nl}eval 'mp npm ci \"a b\" \"\"'; eval 'mp yarn install --frozen-lockfile'; npm ci"
  check alias-status "MATCHAPACK [ci]
3" "${nl}eval 'STUB_RC=3 mp npm ci'; echo $st"
  os=Linux
  check alias-linux "MATCHAPACK [install] [x]
defined" "${nl}eval 'mp npm install x'; $defined"
  setup=$plain_setup
  check no-alias "none" "${nl}$defined"
  unset os
done

# The names `init --alias` refuses (src/main.rs SH_WORDS, ZSH_WORDS,
# FISH_WORDS) cover every builtin and reserved word these shells report
# that an alias name could be (letters, digits, `-`, `_`).
words() { awk "/^const $1: /,/^\];/" /src/src/main.rs | grep -o '"[^"]*"' | tr -d '"'; }
aliasable() { case $1 in '' | -*) return 1 ;; esac; [ -z "$(printf %s "$1" | tr -d 'A-Za-z0-9_-')" ]; }
sh_w=$(words SH_WORDS); zsh_w=$(words ZSH_WORDS); fish_w=$(words FISH_WORDS)
# in_list WORD LIST: LIST one word per line (no pipe: pipefail would count a
# writer cut short by grep -q as "not found").
in_list() { case "
$2
" in *"
$1
"*) return 0 ;; esac; return 1; }
missing=; set -f
for w in $(bash -c 'compgen -k; compgen -b'); do aliasable "$w" && ! in_list "$w" "$sh_w" && missing="$missing bash:$w"; done
for w in $(zsh -fc 'print -l ${(k)builtins} ${(k)reswords}'); do aliasable "$w" && ! in_list "$w" "$sh_w
$zsh_w" && missing="$missing zsh:$w"; done
for w in $(fish --no-config -c 'builtin -n'); do aliasable "$w" && ! in_list "$w" "$fish_w" && missing="$missing fish:$w"; done
set +f
n_sh=$(printf '%s\n' "$sh_w" | wc -l); n_fish=$(printf '%s\n' "$fish_w" | wc -l)
if [ -z "$missing" ] && [ "$n_sh" -ge 70 ] && [ "$n_fish" -ge 50 ]; then pass=$((pass+1)); echo "PASS alias-words: every builtin and reserved word of bash, zsh and fish is refused by init --alias"
else fail=$((fail+1)); echo "FAIL alias-words: not refused:$missing (lists read: $n_sh, $n_fish)"; fi
fi # stub cases

if [ -n "$real" ]; then
  echo "== fish with the real matchapack${host:+ (host, $MATCHAPACK_BIN)}"
  if [ -n "$host" ]; then
    mp=$(cd "$(dirname "$MATCHAPACK_BIN")" && pwd)/$(basename "$MATCHAPACK_BIN")
    [ -x "$mp" ] || { echo "setup failed: MATCHAPACK_BIN=$MATCHAPACK_BIN is not an executable" >&2; exit 2; }
    for s in fish python3 node npm curl; do command -v "$s" >/dev/null || { echo "setup failed: no $s" >&2; exit 2; }; done
  else
    arch=$(uname -m); case $arch in aarch64) narch=arm64;; x86_64) narch=x64;; esac
    curl -fsSL "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-$narch.tar.xz" | tar -xJ -C /opt
    export PATH=/opt/node-v24.22.1-linux-$narch/bin:$PATH
    mkdir -p /build && cp -r /src /build/src && rm -rf /build/src/target
    (cd /build/src && CARGO_TARGET_DIR=/build/target cargo build --release -q) || { echo "build failed" >&2; exit 2; }
    mp=/build/target/release/matchapack
  fi
  fish --version; echo "node $(node --version), npm $(npm --version), $("$mp" --version 2>&1 | head -1)"
  # The npm engine declines under any umask but 022 (its store assumes 022;
  # that gate is test-inject.sh's): a login shell with umask 002 would make
  # the install cases plain npm, so pin it.
  umask 022
  t=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/test-shells.XXXXXX")
  export npm_config_cache=$t/npm-cache YARN_CACHE_FOLDER=$t/yarn-cache
  npm install --prefix "$t/yarn1" --ignore-scripts --no-audit --no-fund --silent yarn@1.22.22 || { echo "setup failed: yarn" >&2; exit 2; }
  yjs=$t/yarn1/node_modules/yarn/bin/yarn.js
  # The fixtures' tarballs, from this run's own server on a free port; $FX
  # is the copy of fixtures/ whose URLs name it (fixture-server.sh).
  here=$src; . "$src/scripts/fixture-server.sh"; fixtures_serve "$t"
  trap '[ -n "$FX_PID" ] && kill "$FX_PID" 2>/dev/null' EXIT
  # PATH: the real binary, a `yarn` that records how it was started (every
  # argument; MATCHAPACK, which `matchapack yarn` sets to 0 when it hands
  # over to Yarn, unset otherwise) and then runs the real Yarn, node and npm.
  rb=$t/bin; calls=$t/yarn-calls; mkdir -p "$rb"; ln -s "$mp" "$rb/matchapack"
  printf '#!/bin/sh\n{ printf "MATCHAPACK=%%s" "${MATCHAPACK-unset}"; for a in "$@"; do printf " [%%s]" "$a"; done; echo; } >> "%s"\nexec node "%s" "$@"\n' "$calls" "$yjs" > "$rb/yarn"
  chmod +x "$rb/yarn"
  export MATCHAPACK_LOG_DIR=$t/log MATCHAPACK_STORE=$t/store MATCHAPACK_CONFIG_DIR=$t/config YARN_DISABLE_SELF_UPDATE_CHECK=true
  unset MATCHAPACK_ENGINE MATCHAPACK_PATHS MATCHAPACK MATCHAPACK_YARN_JS
  rsetup='type -q matchapack; and matchapack init fish | source'
  # rproj <name> npm|yarn [postinstall exit code]: a fresh project.
  rproj() {
    local d=$t/rp/$1; rm -rf "${d:?}"; mkdir -p "$d"
    if [ "$2" = npm ]; then cp "$FX/optional-upward/package.json" "$FX/optional-upward/package-lock.json" "$d/"
    else cp "$FX/yarn1-small/package.json" "$FX/yarn1-small/yarn.lock" "$d/"; fi
    [ -n "${3-}" ] && python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"exit "+sys.argv[2]}; json.dump(d, open(p,"w"))' "$d/package.json" "$3"
    echo "$d"
  }
  # rfish <dir> <fish commands> [yarn]: output in $out; yarn: also source the
  # macOS yarn function by hand.
  rfish() {
    local s=$rsetup; [ "${3-}" = yarn ] && s="$s; source '$src/shell/matchapack-yarn.fish'"
    : > "$calls"
    out=$(cd "$1" && PATH="$rb:$PATH" fish --no-config -c "$s; $2" 2>&1)
  }
  logn() { { wc -l < "$MATCHAPACK_LOG_DIR/log.jsonl"; } 2>/dev/null || echo 0; }
  lastrec() { tail -1 "$MATCHAPACK_LOG_DIR/log.jsonl" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }
  rcheck() { # name condition-result detail
    if [ "$2" = 0 ]; then pass=$((pass+1)); echo "PASS fish real $1"
    else fail=$((fail+1)); echo "FAIL fish real $1: $3"; printf '%s\n' "$out" | tail -6 | sed 's/^/    /'; fi
  }
  rfish "$t" 'type -t npm; functions -q yarn; and echo yarn-defined; or echo no-yarn-function'
  [ "$out" = "function
no-yarn-function" ]; rcheck "init: npm is a function, no yarn function on Linux" $? "[$out]"

  # init --alias with the real binary: mp is matchapack; a builtin's name is
  # refused (status 2) and the npm function is still defined.
  out=$(PATH="$rb:$PATH" fish --no-config -c 'matchapack init fish --alias mp | source; mp --version; type -t npm' 2>&1)
  printf '%s\n' "$out" | head -1 | grep -q '^matchapack ' && [ "$(printf '%s\n' "$out" | tail -1)" = function ]
  rcheck "init --alias mp: mp runs the real matchapack" $? "[$out]"
  out=$(PATH="$rb:$PATH" fish --no-config -c 'matchapack init fish --alias string | source; echo "pipestatus $pipestatus"; type -t npm' 2>&1)
  printf '%s\n' "$out" | grep -q 'builtin or reserved word of fish' && printf '%s\n' "$out" | grep -q '^pipestatus 2 0$' && [ "$(printf '%s\n' "$out" | tail -1)" = function ]
  rcheck "init --alias string: refused (status 2), the npm function still defined" $? "[$out]"

  d=$(rproj ci npm); n0=$(logn)
  rfish "$d" 'npm ci --no-audit --no-fund; echo "status $status"'
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(logn)" = $((n0+1)) ] && [ "$(lastrec 'r["outcome"], r["args"]')" = "('installed', ['ci', '--no-audit', '--no-fund'])" ] \
    && printf '%s\n' "$out" | grep -q '^matchapack: npm ci with clones' && [ -d "$d/node_modules/lc-r" ]
  rcheck "npm ci → matchapack npm (logged, arguments unchanged), installed, status 0" $? "log $n0 → $(logn)"

  # The install family: `npm i` after the npm ci above goes through
  # matchapack (logged as npm's install), and with nothing to place it
  # prints nothing of matchapack's.
  n0=$(logn)
  rfish "$d" 'npm i --no-audit --no-fund; echo "status $status"'
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(logn)" = $((n0+1)) ] && [ "$(lastrec 'r["outcome"], r["command"], r["args"]')" = "('installed', 'install', ['i', '--no-audit', '--no-fund'])" ] \
    && ! printf '%s\n' "$out" | grep -q '^matchapack'
  rcheck "npm i → matchapack npm (logged as install), silent with nothing to place, status 0" $? "log $n0 → $(logn)"

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
  # (yc: the folder; ycf: the same in fish's double quotes)
  d=$(rproj yargs yarn); yc="$t/c d/it's \"q\""; ycf="$t/c d/it's \\\"q\\\""
  rfish "$d" "yarn install --frozen-lockfile --cache-folder \"$ycf\" --non-interactive; echo \"status \$status\"" yarn
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(cat "$calls")" = "MATCHAPACK=0 [install] [--frozen-lockfile] [--cache-folder] [$yc] [--non-interactive]" ] \
    && [ -d "$yc/v6" ] && [ -f "$d/node_modules/.yarn-integrity" ]
  rcheck "yarn install with spaced and quoted arguments → matchapack yarn → Yarn, arguments unchanged, installed" $? "calls [$(cat "$calls")]"

  d=$(rproj ybare yarn); rfish "$d" 'yarn; echo "status $status"' yarn
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(cat "$calls")" = "MATCHAPACK=0" ] && [ -f "$d/node_modules/.yarn-integrity" ]
  rcheck "bare yarn → matchapack yarn → Yarn, installed" $? "calls [$(cat "$calls")]"

  d=$(rproj yflag yarn); rfish "$d" 'yarn --frozen-lockfile --non-interactive; echo "status $status"' yarn
  [ "$(printf '%s\n' "$out" | tail -1)" = "status 0" ] && [ "$(cat "$calls")" = "MATCHAPACK=0 [--frozen-lockfile] [--non-interactive]" ]
  rcheck "yarn --frozen-lockfile → matchapack yarn → Yarn" $? "calls [$(cat "$calls")]"

  rfish "$t" 'yarn --version; echo "status $status"' yarn
  [ "$out" = "1.22.22
status 0" ] && [ "$(cat "$calls")" = "MATCHAPACK=unset [--version]" ]
  rcheck "yarn --version → Yarn directly, matchapack never started" $? "calls [$(cat "$calls")]"

  d=$(rproj yfailplain yarn 7); rfish "$d" 'command yarn install --frozen-lockfile; echo "status $status"' yarn; want=$(printf '%s\n' "$out" | tail -1)
  d=$(rproj yfail yarn 7); rfish "$d" 'yarn install --frozen-lockfile; echo "status $status"' yarn
  [ "$want" != "status 0" ] && [ "$(printf '%s\n' "$out" | tail -1)" = "$want" ] && [ "$(cat "$calls")" = "MATCHAPACK=0 [install] [--frozen-lockfile]" ]
  rcheck "a failing yarn install: status ($want) passes through the function and matchapack, as plain Yarn's" $? "plain $want, got $(printf '%s\n' "$out" | tail -1), calls [$(cat "$calls")]"
fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
