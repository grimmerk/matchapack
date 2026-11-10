// Unit checks of the inject hook's eligibility rules (src/inject.js
// `ineligible`), against the fetcher objects of the npm given: the real
// pacote classes are constructed, so a renamed field or a changed default in
// a new pacote fails here instead of silently turning every package into a
// fallback (or worse, into a clone npm would have extracted differently).
//   node scripts/test-inject-hook.js <npm install dir>
// Prints one PASS/FAIL line per case; exit 1 when any failed.
'use strict'
const path = require('path')
const npmDir = process.argv[2]
const { ineligible } = require(path.join(__dirname, '..', 'src', 'inject.js'))
const pacote = require(require.resolve('pacote', {
  paths: [path.join(npmDir, 'node_modules', '@npmcli', 'arborist')],
}))

const sri = 'sha512-' + Buffer.alloc(64, 7).toString('base64')
const url = 'https://registry.npmjs.org/x/-/x-1.0.0.tgz'
const base = { cache: '/nonexistent/_cacache', integrity: sri }
const remote = (opts = {}) => new pacote.RemoteFetcher(`x@${url}`, { ...base, ...opts })
const registry = (opts = {}) => new pacote.RegistryFetcher('x@1.0.0', { ...base, ...opts })

let failed = 0
const check = (name, got, want) => {
  const ok = got === want
  if (!ok) {
    failed++
  }
  console.log(`${ok ? 'PASS' : 'FAIL'} hook: ${name}${ok ? '' : ` (got ${JSON.stringify(got)}, want ${JSON.stringify(want)})`}`)
}

// What npm ci asks for a lockfile entry with `resolved` and `integrity`.
check('a registry tarball by URL with an integrity is eligible', ineligible(pacote, remote(), 0o022), null)
check('a registry spec with `resolved` is eligible', ineligible(pacote, registry({ resolved: url }), 0o022), null)
// The hook reads these fields; the real fetcher must carry them.
const f = remote()
check('fetcher fields exist with pacote\'s defaults',
  [typeof f.integrity, f.resolved, f.fmode, f.dmode, f.umask, f.allowGitIgnore, f.cache].join(' '),
  ['object', url, 0o666, 0o777, 0, false, '/nonexistent/_cacache'].join(' '))
check('the integrity is sent as the SRI string', String(f.integrity), sri)

check('a registry spec without `resolved` (manifest fetch, bin modes) is npm\'s',
  ineligible(pacote, registry(), 0o022), 'registry-no-resolved')
check('no integrity is npm\'s', ineligible(pacote, remote({ integrity: undefined }), 0o022), 'no-integrity')
check('a file: tarball is npm\'s',
  ineligible(pacote, new pacote.FileFetcher('x@file:/nonexistent/x-1.0.0.tgz', base), 0o022), 'type-FileFetcher')
check('a git dependency is npm\'s',
  ineligible(pacote, new pacote.GitFetcher('x@github:a/b#0123456789abcdef0123456789abcdef01234567', base), 0o022), 'type-GitFetcher')
check('something that is not a fetcher is npm\'s', ineligible(pacote, { integrity: sri, type: 'Fake' }, 0o022), 'type-Fake')
check('npm config umask is npm\'s', ineligible(pacote, remote({ umask: 0o002 }), 0o022), 'modes')
check('a file mode option is npm\'s', ineligible(pacote, remote({ fmode: 0o644 }), 0o022), 'modes')
check('a directory mode option is npm\'s', ineligible(pacote, remote({ dmode: 0o755 }), 0o022), 'modes')
check('process umask 002 is npm\'s', ineligible(pacote, remote(), 0o002), 'process-umask')
check('process umask 077 is npm\'s', ineligible(pacote, remote(), 0o077), 'process-umask')
check('allowGitIgnore is npm\'s', ineligible(pacote, remote({ allowGitIgnore: true }), 0o022), 'allowGitIgnore')

process.exit(failed ? 1 : 0)
