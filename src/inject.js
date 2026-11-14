// The inject engine's node side (MATCHAPACK_ENGINE=inject; NOTES "Inject
// engine"). Runs npm's REAL command in this process, the way bin/npm-cli.js
// does, with one method replaced: pacote's FetcherBase.prototype.extract, the
// single place where npm puts a package's files into a directory. It is
// reached after pacote's policy checks (allow-git / allow-remote / allow-file
// / allow-directory / allow-registry run in FetcherBase.get, before a fetcher
// exists), so npm's refusals stay npm's.
//
// Nothing of npm is loaded before npm's own start-up code runs
// (grimmerk/matchapack-private#59; NOTES "The lazy hook and npm's install
// root"). In order:
//   1. before npm: find npm's pacote (resolved, not loaded) and hash its
//      lib/fetcher.js; a file not on the list is "tell matchapack, exit 90",
//      and matchapack runs plain npm;
//   2. a watcher on Module.prototype.require, then npm's lib/cli.js;
//   3. when npm has loaded lib/npm.js: wrap Npm.prototype.exec (first call
//      only). npm calls it once its config is loaded, with the command; the
//      wrapper reports npm's own install root (npm.prefix, npm.global) and
//      the command to matchapack, which runs its gates on that root and pins
//      it (#36). A refusal is exit 90 before the command ran;
//   4. when npm has loaded the pacote hashed in 1 (which loads tar): tell
//      matchapack the store key of this extraction code (`extractionKey`),
//      check pacote's shape and replace extract; then the watcher removes
//      itself. A pacote of an unexpected shape gets no hook: npm extracts
//      every package itself.
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
// Exit status: npm's. 90 when the helper stops before npm's command runs:
// before npm starts (unknown fingerprint, no pacote; also on an unexpected
// error up to there), or when matchapack refuses the install root npm
// reported. matchapack then runs plain npm.
//
// `node inject.js --fingerprint <npm install dir>` prints the fingerprint of
// that npm's pacote and exits: the one way it is computed, for the list and
// for CI. `--key <npm install dir>` prints the store key of that npm's
// extraction code under this Node, and its tar fingerprint.
'use strict'
const path = require('path')
const fs = require('fs')
const crypto = require('crypto')

// The exit status of a stop before npm's command runs (inject.rs
// HELPER_DECLINED).
const DECLINED = 90

// The pacote that npm's arborist loads (one copy that extracts, in npm's
// node_modules; npm 10.9.1–10.9.9 carry a second one for audit metadata),
// resolved, not loaded: `main`, the file npm's require of it loads, and
// `fetcher`, lib/fetcher.js next to it, which holds the class the hook
// replaces the work of. Real paths, as require.cache keys them.
function pacotePaths (npmDir) {
  const main = require.resolve('pacote', { paths: [path.join(npmDir, 'node_modules', '@npmcli', 'arborist')] })
  return { main, fetcher: path.join(path.dirname(main), 'fetcher.js') }
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
// #empty), which no code outside the class can reach. They are all in
// pacote's lib/fetcher.js, read as bytes from disk before npm starts, so
// nothing of npm has to be loaded to decide (#59): its SHA-256 is the
// fingerprint. The hook is installed only on a fingerprint the harness has
// run on (src/npm-fingerprints.json; NOTES "G-lite: npm release safety").
// Until 2026-11 it was the text of the loaded class alone; the file holds
// that text, so the gate only became stricter.
function fingerprintFile (file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
}

function fingerprint (npmDir) {
  return fingerprintFile(pacotePaths(npmDir).fetcher)
}

const sha256 = (data) => crypto.createHash('sha256').update(data).digest('hex')

// The directory of the package `name` as Node finds it from `from` (a
// package directory): the nearest node_modules/<name> upwards. Real path,
// as require.cache keys the files.
function packageDir (name, from) {
  for (let d = from; ; d = path.dirname(d)) {
    if (path.basename(d) !== 'node_modules') {
      const dir = path.join(d, 'node_modules', name)
      if (fs.existsSync(path.join(dir, 'package.json'))) {
        return fs.realpathSync(dir)
      }
    }
    if (path.dirname(d) === d) {
      throw new Error(`cannot find ${name} from ${from}`)
    }
  }
}

// The packages tar's code is made of, as directory → name: tar as pacote
// (in `pacoteDir`) resolves it, and every package in its `dependencies`,
// transitively, each resolved from the package that depends on it. Even
// when tar bundles its code, pacote hands it streams of these packages.
function tarPackages (pacoteDir) {
  const found = new Map()
  const visit = (name, from) => {
    const dir = packageDir(name, from)
    if (found.has(dir)) {
      return
    }
    found.set(dir, name)
    const deps = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')).dependencies || {}
    for (const d of Object.keys(deps)) {
      visit(d, dir)
    }
  }
  visit('tar', pacoteDir)
  return found
}

// The tar fingerprint (NOTES "PR C1: the key is the code"): the SHA-256 over
// the sorted lines `<package name>/<path in the package>`, NUL, the file's
// SHA-256, newline, one per file in `cache` (require.cache) that lies in one
// of tarPackages' directories (a file under a nested node_modules of one
// belongs to that nested package, so only when it is in the set too),
// except each package's own package.json (code that only reads a version
// would otherwise change it; which files its fields make Node load is
// counted anyway). What counts is what Node loaded: called once pacote is
// loaded, which loads tar. Throws when no file of tar itself is loaded.
function tarFingerprint (pacoteDir, cache = require.cache) {
  const pkgs = [...tarPackages(pacoteDir)]
  const tarDir = pkgs[0][0]
  const lines = []
  let ofTar = 0
  for (const file of Object.keys(cache)) {
    let best = null
    for (const [dir, name] of pkgs) {
      if (file.startsWith(dir + path.sep) && (!best || dir.length > best[0].length)) {
        best = [dir, name]
      }
    }
    if (!best) {
      continue
    }
    const rel = path.relative(best[0], file).split(path.sep)
    if (rel.includes('node_modules') || rel.join('/') === 'package.json') {
      continue
    }
    if (best[0] === tarDir) {
      ofTar++
    }
    lines.push(`${best[1]}/${rel.join('/')}\0${sha256(fs.readFileSync(file))}\n`)
  }
  if (ofTar === 0) {
    throw new Error(`no file of tar (${tarDir}) is loaded`)
  }
  return sha256(lines.sort().join(''))
}

// The store key (serve.rs `code_key`): the extraction code npm runs, as
// pacote's lib/fetcher.js (the gate's fingerprint), tar's loaded files, and
// libuv's version. null when a part is missing: matchapack then compares
// every entry it uses.
function extractionKey (fetcherFp, tarFp, uv) {
  const hex = (s) => typeof s === 'string' && /^[0-9a-f]{64}$/.test(s)
  if (!hex(fetcherFp) || !hex(tarFp) || typeof uv !== 'string' || !/^[0-9]+(\.[0-9]+)*$/.test(uv)) {
    return null
  }
  return `fetcher:${fetcherFp.slice(0, 32)} tar:${tarFp.slice(0, 32)} uv:${uv}`
}

// The key and the tar fingerprint for the pacote at `paths` (pacotePaths),
// once it is loaded; nulls, and why, when they cannot be had.
function extractionCode (paths, fetcherFp) {
  try {
    const tarFp = tarFingerprint(path.dirname(path.dirname(paths.main)))
    return { key: extractionKey(fetcherFp, tarFp, process.versions.uv), tar_fingerprint: tarFp }
  } catch (e) {
    return { key: null, tar_fingerprint: null, error: String((e && e.message) || e) }
  }
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
  process.exit(DECLINED)
}

function printFingerprint (npmDir) {
  let fp
  try {
    fp = fingerprint(npmDir)
  } catch (e) {
    process.stderr.write(`cannot read npm's pacote lib/fetcher.js from ${npmDir}: ${e.message}\n`)
    process.exit(1)
  }
  process.stdout.write(fp + '\n')
}

// `--key`: the store key of that npm's extraction code, run on this Node
// (its libuv), and the full tar fingerprint, on two lines. Loads pacote.
function printKey (npmDir) {
  let code
  try {
    const paths = pacotePaths(npmDir)
    const fp = fingerprintFile(paths.fetcher)
    require(paths.main)
    code = extractionCode(paths, fp)
  } catch (e) {
    code = { key: null, error: e.message }
  }
  if (!code.key) {
    process.stderr.write(`cannot name the extraction code of the npm in ${npmDir}: ${code.error || 'a part of the key is missing'}\n`)
    process.exit(1)
  }
  process.stdout.write(`${code.key}\n${code.tar_fingerprint}\n`)
}

// A package.json's version, read as JSON: no code of npm runs for it.
function versionOf (file) {
  return JSON.parse(fs.readFileSync(file, 'utf8')).version
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

  // 1. Nothing of npm is loaded here (#59): pacote's files are found and
  // read, and npm's lib/npm.js is found.
  let paths
  try {
    paths = pacotePaths(npmDir)
  } catch (e) {
    fail(`cannot find npm's pacote: ${e.message}`)
  }
  let fp
  try {
    fp = fingerprintFile(paths.fetcher)
  } catch (e) {
    fail(`cannot read npm's pacote (${paths.fetcher}): ${e.code || e.message}`)
  }
  // pacote code the harness has not run on: plain npm, before anything
  // changes.
  if (allowed !== 'any' && !allowed.split(',').includes(fp)) {
    let npmVersion = '?'
    try {
      npmVersion = versionOf(path.join(npmDir, 'package.json'))
    } catch {
      // the message names the fingerprint either way
    }
    fail(`npm ${npmVersion} has pacote code this matchapack was not tested with; update matchapack, or set MATCHAPACK_NPM_FINGERPRINT=any to try it; fingerprint ${fp}`)
  }
  // As require.cache keys it: the real path (npm's lib may be a symlink).
  let npmJs
  try {
    npmJs = fs.realpathSync(path.join(npmDir, 'lib', 'npm.js'))
  } catch (e) {
    fail(`cannot find npm's lib/npm.js: ${e.code || e.message}`)
  }
  // The pacote and tar versions npm will load, for the log and messages
  // (the store key is the code itself, sent in step 4).
  let pacoteVersion = null
  let tarVersion = null
  try {
    const pacoteDir = path.dirname(path.dirname(paths.main))
    pacoteVersion = versionOf(path.join(pacoteDir, 'package.json'))
    tarVersion = versionOf(require.resolve('tar/package.json', { paths: [pacoteDir] }))
  } catch {
    // unknown: the log says `?`
  }
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

  // Set in step 4, when npm has loaded pacote.
  let pacote = null
  let origExtract = null
  const matchapackExtract = async function (dest) {
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
    // pacote's): matchapack refuses a request whose root is not the one npm
    // reported in step 3 (`npm-root-elsewhere`); the destination check uses
    // the pinned root.
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

  // Step 3: npm's own install root, when npm starts the command (#36).
  const wrapExec = (Npm) => {
    const origExec = Npm && Npm.prototype && Npm.prototype.exec
    if (typeof origExec !== 'function') {
      // No root is reported: matchapack refuses every clone, npm extracts
      // every package itself, and the run says why.
      send({ no_root: 'npm\'s lib/npm.js has no Npm.prototype.exec' })
      return
    }
    Npm.prototype.exec = async function (cmd) {
      // The first call only: a command that runs another one (install-test
      // runs test) calls exec again, and that passes through.
      Npm.prototype.exec = origExec
      let report
      try {
        const { deref } = require(path.join(path.dirname(npmJs), 'utils', 'cmd-list.js'))
        if (typeof deref !== 'function') {
          throw new Error('lib/utils/cmd-list.js has no deref')
        }
        const flag = (k) => {
          try {
            return !!this.config.get(k)
          } catch {
            return false
          }
        }
        const root = this.prefix
        if (typeof root !== 'string' || !path.isAbsolute(root)) {
          throw new Error(`npm.prefix is not an absolute path (${typeof root})`)
        }
        report = {
          root,
          global: !!this.global,
          command: deref(cmd) || null,
          typed: String(cmd),
          dry_run: flag('dry-run'),
          package_lock_only: flag('package-lock-only'),
          usage: flag('usage'),
        }
      } catch (e) {
        send({ no_root: `could not read npm's install root: ${e && e.message}` })
        return origExec.apply(this, arguments)
      }
      let answer = null
      try {
        answer = await request(report)
      } catch {
        // matchapack is gone: every later request falls back to pacote
      }
      if (answer && answer.ok === false) {
        // Refused (the reason is recorded): npm has loaded its config and
        // opened its log file, the command has not run. npm's own unload
        // stops its timers, display and log file; without its exit
        // listeners npm does not print "Exit handler never called!".
        try {
          if (typeof this.unload === 'function') {
            this.unload()
          }
        } catch {
          // exiting either way
        }
        process.removeAllListeners('exit')
        process.exit(DECLINED)
      }
      return origExec.apply(this, arguments)
    }
  }

  // Step 4: npm has loaded the pacote whose lib/fetcher.js was hashed.
  const hookPacote = (p) => {
    if (hookMode === 'none') {
      send({ hook: 'none' })
      return
    }
    let why = null
    let FetcherBase = null
    try {
      FetcherBase = fetcherBase(p)
      if (!FetcherBase) {
        why = 'this npm\'s pacote has no FetcherBase.prototype.extract shared by its fetchers'
      }
    } catch (e) {
      why = `checking npm's pacote: ${e && e.message}`
    }
    if (why) {
      // npm is running: no "exit 90, plain npm" any more. No hook: npm
      // extracts every package itself, and the run says why.
      send({ hook: 'not-installed', reason: why })
      return
    }
    pacote = p
    origExtract = FetcherBase.prototype.extract
    FetcherBase.prototype.extract = matchapackExtract
    send({ hook: hookOff ? 'off' : 'on' })
  }

  // Step 2: watch npm's own requires for the two files. Nothing is resolved
  // or loaded here; after each require returns, two cache lookups. The
  // watcher removes itself once both were seen.
  const Module = require('module')
  const origRequire = Module.prototype.require
  let needNpm = true
  let needPacote = true
  const watcher = function () {
    const exports = origRequire.apply(this, arguments)
    if (needNpm) {
      const m = require.cache[npmJs]
      if (m && m.loaded) {
        needNpm = false
        try {
          wrapExec(m.exports)
        } catch (e) {
          send({ no_root: `wrapping npm's exec: ${e && e.message}` })
        }
      }
    }
    if (needPacote) {
      const m = require.cache[paths.main]
      if (m && m.loaded) {
        needPacote = false
        // Before the hook: matchapack has the key before any request.
        const code = extractionCode(paths, fp)
        send({ key: code.key, tar_fingerprint: code.tar_fingerprint, ...(code.error ? { key_error: code.error } : {}) })
        hookPacote(m.exports)
      }
    }
    if (!needNpm && !needPacote && Module.prototype.require === watcher) {
      Module.prototype.require = origRequire
    }
    return exports
  }
  Module.prototype.require = watcher

  // From here on npm is running: matchapack reports its exit status as npm's
  // (unless the root is refused in step 3).
  npmStarted = true
  send({ hello: true, pacote: pacoteVersion, tar: tarVersion, uv: process.versions.uv, fingerprint: fp, hook: hookMode === 'none' ? 'none' : hookOff ? 'off' : 'pending' })
  process.argv = [process.argv[0], path.join(npmDir, 'bin', 'npm-cli.js'), ...npmArgs]
  require(path.join(npmDir, 'lib', 'cli.js'))(process)
}

let npmStarted = false
if (require.main === module && process.argv[2] === '--fingerprint') {
  printFingerprint(process.argv[3] || '')
} else if (require.main === module && process.argv[2] === '--key') {
  printKey(process.argv[3] || '')
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
  module.exports = { ineligible, resultTrap, fingerprint, fingerprintFile, fetcherBase, pacotePaths, RESULT_KEYS, tarPackages, tarFingerprint, extractionKey, extractionCode }
}
