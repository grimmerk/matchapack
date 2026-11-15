// Does npm trust a project's hidden lockfile (node_modules/.package-lock.json)?
//
//   node scripts/hidden-lockfile-trusted.js <npm install dir> <project dir>...
//
// npm reads the installed tree from the hidden lockfile only when no package
// directory under node_modules is newer than that file and every entry is
// there (arborist lib/shrinkwrap.js, assertNoNewer); otherwise it reads every
// package.json from disk: the same result, more slowly. An install that left
// a directory newer than the hidden lockfile would make every later
// `npm install` / `npm ls` take the slow path, so scripts/verify-steps.sh
// checks that npm trusts it after an engine install exactly when it trusts it
// after plain npm. Uses the given npm's own arborist.
//
// Prints one line per project: `trusted` or `not-trusted` (and npm's reason),
// then the path. Exit status 0 whatever the answers; 2 on an error.
'use strict'
const path = require('path')
const [npmDir, ...dirs] = process.argv.slice(2)
if (!npmDir || dirs.length === 0) {
  console.error('usage: hidden-lockfile-trusted.js <npm install dir> <project dir>...')
  process.exit(2)
}
const Shrinkwrap = require(path.join(npmDir, 'node_modules', '@npmcli', 'arborist', 'lib', 'shrinkwrap.js'))
;(async () => {
  for (const dir of dirs) {
    const meta = await Shrinkwrap.load({ path: path.resolve(dir), hiddenLockfile: true })
    const why = meta.loadingError ? ` (${String(meta.loadingError.message || meta.loadingError).split('\n')[0]})` : ''
    console.log(`${meta.loadedFromDisk ? 'trusted' : 'not-trusted'}${why}\t${dir}`)
  }
})().catch((e) => {
  console.error(e && e.stack ? e.stack : e)
  process.exit(2)
})
