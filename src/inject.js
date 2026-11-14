// The inject engine's node side (MATCHAPACK_ENGINE=inject; NOTES "Inject
// engine"). Runs npm's REAL command in this process, the way bin/npm-cli.js
// does, with one method replaced: pacote's FetcherBase.prototype.extract, the
// single place where npm puts a package's files into a directory. It is
// reached after pacote's policy checks (allow-git / allow-remote / allow-file
// / allow-directory / allow-registry run in FetcherBase.get, before a fetcher
// exists), so npm's refusals stay npm's.
//
// For a package the hook can reproduce exactly (see `ineligible`), it asks
// the matchapack process that started this one to place the package's store
// entry at the destination (clone, then set every placed path's time to now)
// and resolves when that is done. When the package is not in the store yet,
// or is picked to be checked against pacote, matchapack answers with a
// directory instead: the hook runs pacote's own extract into it and asks
// again, and matchapack makes that extraction the store entry, or compares
// the entry with it (src/serve.rs). For anything else, and whenever matchapack
// answers with an error, it calls pacote's own extract, unchanged. So the
// worst case is plain `npm ci`: slower, never different.
//
// argv: <npm install dir> <npm args...>   (e.g. ci --no-audit)
// env:  MATCHAPACK_INJECT_FD    the socket to matchapack (protocol: src/serve.rs)
//       MATCHAPACK_INJECT_FINGERPRINTS  `any`, or the comma list of pacote
//                              fingerprints the hook may be installed on
//                              (`fingerprint` below; matchapack passes its
//                              list, src/npm-fingerprints.json, and the
//                              user's MATCHAPACK_NPM_FINGERPRINT)
//       MATCHAPACK_INJECT_HOOK  `off`: install the hook but let pacote extract
//                              every package (tests, and to rule the hook out);
//                              `none`: do not install it at all (tests: what
//                              an npm that no longer calls the method looks
//                              like to matchapack)
// Exit status: npm's. 90 before npm starts: the hook could not be installed
// (matchapack then runs plain npm), also on an unexpected error up to there.
//
// `node inject.js --fingerprint <npm install dir>` prints the fingerprint of
// that npm's pacote and exits: the one way it is computed, for the list and
// for CI.
'use strict'
const path = require('path')
const fs = require('fs')
const crypto = require('crypto')

// The pacote that npm's arborist loads: one copy, in npm's node_modules.
function loadPacote (npmDir) {
  const from = path.join(npmDir, 'node_modules', '@npmcli', 'arborist')
  return require(require.resolve('pacote', { paths: [from] }))
}

// FetcherBase, when this pacote has the shape the hook needs: one
// prototype.extract that the registry and remote fetchers share. Every
// fetcher class extends FetcherBase; it is reached through pacote's exports
// only. null otherwise.
function fetcherBase (pacote) {
  const FetcherBase = pacote.RemoteFetcher && Object.getPrototypeOf(pacote.RemoteFetcher)
  if (typeof FetcherBase?.prototype?.extract !== 'function' ||
      typeof pacote.RegistryFetcher !== 'function' ||
      pacote.RemoteFetcher.prototype.extract !== FetcherBase.prototype.extract ||
      pacote.RegistryFetcher.prototype.extract !== FetcherBase.prototype.extract) {
    return null
  }
  return FetcherBase
}

// What the hook replaces is the work of FetcherBase: extract() and the
// private methods it calls (#extract, #tarxOptions, #entryMode, #mkdir,
// #empty), which no code outside the class can reach. The whole class text
// holds them all, comments included, as it is in the file: its SHA-256 is
// the fingerprint. The hook is installed only on a fingerprint the harness
// has run on (src/npm-fingerprints.json; NOTES "G-lite: npm release safety").
function fingerprint (FetcherBase) {
  return crypto.createHash('sha256').update(Function.prototype.toString.call(FetcherBase)).digest('hex')
}

// Why this fetcher must use pacote's own extract, or null when a clone gives
// the same result. The store holds what pacote extracts from a registry
// tarball with its default options under umask 022 (store.rs `extract`):
// anything else is npm's to extract.
function ineligible (pacote, f, processUmask) {
  // Registry tarballs arrive as name@URL (RemoteFetcher) when the lockfile
  // has `resolved`, as name@version (RegistryFetcher) otherwise. git, file:
  // tarballs and directories are other classes.
  if (!(f instanceof pacote.RemoteFetcher || f instanceof pacote.RegistryFetcher)) {
    return 'type-' + String(f && f.type)
  }
  // The store is keyed by integrity; without one there is nothing to look up
  // (and pacote would have nothing to verify the tarball against).
  if (!f.integrity) {
    return 'no-integrity'
  }
  // Without `resolved` a RegistryFetcher fetches the manifest first, and
  // pacote then marks the declared bins executable while extracting; with it
  // the tarball comes straight from the cache and no file mode is added.
  if (f instanceof pacote.RegistryFetcher && !f.resolved) {
    return 'registry-no-resolved'
  }
  // Any other fetcher whose manifest is loaded: pacote marks its declared
  // bins executable while extracting (#entryMode), which the store entry,
  // shared by every fetcher of that tarball, does not hold. pacote sets
  // `package` to null in the constructor; anything else is npm's.
  if (f.package !== null) {
    return 'manifest-loaded'
  }
  // pacote's defaults (npm passes none of these, and its `umask` config
  // defaults to 0): mode = ((mode | fmode) & ~umask) | 0o600.
  if (f.fmode !== 0o666 || f.dmode !== 0o777 || f.umask !== 0) {
    return 'modes'
  }
  // tar creates files through the process umask.
  if (processUmask !== 0o022) {
    return 'process-umask'
  }
  // The store renames .gitignore to .npmignore, as pacote does by default.
  if (f.allowGitIgnore) {
    return 'allowGitIgnore'
  }
  return null
}

// The keys of the value the hook resolves to (instead of pacote's
// extract's) that npm may read without it mattering: pacote's own value is a
// plain object with these three. Reading any other key means this npm uses
// something of extract's result that the hook may not give as pacote would.
const RESULT_KEYS = new Set(['resolved', 'integrity', 'from'])

// The value as a Proxy that reports each read of a key (`get`, `in`) to
// report(key, expected). Not reported, because a plain object answers them
// the same way whoever made it: `then` (promise resolution reads it),
// `toJSON` (JSON.stringify), Symbol keys (inspection, string conversion),
// and the names Object.prototype has (`constructor`, `toString`, …). The
// Proxy returns what the object holds, so it changes no result.
function resultTrap (value, report) {
  const watch = (key) => {
    if (typeof key === 'symbol' || key === 'then' || key === 'toJSON' || key in Object.prototype) {
      return
    }
    try {
      report(key, RESULT_KEYS.has(key))
    } catch {
      // the report is not worth a failed install
    }
  }
  return new Proxy(value, {
    get (target, key, receiver) {
      watch(key)
      return Reflect.get(target, key, receiver)
    },
    has (target, key) {
      watch(key)
      return Reflect.has(target, key)
    },
  })
}

// The socket's fd, while this process still owns it as a plain blocking fd
// (until net.Socket takes it over): fail() then tells matchapack why.
let rawFd = null

// Tell matchapack why the hook stops before npm starts, and wait for its
// answer, so that the reason is recorded before this process exits
// (matchapack prints it in its "running plain npm instead" line and logs
// it). False when that could not be done.
function tellMatchapack (message) {
  if (rawFd === null) {
    return false
  }
  try {
    fs.writeSync(rawFd, JSON.stringify({ id: 'declined', declined: message }) + '\n')
    const b = Buffer.alloc(512)
    let got = ''
    for (;;) {
      const n = fs.readSync(rawFd, b, 0, b.length, null)
      if (n === 0) {
        return false
      }
      got += b.toString('utf8', 0, n)
      if (got.includes('\n')) {
        return true
      }
    }
  } catch {
    return false
  }
}

function fail (message) {
  if (!tellMatchapack(message)) {
    process.stderr.write(`matchapack inject helper: ${message}\n`)
  }
  process.exit(90)
}

function printFingerprint (npmDir) {
  let FetcherBase
  try {
    FetcherBase = fetcherBase(loadPacote(npmDir))
  } catch (e) {
    process.stderr.write(`cannot load npm's pacote from ${npmDir}: ${e.message}\n`)
    process.exit(1)
  }
  if (!FetcherBase) {
    process.stderr.write('this npm\'s pacote has no FetcherBase.prototype.extract shared by its fetchers\n')
    process.exit(1)
  }
  process.stdout.write(fingerprint(FetcherBase) + '\n')
}

function main () {
  const [npmDir, ...npmArgs] = process.argv.slice(2)
  const fd = Number(process.env.MATCHAPACK_INJECT_FD)
  const hookMode = process.env.MATCHAPACK_INJECT_HOOK
  const hookOff = hookMode === 'off'
  const allowed = process.env.MATCHAPACK_INJECT_FINGERPRINTS || ''
  // Not for the scripts npm runs.
  delete process.env.MATCHAPACK_INJECT_FD
  delete process.env.MATCHAPACK_INJECT_HOOK
  delete process.env.MATCHAPACK_INJECT_FINGERPRINTS
  if (!npmDir || !Number.isInteger(fd) || fd < 3) {
    fail('usage: MATCHAPACK_INJECT_FD=<fd> node inject.js <npm dir> <npm args...>')
  }
  rawFd = fd

  let pacote
  try {
    pacote = loadPacote(npmDir)
  } catch (e) {
    fail(`cannot load npm's pacote: ${e.message}`)
  }
  const FetcherBase = fetcherBase(pacote)
  if (!FetcherBase) {
    fail('this npm\'s pacote has no FetcherBase.prototype.extract shared by its fetchers')
  }
  // pacote code the harness has not run on: plain npm, before anything
  // changes (the same path as the shape check above).
  const fp = fingerprint(FetcherBase)
  if (allowed !== 'any' && !allowed.split(',').includes(fp)) {
    let npmVersion = '?'
    try {
      npmVersion = require(path.join(npmDir, 'package.json')).version
    } catch {
      // the message names the fingerprint either way
    }
    fail(`npm ${npmVersion} has pacote code this matchapack was not tested with; update matchapack, or set MATCHAPACK_NPM_FINGERPRINT=any to try it; fingerprint ${fp}`)
  }
  const origExtract = FetcherBase.prototype.extract
  const processUmask = process.umask()

  const net = require('net')
  let sock
  // From here on the fd is the socket's (non-blocking): fail() writes to stderr.
  rawFd = null
  try {
    sock = new net.Socket({ fd, readable: true, writable: true })
  } catch (e) {
    fail(`cannot open the socket to matchapack: ${e.message}`)
  }
  sock.setEncoding('utf8')
  // The socket keeps this process alive only while an answer is awaited.
  sock.unref()
  const pending = new Map()
  let nextId = 0
  // Set when no further request can succeed; the reason is noted per package.
  let down = hookOff ? 'hook-off' : null
  let buf = ''
  const settle = (id, fn) => {
    const p = pending.get(id)
    if (!p) {
      return
    }
    pending.delete(id)
    if (pending.size === 0) {
      sock.unref()
    }
    fn(p)
  }
  sock.on('data', (chunk) => {
    buf += chunk
    let i
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i)
      buf = buf.slice(i + 1)
      let msg
      try {
        msg = JSON.parse(line)
      } catch {
        continue
      }
      settle(msg.id, p => p.resolve(msg))
    }
  })
  const gone = () => {
    down = down || 'server-gone'
    for (const id of [...pending.keys()]) {
      settle(id, p => p.reject(new Error('matchapack is gone')))
    }
  }
  sock.on('error', gone)
  sock.on('close', gone)
  const send = (obj) => {
    if (!sock.destroyed && sock.writable) {
      sock.write(JSON.stringify(obj) + '\n')
    }
  }
  const request = (req) => new Promise((resolve, reject) => {
    if (sock.destroyed || !sock.writable) {
      return reject(new Error('matchapack is gone'))
    }
    const id = nextId++
    if (pending.size === 0) {
      sock.ref()
    }
    pending.set(id, { resolve, reject })
    send({ id, ...req })
  })

  const hook = async function (dest) {
    let why
    try {
      why = down || ineligible(pacote, this, processUmask)
    } catch {
      why = 'hook-error'
    }
    if (why) {
      send({ note: why })
      return origExtract.call(this, dest)
    }
    // npm's own project root, as it passes it to pacote (ci.js builds the
    // Arborist with `path: npm.prefix`; reify spreads its options into
    // pacote's): for diagnosis only, matchapack never widens its checks with
    // it (grimmerk/matchapack-private#36).
    const npmRoot = this.opts && typeof this.opts.path === 'string' && path.isAbsolute(this.opts.path) ? this.opts.path : undefined
    const req = { integrity: String(this.integrity), dest: path.resolve(dest), npm_root: npmRoot }
    let answer
    try {
      answer = await request(req)
      if (!answer.ok && answer.extract_to) {
        // pacote's own extraction, with this fetcher (its tarball, its
        // integrity check, its rules), into matchapack's directory.
        let failed = false
        try {
          await origExtract.call(this, answer.extract_to)
        } catch {
          failed = true
        }
        // Also on failure: matchapack releases what waited for this fill.
        answer = await request({ ...req, extracted: answer.extract_to, failed })
        if (failed) {
          return origExtract.call(this, dest)
        }
      }
    } catch {
      return origExtract.call(this, dest)
    }
    if (!answer.ok) {
      if (answer.fatal) {
        down = down || answer.reason || 'fatal'
      }
      // pacote empties the directory (never its node_modules) and extracts.
      return origExtract.call(this, dest)
    }
    // What pacote's extract resolves to. arborist does not read it today; a
    // read of any other key is reported, and matchapack then rebuilds the
    // tree with plain npm (wrapper.rs, the "broke" path).
    return resultTrap(
      { resolved: this.resolved, integrity: this.integrity && String(this.integrity), from: this.from },
      (key, expected) => send({ result_read: key, expected }))
  }

  if (hookMode !== 'none') {
    FetcherBase.prototype.extract = hook
  }

  // The pacote and tar this process really loaded: matchapack keys its
  // checks of the store on them (src/serve.rs).
  let pacoteVersion = null
  let tarVersion = null
  try {
    const pacoteDir = path.dirname(path.dirname(require.resolve('pacote', {
      paths: [path.join(npmDir, 'node_modules', '@npmcli', 'arborist')],
    })))
    pacoteVersion = require(path.join(pacoteDir, 'package.json')).version
    tarVersion = require(require.resolve('tar/package.json', { paths: [pacoteDir] })).version
  } catch {
    // unknown: every entry is compared, as under a new version
  }
  // From here on npm is running: matchapack reports its exit status as npm's.
  npmStarted = true
  send({ hello: true, pacote: pacoteVersion, tar: tarVersion, fingerprint: fp, hook: hookMode === 'none' ? 'none' : hookOff ? 'off' : 'on' })
  process.argv = [process.argv[0], path.join(npmDir, 'bin', 'npm-cli.js'), ...npmArgs]
  require(path.join(npmDir, 'lib', 'cli.js'))(process)
}

let npmStarted = false
if (require.main === module && process.argv[2] === '--fingerprint') {
  printFingerprint(process.argv[3] || '')
} else if (require.main === module) {
  try {
    main()
  } catch (e) {
    if (npmStarted) {
      throw e
    }
    fail(`before npm started: ${e && e.stack ? e.stack : e}`)
  }
} else {
  module.exports = { ineligible, resultTrap, fingerprint, fetcherBase, loadPacote, RESULT_KEYS }
}
