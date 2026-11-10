// The inject engine's node side (THINCLONE_ENGINE=inject; NOTES "Inject
// engine"). Runs npm's REAL command in this process, the way bin/npm-cli.js
// does, with one method replaced: pacote's FetcherBase.prototype.extract, the
// single place where npm puts a package's files into a directory. It is
// reached after pacote's policy checks (allow-git / allow-remote / allow-file
// / allow-directory / allow-registry run in FetcherBase.get, before a fetcher
// exists), so npm's refusals stay npm's.
//
// For a package the hook can reproduce exactly (see `ineligible`), it asks
// the thinclone process that started this one to place the package's store
// entry at the destination (clone, then set every placed path's time to now)
// and resolves when that is done. For anything else, and whenever thinclone
// answers with an error, it calls pacote's own extract, unchanged. So the
// worst case is plain `npm ci`: slower, never different.
//
// argv: <npm install dir> <npm args...>   (e.g. ci --no-audit)
// env:  THINCLONE_INJECT_FD    the socket to thinclone (protocol: src/serve.rs)
//       THINCLONE_INJECT_HOOK  `off`: install the hook but let pacote extract
//                              every package (tests, and to rule the hook out);
//                              `none`: do not install it at all (tests: what
//                              an npm that no longer calls the method looks
//                              like to thinclone)
// Exit status: npm's. 90 before npm starts: the hook could not be installed
// (thinclone then runs plain npm).
'use strict'
const path = require('path')

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

function fail (message) {
  process.stderr.write(`thinclone inject helper: ${message}\n`)
  process.exit(90)
}

function main () {
  const [npmDir, ...npmArgs] = process.argv.slice(2)
  const fd = Number(process.env.THINCLONE_INJECT_FD)
  const hookMode = process.env.THINCLONE_INJECT_HOOK
  const hookOff = hookMode === 'off'
  // Not for the scripts npm runs.
  delete process.env.THINCLONE_INJECT_FD
  delete process.env.THINCLONE_INJECT_HOOK
  if (!npmDir || !Number.isInteger(fd) || fd < 3) {
    fail('usage: THINCLONE_INJECT_FD=<fd> node inject.js <npm dir> <npm args...>')
  }

  // The pacote that npm's arborist loads: one copy, in npm's node_modules.
  let pacote
  try {
    const from = path.join(npmDir, 'node_modules', '@npmcli', 'arborist')
    pacote = require(require.resolve('pacote', { paths: [from] }))
  } catch (e) {
    fail(`cannot load npm's pacote: ${e.message}`)
  }
  // Every fetcher class extends FetcherBase; it is reached through pacote's
  // exports only.
  const FetcherBase = pacote.RemoteFetcher && Object.getPrototypeOf(pacote.RemoteFetcher)
  if (typeof FetcherBase?.prototype?.extract !== 'function' ||
      typeof pacote.RegistryFetcher !== 'function' ||
      pacote.RemoteFetcher.prototype.extract !== FetcherBase.prototype.extract ||
      pacote.RegistryFetcher.prototype.extract !== FetcherBase.prototype.extract) {
    fail('this npm\'s pacote has no FetcherBase.prototype.extract shared by its fetchers')
  }
  const origExtract = FetcherBase.prototype.extract
  const processUmask = process.umask()

  const net = require('net')
  let sock
  try {
    sock = new net.Socket({ fd, readable: true, writable: true })
  } catch (e) {
    fail(`cannot open the socket to thinclone: ${e.message}`)
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
      settle(id, p => p.reject(new Error('thinclone is gone')))
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
      return reject(new Error('thinclone is gone'))
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
    let answer
    try {
      answer = await request({
        integrity: String(this.integrity),
        dest: path.resolve(dest),
        resolved: this.resolved || undefined,
        cache: this.cache || undefined,
      })
    } catch {
      return origExtract.call(this, dest)
    }
    if (!answer.ok) {
      if (answer.fatal) {
        down = down || 'clone-unsupported'
      }
      // pacote empties the directory (never its node_modules) and extracts.
      return origExtract.call(this, dest)
    }
    // What pacote's extract resolves to; arborist does not read it.
    return { resolved: this.resolved, integrity: this.integrity && String(this.integrity), from: this.from }
  }

  if (hookMode !== 'none') {
    FetcherBase.prototype.extract = hook
  }

  let pacoteVersion = null
  try {
    pacoteVersion = require(path.join(path.dirname(require.resolve('pacote', {
      paths: [path.join(npmDir, 'node_modules', '@npmcli', 'arborist')],
    })), '..', 'package.json')).version
  } catch {
    // only for the log
  }
  // From here on npm is running: thinclone reports its exit status as npm's.
  send({ hello: true, pacote: pacoteVersion, hook: hookMode === 'none' ? 'none' : hookOff ? 'off' : 'on' })
  process.argv = [process.argv[0], path.join(npmDir, 'bin', 'npm-cli.js'), ...npmArgs]
  require(path.join(npmDir, 'lib', 'cli.js'))(process)
}

if (require.main === module) {
  main()
} else {
  module.exports = { ineligible }
}
