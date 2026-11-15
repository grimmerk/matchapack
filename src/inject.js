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
//   4. when npm has loaded the pacote hashed in 1: check its shape and
//      replace extract; then the watcher removes itself. A pacote of an
//      unexpected shape gets no hook: npm extracts every package itself.
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
// for CI.
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

// The options that filter a command to some workspaces or name its root.
const FILTERS = new Set(['workspace', 'workspaces', 'include-workspace-root', 'prefix'])

// `{ args, npmrc }` for the repair advice of a broken run (wrapper.rs), or
// null: `args`, the command's options as npm parsed them (`-w` expanded,
// `--x=y` split: nopt's `cooked`) without the command word, the package
// names, and the options in FILTERS with their values, an abbreviation
// (`--prefi`) resolved as npm's nopt resolves it; `npmrc`, the npmrc layers
// (project, user, global, builtin) whose `workspace` or `workspaces` is in
// effect.
function repairInfo (config, typed, positional) {
  try {
    const cooked = config.parsedArgv && config.parsedArgv.cooked
    const defs = config.definitions
    if (!Array.isArray(cooked) || !defs) {
      return null
    }
    const names = Object.keys(defs)
    // nopt resolves an abbreviation to the one option it starts (`abbrev`).
    const resolve = (name) => {
      if (defs[name]) {
        return name
      }
      const hits = names.filter(n => n.startsWith(name))
      return hits.length === 1 ? hits[0] : null
    }
    const isBool = (key) => {
      const t = defs[key] && defs[key].type
      return t === Boolean || (Array.isArray(t) && t.includes(Boolean))
    }
    const args = []
    let cmdSeen = false
    for (let i = 0; i < cooked.length; i++) {
      const t = String(cooked[i])
      if (t.startsWith('--') && t.length > 2) {
        let name = t.slice(2)
        let neg = false
        if (!defs[name] && name.startsWith('no-')) {
          name = name.slice(3)
          neg = true
        }
        const key = resolve(name)
        if (key && FILTERS.has(key)) {
          const next = cooked[i + 1]
          if (!neg && (!isBool(key) || next === 'true' || next === 'false') && next !== undefined) {
            i++
          }
          continue
        }
        args.push(t)
        continue
      }
      if (!cmdSeen && t === typed) {
        cmdSeen = true
        continue
      }
      args.push(t)
    }
    // Package names (the command's positional arguments) are not kept: a
    // whole-project rebuild installs from package.json.
    for (const name of Array.isArray(positional) ? positional : []) {
      const i = args.indexOf(String(name))
      if (i >= 0) {
        args.splice(i, 1)
      }
    }
    const npmrc = []
    for (const key of ['workspace', 'workspaces']) {
      const v = config.get(key)
      const on = key === 'workspace' ? Array.isArray(v) && v.length > 0 : v === true
      const where = on && typeof config.find === 'function' ? config.find(key) : null
      if (where && ['project', 'user', 'global', 'builtin'].includes(where) && !npmrc.includes(where)) {
        npmrc.push(where)
      }
    }
    return { args, npmrc }
  } catch {
    return null
  }
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
  // The pacote and tar npm will load: matchapack keys its checks of the
  // store on them (src/serve.rs).
  let pacoteVersion = null
  let tarVersion = null
  try {
    const pacoteDir = path.dirname(path.dirname(paths.main))
    pacoteVersion = versionOf(path.join(pacoteDir, 'package.json'))
    tarVersion = versionOf(require.resolve('tar/package.json', { paths: [pacoteDir] }))
  } catch {
    // unknown: every entry is compared, as under a new version
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
  // Set in step 3 when matchapack accepted npm's install root: the root as
  // npm reported it and as matchapack pinned it (canonical).
  let roots = null
  const insideRoot = (d) => roots.some(r => d === r || d.startsWith(r.endsWith(path.sep) ? r : r + path.sep))
  const matchapackExtract = async function (dest) {
    const to = path.resolve(dest)
    // Not an install into the project: pacote extracts a package given as
    // a URL, a local tarball or a git repository into a temporary directory
    // in npm's cache to read its manifest, through this same method.
    // pacote's own extract, counted apart (not a package npm extracted into
    // the project; src/serve.rs). Compared as npm names the path: a
    // workspace symlinked to a directory outside the project is under the
    // root by name, and its packages go to matchapack, whose destination
    // check refuses them (resolved, they are outside).
    if (roots && !insideRoot(to)) {
      send({ aside: 'outside-root' })
      return origExtract.call(this, dest)
    }
    let why
    try {
      why = down || ineligible(pacote, this, processUmask)
    } catch {
      why = 'hook-error'
    }
    if (why) {
      // The destination: matchapack compares the packages npm placed with
      // the destinations the hook saw ("unseen", src/inject.rs).
      send({ note: why, dest: to })
      return origExtract.call(this, dest)
    }
    // npm's own project root, as it passes it to pacote (ci.js builds the
    // Arborist with `path: npm.prefix`; reify spreads its options into
    // pacote's): matchapack refuses a request whose root is not the one npm
    // reported in step 3 (`npm-root-elsewhere`); the destination check uses
    // the pinned root.
    const npmRoot = this.opts && typeof this.opts.path === 'string' && path.isAbsolute(this.opts.path) ? this.opts.path : undefined
    const req = { integrity: String(this.integrity), dest: to, npm_root: npmRoot }
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
    // read of any other key is reported, and matchapack treats the run as
    // broken (wrapper.rs: npm ci is rebuilt with plain npm ci; the install
    // family stops with the repair command).
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
          // Whether npm writes package-lock.json in this run (false with
          // --no-package-lock or package-lock=false): what a repair after
          // a broken run can rebuild from (wrapper.rs).
          package_lock: flag('package-lock'),
          // Whether npm saves package.json and package-lock.json (false
          // with --no-save), and whether the command is filtered to some
          // workspaces (-w, --workspaces, or started inside a member, where
          // npm sets `workspace` itself): the repair must then rebuild the
          // whole project.
          save: flag('save'),
          workspace_filter: (() => {
            try {
              const w = this.config.get('workspace')
              return (Array.isArray(w) && w.length > 0) || this.config.get('workspaces') === true
            } catch {
              return false
            }
          })(),
          usage: flag('usage'),
          // For the repair after a broken run (wrapper.rs): the command's
          // arguments as npm parsed them, without the ones that filter it
          // to some workspaces or name its root, abbreviations resolved by
          // npm's own rules; and the npmrc files that set such a filter
          // (the command line cannot unset them).
          // The package names are the arguments exec is given (npm 11 and
          // 12 pass them; npm 10 defaults to npm.argv, its parse's `remain`
          // with the command word already shifted off).
          repair: repairInfo(this.config, String(cmd), Array.isArray(arguments[1]) ? arguments[1] : this.argv),
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
      if (answer && answer.ok === true) {
        roots = [report.root, answer.root].filter(r => typeof r === 'string' && path.isAbsolute(r))
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
  send({ hello: true, pacote: pacoteVersion, tar: tarVersion, fingerprint: fp, hook: hookMode === 'none' ? 'none' : hookOff ? 'off' : 'pending' })
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
  module.exports = { ineligible, resultTrap, fingerprint, fingerprintFile, fetcherBase, pacotePaths, repairInfo, RESULT_KEYS }
}
