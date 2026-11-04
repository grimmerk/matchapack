# optional-engines fixture

An optional dependency whose `engines` cannot be met: `lc-eng` asks for
`node >=99`. npm ci leaves it out, together with `lc-q`, which only `lc-eng`
needs (arborist `optional-set.js`); `lc-s` stays because `lc-r` also needs it.
Expected node_modules: `lc-r`, `lc-s`.

npm 11 drops such packages while it builds the ideal tree; npm 10.9 only when
it extracts them, which a dry run never reaches. thinclone's ideal-tree helper
repeats npm's check, so both match (found by review of the R9 change: before
the check, thinclone installed `lc-eng` and `lc-q` on npm 10.9.2).

The tarballs live with the optional-upward fixture (`lc-eng`'s source is in
`../optional-upward/pkgs/lc-eng`). Serve them before running the harness:

    (cd ../optional-upward/tarballs && python3 -m http.server 48321 --bind 127.0.0.1)
