# optional-upward fixture

Exercises npm's optional-set removal in both directions (arborist
`optional-set.js`): `lc-x-aix` cannot install here (`os: ["aix"]`); `lc-p`
requires it through a NON-optional edge, so `lc-p` goes too (upward); `lc-q`
is needed only by `lc-p`, so it goes (downward); `lc-s` stays because `lc-r`
also needs it. Expected node_modules: `lc-r`, `lc-s`.

Tarballs are committed so the lockfile integrities stay valid. Serve them
before running the harness:

    (cd tarballs && python3 -m http.server 48321 --bind 127.0.0.1)
