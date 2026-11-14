// Unit checks of the inject hook's eligibility rules (src/inject.js
// `ineligible`), its result trap (`resultTrap`) and its fingerprint, against
// the fetcher objects of the npm given: the real
// pacote classes are constructed, so a renamed field or a changed default in
// a new pacote fails here instead of silently turning every package into a
// fallback (or worse, into a clone npm would have extracted differently).
//   node scripts/test-inject-hook.js <npm install dir>
// Prints one PASS/FAIL line per case; exit 1 when any failed.
'use strict'
const path = require('path')
const npmDir = process.argv[2]
const util = require('util')
const { execFileSync } = require('child_process')
const injectJs = path.join(__dirname, '..', 'src', 'inject.js')
const { ineligible, resultTrap, fingerprint, fetcherBase } = require(injectJs)
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
  [typeof f.integrity, f.resolved, f.fmode, f.dmode, f.umask, f.allowGitIgnore, f.cache, f.package].join(' '),
  ['object', url, 0o666, 0o777, 0, false, '/nonexistent/_cacache', null].join(' '))
check('a new fetcher has no manifest loaded (`package` is null)', f.package, null)
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
// A fetcher whose manifest is loaded: pacote marks the declared bins
// executable while extracting (#entryMode), the store entry does not.
const withManifest = (spec) => {
  const x = spec
  x.package = { name: 'x', version: '1.0.0', bin: { x: 'cli.js' } }
  return x
}
check('a remote fetcher with its manifest loaded is npm\'s', ineligible(pacote, withManifest(remote()), 0o022), 'manifest-loaded')
check('a registry fetcher with `resolved` and its manifest loaded is npm\'s',
  ineligible(pacote, withManifest(registry({ resolved: url })), 0o022), 'manifest-loaded')
check('a manifest without bins is npm\'s too (any loaded manifest)',
  ineligible(pacote, Object.assign(remote(), { package: { name: 'x', version: '1.0.0' } }), 0o022), 'manifest-loaded')

// The trap on the hook's result: it changes nothing a caller sees, and it
// reports reads of keys other than resolved / integrity / from.
const plain = () => ({ resolved: url, integrity: sri, from: 'x@1.0.0' })
const trapped = () => {
  const reads = []
  return { reads, value: resultTrap(plain(), (key, expected) => reads.push(`${key}:${expected}`)) }
}
const quiet = async (name, use) => {
  const t = trapped()
  const got = await use(t.value)
  const want = await use(plain())
  check(`trap: ${name} gives what the plain object gives, and reads nothing unexpected`,
    JSON.stringify([got, t.reads.filter(r => r.endsWith(':false'))]), JSON.stringify([want, []]))
}
;(async () => {
  await quiet('await of an async function\'s result', async (v) => JSON.stringify(await (async () => v)()))
  await quiet('Promise.resolve', async (v) => JSON.stringify(await Promise.resolve(v)))
  await quiet('util.inspect', (v) => util.inspect(v))
  await quiet('util.format %j', (v) => util.format('%j', v))
  {
    // %o (and inspect's showProxy) print the object as a Proxy: a difference
    // in a debug line only, and nothing is read through the trap.
    const t = trapped()
    check('trap: util.format %o reads nothing through the trap', [util.format('%o', t.value).startsWith('Proxy ['), t.reads.length].join(), 'true,0')
  }
  await quiet('JSON.stringify', (v) => JSON.stringify(v))
  await quiet('object spread', (v) => JSON.stringify({ ...v }))
  await quiet('string conversion', (v) => `${v} ${String(v)}`)
  await quiet('Object.keys and hasOwnProperty', (v) => Object.keys(v).join() + Object.prototype.hasOwnProperty.call(v, 'files'))
  await quiet('instanceof, constructor', (v) => `${v instanceof Object} ${v.constructor === Object}`)
  await quiet('assert.deepStrictEqual', (v) => { require('assert').deepStrictEqual(v, plain()); return 'equal' })
  {
    const t = trapped()
    const r = await (async () => t.value)()
    check('trap: awaiting the result resolves to the trapped object itself', r === t.value, true)
  }
  {
    const t = trapped()
    void t.value.resolved; void t.value.integrity; void t.value.from
    check('trap: reads of resolved, integrity and from are noted as expected', t.reads.join(), 'resolved:true,integrity:true,from:true')
  }
  {
    const t = trapped()
    const got = [t.value.files, 'unpacked' in t.value]
    check('trap: reads of any other key are reported (get and `in`), and give what the plain object gives',
      JSON.stringify([got, t.reads]), JSON.stringify([[undefined, false], ['files:false', 'unpacked:false']]))
  }
  {
    const t = trapped()
    const v = resultTrap({ a: 1 }, () => { throw new Error('x') })
    check('trap: a failing report does not fail the read', v.b === undefined && t.reads.length === 0, true)
  }

  // The fingerprint: the hook's own function and `--fingerprint` agree (CI
  // and run time hash the same thing), and it is the class text's SHA-256.
  const fp = fingerprint(fetcherBase(pacote))
  const printed = execFileSync(process.execPath, [injectJs, '--fingerprint', npmDir], { encoding: 'utf8' }).trim()
  check('fingerprint: --fingerprint prints what the hook computes', printed, fp)
  check('fingerprint: SHA-256 of FetcherBase\'s class text', fp,
    require('crypto').createHash('sha256').update(Function.prototype.toString.call(Object.getPrototypeOf(pacote.RemoteFetcher))).digest('hex'))

  process.exit(failed ? 1 : 0)
})()

