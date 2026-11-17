# optional-engines fixture

An optional dependency whose `engines` cannot be met: `lc-eng` asks for
`node >=99`. npm ci leaves it out, together with `lc-q`, which only `lc-eng`
needs (arborist `optional-set.js`); `lc-s` stays because `lc-r` also needs it.
Expected node_modules: `lc-r`, `lc-s`.

npm 11 drops such packages while it builds the ideal tree (marks them inert),
so the tree matchapack takes from npm's dry run already lacks them. npm 10.9
decides only when it extracts them, which a dry run never reaches: on npm 10.9.2
matchapack installed `lc-eng` and `lc-q` here, one reason npm 10 is not in the
version gate (NOTES, "npm 10").

The tarballs live with the optional-upward fixture (`lc-eng`'s source is in
`../optional-upward/pkgs/lc-eng`). The suites serve them on a port of their
own (`scripts/fixture-server.sh`); to run the harness by hand, serve them
on the placeholder 127.0.0.1:48321:

    (cd ../optional-upward/tarballs && python3 -m http.server 48321 --bind 127.0.0.1)
