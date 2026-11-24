# Sourced by the suites (needs $here, the checkout): the fixtures' tarball
# server for this run.
#   fixtures_serve <dir>
# sets FX (the fixtures to use: a copy under <dir> whose tarball URLs name
# this run's port), FX_URL (http://127.0.0.1:<port>), FX_PORT and FX_PID
# (the server's pid, empty when a parent suite started it), and exports
# MATCHAPACK_TEST_FX and MATCHAPACK_TEST_FX_URL for the suites and helpers
# this suite runs (test-yarn1.sh -> verify-yarn1.sh, test-inject-hook.js).
#
# The fixtures in the checkout keep 127.0.0.1:48321 as a placeholder; no
# suite serves or uses that port. Each run binds a free port, so several
# worktrees run their suites at the same time (no lock needed; a suite still
# wrapped in `lockf` runs the same). The server stops when the suite that
# started it exits (scripts/fixture-server.py).
#
# A suite never uses a server that does not serve its own copy: the server's
# GET /.matchapack-server-id must answer the token in $FX/.matchapack-server-id,
# and the copy must have been made from this checkout's fixtures/
# ($FX/.matchapack-source), or the suite stops with status 2. Safe under
# `set -e` (linux-test.sh): every check that may fail is tested, not run bare.
fixtures_serve() {
  local dir=${1:?usage: fixtures_serve <dir>} ready got want fxsrc mine
  if [ -n "${MATCHAPACK_TEST_FX-}" ]; then
    # A parent suite's server (this suite runs inside that run).
    FX=$MATCHAPACK_TEST_FX; FX_URL=${MATCHAPACK_TEST_FX_URL:?MATCHAPACK_TEST_FX is set without MATCHAPACK_TEST_FX_URL}; FX_PID=
  else
    mkdir -p "$dir"; dir=$(cd "$dir" && pwd -P); FX=$dir/fixtures; ready=$dir/fixture-server.port; rm -f "$ready"
    python3 "${here:?}/scripts/fixture-server.py" "$here/fixtures" "$FX" "$ready" "$$" > "$dir/fixture-server.log" 2>&1 &
    FX_PID=$!
    for _ in $(seq 300); do
      [ -s "$ready" ] && break
      kill -0 "$FX_PID" 2>/dev/null || break
      sleep 0.1
    done
    [ -s "$ready" ] || { echo "the fixtures' tarball server did not start: $(tail -3 "$dir/fixture-server.log")" >&2; exit 2; }
    FX_URL=http://127.0.0.1:$(cat "$ready")
  fi
  FX_PORT=${FX_URL##*:}
  want=$(cat "$FX/.matchapack-server-id" 2>/dev/null) || want=
  got=$(curl -sf --max-time 5 "$FX_URL/.matchapack-server-id") || got=
  fxsrc=$(cat "$FX/.matchapack-source" 2>/dev/null) || fxsrc=
  mine=$(cd "${here:?}/fixtures" && pwd -P) || mine=
  if [ -z "$mine" ] || [ "$fxsrc" != "$mine" ]; then
    echo "$FX is a copy of ${fxsrc:-unknown fixtures}, not of this checkout's $mine: refusing to use it" >&2
    [ -z "$FX_PID" ] && echo "(MATCHAPACK_TEST_FX came from the environment: unset MATCHAPACK_TEST_FX and MATCHAPACK_TEST_FX_URL to start a server of this run)" >&2
    exit 2
  fi
  if [ -z "$want" ] || [ "$got" != "$want" ]; then
    echo "the server on $FX_URL does not serve this run's fixtures ($FX): refusing to use it" >&2
    [ -z "$FX_PID" ] && echo "(MATCHAPACK_TEST_FX came from the environment: unset MATCHAPACK_TEST_FX and MATCHAPACK_TEST_FX_URL to start a server of this run)" >&2
    exit 2
  fi
  export MATCHAPACK_TEST_FX=$FX MATCHAPACK_TEST_FX_URL=$FX_URL
}
