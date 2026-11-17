# optional-upward fixture

Exercises npm's optional-set removal in both directions (arborist
`optional-set.js`): `lc-x-aix` cannot install here (`os: ["aix"]`); `lc-p`
requires it through a NON-optional edge, so `lc-p` goes too (upward); `lc-q`
is needed only by `lc-p`, so it goes (downward); `lc-s` stays because `lc-r`
also needs it. Expected node_modules: `lc-r`, `lc-s`.

Tarballs are committed so the lockfile integrities stay valid. The URLs name
the placeholder 127.0.0.1:48321: the suites serve a copy on a free port of
their own, with the URLs rewritten (`scripts/fixture-server.sh`). To run the
harness by hand, serve them on the placeholder:

    (cd tarballs && python3 -m http.server 48321 --bind 127.0.0.1)
