// `matchapack store verify`'s node side (src/verify.rs; NOTES "PR C1"):
// extracts store entries' tarballs with npm's own pacote, the way the inject
// engine's hook has them extracted (src/inject.js), so that the store is
// judged by what `npm ci` would put on disk.
//
// argv: <npm install dir> <npm content cache (the _cacache directory)>
// stdin:  one job per line, {"id", "integrity", "dir"}: extract the tarball
//         with that integrity from the cache into `dir` (an empty directory
//         matchapack made)
// stdout: first {"hello": true, "pacote", "tar", "key", "tar_fingerprint"}
//         (the versions, and the store key of the extraction code node
//         loaded, as the inject hook computes it: src/inject.js
//         `extractionCode`; a null key when it cannot be named),
//         then one line per job, {"id", "ok"} or {"id", "ok": false, "error"},
//         in the order the extractions finish. Jobs run as they arrive;
//         matchapack bounds how many are outstanding.
// Exit status: 0 when stdin ends and every job is answered; 90 when pacote
// cannot be loaded or has an unexpected shape (nothing was extracted).
//
// The tarball is read from npm's cache only, by its integrity (`offline`:
// nothing is fetched). matchapack checks beforehand that the cache holds
// those bytes, so pacote's own corruption handling (which removes the cache
// entry) is not reached.
'use strict'
const path = require('path')
const { ineligible, pacotePaths, fingerprintFile, extractionCode } = require('./inject.js')

function fail (message) {
  process.stderr.write(`matchapack verify helper: ${message}\n`)
  process.exit(90)
}

function main () {
  const [npmDir, cache] = process.argv.slice(2)
  if (!npmDir || !cache) {
    fail('usage: node store-verify.js <npm dir> <npm _cacache dir>')
  }
  let pacote, pacoteVersion, tarVersion, code
  try {
    // The same pacote the hook hashes and hooks (arborist's).
    const paths = pacotePaths(npmDir)
    const fp = fingerprintFile(paths.fetcher)
    const main = paths.main
    pacote = require(main)
    code = extractionCode(paths, fp)
    const pacoteDir = path.dirname(path.dirname(main))
    pacoteVersion = require(path.join(pacoteDir, 'package.json')).version
    tarVersion = require(require.resolve('tar/package.json', { paths: [pacoteDir] })).version
  } catch (e) {
    fail(`cannot load npm's pacote: ${e.message}`)
  }
  if (typeof pacote.RemoteFetcher !== 'function' || typeof pacote.RemoteFetcher.prototype.extract !== 'function') {
    fail('this npm\'s pacote has no RemoteFetcher with an extract method')
  }
  // Read once, before any extraction: process.umask() reads by setting it
  // to 0 and back, and a file tar creates on another thread meanwhile would
  // get modes 666/777 (seen on a store of 8,779 entries).
  const processUmask = process.umask()
  const out = (o) => process.stdout.write(JSON.stringify(o) + '\n')
  out({ hello: true, pacote: pacoteVersion, tar: tarVersion, key: code.key, tar_fingerprint: code.tar_fingerprint })

  const run = async ({ id, integrity, dir }) => {
    try {
      // A registry tarball with a `resolved` URL, as npm ci's lockfile entries
      // give it to pacote: the fetcher class the hook clones for. The URL is
      // never fetched (offline, and the tarball is taken from the cache by
      // its integrity first).
      const f = new pacote.RemoteFetcher('https://matchapack.invalid/store-verify.tgz', { integrity, cache, offline: true })
      // The same options the hook requires before it clones (npm's
      // defaults, umask 022): what is compared is what the hook places.
      const why = ineligible(pacote, f, processUmask)
      if (why) {
        throw new Error(`not extracted as npm ci would (${why})`)
      }
      await f.extract(dir)
      out({ id, ok: true })
    } catch (e) {
      out({ id, ok: false, error: String((e && e.message) || e).split('\n')[0] })
    }
  }
  let buf = ''
  process.stdin.setEncoding('utf8')
  process.stdin.on('data', (chunk) => {
    buf += chunk
    let i
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i)
      buf = buf.slice(i + 1)
      let job
      try {
        job = JSON.parse(line)
      } catch {
        continue
      }
      run(job)
    }
  })
}

main()
