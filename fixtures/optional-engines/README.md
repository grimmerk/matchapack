# optional-engines fixture

An optional dependency whose `engines` cannot be met: `lc-eng` asks for
`node >=99`. npm ci leaves it out, together with `lc-q`, which only `lc-eng`
needs (arborist `optional-set.js`); `lc-s` stays because `lc-r` also needs it.
Expected node_modules: `lc-r`, `lc-s`.

npm 11 drops such packages while it builds the ideal tree (marks them inert).
npm 10.9 decides only when it extracts them, which npm's dry run never
reaches: an earlier matchapack engine, which took its tree from npm's dry run,
installed `lc-eng` and `lc-q` here on npm 10.9.2. The inject engine leaves
the extraction to npm, so the tree is npm's own on both.

The tarballs live with the optional-upward fixture (`lc-eng`'s source is in
`../optional-upward/pkgs/lc-eng`). The suites serve them on a port of their
own (`scripts/fixture-server.sh`); to run the harness by hand, serve them
on the placeholder 127.0.0.1:48321:

    (cd ../optional-upward/tarballs && python3 -m http.server 48321 --bind 127.0.0.1)
