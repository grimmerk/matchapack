// Unit checks of the inject hook's eligibility rules (src/inject.js
// `ineligible`), its result trap (`resultTrap`), its fingerprint, and the
// helper's start-up (nothing of npm loaded before npm, the install root npm
// reports, the lazy hook; #59, #36), against
// the fetcher objects of the npm given: the real
// pacote classes are constructed, so a renamed field or a changed default in
// a new pacote fails here instead of silently turning every package into a
// fallback (or worse, into a clone npm would have extracted differently).
//   node scripts/test-inject-hook.js <npm install dir>
// Prints one PASS/FAIL line per case; exit 1 when any failed.
'use strict'
const path = require('path')
const fs = require('fs')
const os = require('os')
const crypto = require('crypto')
const npmDir = fs.realpathSync(process.argv[2])
const util = require('util')
const { execFileSync, spawn } = require('child_process')
const injectJs = path.join(__dirname, '..', 'src', 'inject.js')
const { ineligible, resultTrap, fingerprint, fetcherBase } = require(injectJs)
const pacoteMain = require.resolve('pacote', { paths: [path.join(npmDir, 'node_modules', '@npmcli', 'arborist')] })
const pacote = require(pacoteMain)

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
// The helper's start-up (#59) and the install root it reports (#36), on this
// npm: src/inject.js started with a socket as fd 3, as inject.rs starts it,
// and a stand-in for matchapack on the other end that answers the root
// message as `rootAnswer` says. A probe loaded before the helper
// (`--require`) wraps Module.prototype.require first and reports:
//   - how many files under the npm directory are in require.cache when npm's
//     lib/cli.js is first required (#59: the helper loads none of npm);
//   - at exit, whether Module.prototype.require is the probe's own again
//     (the helper's watcher removed itself), and which loaded pacote copies
//     have their FetcherBase.prototype.extract replaced by the hook.
// The project is empty (no dependencies), so `npm ci` loads arborist and
// pacote, extracts nothing and needs no registry.
function helperRun ({ args, rootAnswer, cwd, env = {} }) {
  const tmp = helperRun.tmp
  const probe = path.join(tmp, 'probe.js')
  fs.writeFileSync(probe, `
const Module = require('module')
const npmDir = ${JSON.stringify(npmDir + path.sep)}
const cli = ${JSON.stringify(path.join(npmDir, 'lib', 'cli.js'))}
const orig = Module.prototype.require
let report = null
const probe = function (id) {
  if (!report) {
    let file = null
    try { file = Module._resolveFilename(id, this) } catch {}
    if (file === cli) {
      report = Object.keys(require.cache).filter(k => k.startsWith(npmDir)).map(k => k.slice(npmDir.length))
    }
  }
  return orig.apply(this, arguments)
}
Module.prototype.require = probe
process.on('exit', () => {
  const hooked = []
  for (const k of Object.keys(require.cache)) {
    if (!k.endsWith('/pacote/lib/index.js')) continue
    const R = require.cache[k].exports && require.cache[k].exports.RemoteFetcher
    const F = R && Object.getPrototypeOf(R)
    hooked.push([k.slice(npmDir.length), !!(F && F.prototype.extract && F.prototype.extract.name === 'matchapackExtract')])
  }
  process.stderr.write('PROBE ' + JSON.stringify({ beforeCli: report, requireRestored: Module.prototype.require === probe, hooked }) + '\\n')
})
`)
  return new Promise((resolve) => {
    const child = spawn(process.execPath, ['--require', probe, injectJs, npmDir, ...args], {
      cwd,
      env: {
        ...process.env,
        ...env,
        MATCHAPACK_INJECT_FD: '3',
        MATCHAPACK_INJECT_FINGERPRINTS: 'any',
        npm_config_cache: path.join(tmp, 'cache'),
        npm_config_userconfig: path.join(tmp, 'npmrc'),
        npm_config_update_notifier: 'false',
        npm_config_audit: 'false',
        npm_config_fund: 'false',
      },
      stdio: ['ignore', 'pipe', 'pipe', 'pipe'],
    })
    let out = ''
    let err = ''
    const messages = []
    let buf = ''
    child.stdout.on('data', d => { out += d })
    child.stderr.on('data', d => { err += d })
    child.stdio[3].on('error', () => {})
    child.stdio[3].on('data', (d) => {
      buf += d
      let i
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i)
        buf = buf.slice(i + 1)
        let m
        try {
          m = JSON.parse(line)
        } catch {
          continue
        }
        messages.push(m)
        const answer = (a) => child.stdio[3].write(JSON.stringify({ id: m.id, ...a }) + '\n')
        if (m.id === 'declined') {
          answer({ ok: true })
        } else if (m.root !== undefined && m.integrity === undefined) {
          answer(rootAnswer(m))
        } else if (m.integrity !== undefined) {
          answer({ ok: false, reason: 'test', message: 'test', fatal: false })
        }
      }
    })
    // 'close', not 'exit': 'exit' may come before the last stderr or fd 3
    // data has been read (Node's child_process docs), so a run could look
    // as if it had no probe line or fewer messages.
    child.on('close', (code) => {
      const line = err.split('\n').find(l => l.startsWith('PROBE '))
      resolve({ code, out, err, messages, probe: line ? JSON.parse(line.slice(6)) : null })
    })
  })
}

async function helperCases () {
  const tmp = helperRun.tmp = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), 'matchapack-hook-test-'))
  fs.writeFileSync(path.join(tmp, 'npmrc'), '')
  const project = (name) => {
    const d = path.join(tmp, name)
    fs.mkdirSync(d)
    fs.writeFileSync(path.join(d, 'package.json'), JSON.stringify({ name: 'tiny', version: '1.0.0', scripts: { test: 'node -e "process.exit(0)"' } }))
    fs.writeFileSync(path.join(d, 'package-lock.json'), JSON.stringify({ name: 'tiny', version: '1.0.0', lockfileVersion: 3, requires: true, packages: { '': { name: 'tiny', version: '1.0.0' } } }))
    return d
  }
  const roots = (r) => r.messages.filter(m => m.root !== undefined && m.integrity === undefined)
  try {
    // #59: npm prints its version; nothing of npm was loaded before its
    // lib/cli.js, and the hello names the file fingerprint.
    {
      const r = await helperRun({ args: ['--version'], rootAnswer: () => ({ ok: true }), cwd: tmp })
      const hello = r.messages.find(m => m.hello)
      check('helper: no file of npm is loaded before npm\'s lib/cli.js (#59)',
        JSON.stringify([r.code, r.out.trim(), r.probe && r.probe.beforeCli]),
        JSON.stringify([0, require(path.join(npmDir, 'package.json')).version, []]))
      check('helper: the hello carries the file fingerprint and the pending hook',
        JSON.stringify(hello && [hello.fingerprint, hello.hook]), JSON.stringify([fingerprint(npmDir), 'pending']))
    }
    // npm ci: the root npm reports once, when it starts the command; the
    // hook on arborist's pacote once npm loaded it; the watcher gone.
    {
      const d = project('ci')
      const r = await helperRun({ args: ['ci'], rootAnswer: (m) => ({ ok: true, root: m.root }), cwd: d })
      const rs = roots(r)
      check('helper: npm ci reports its install root once, with the command',
        JSON.stringify([r.code, rs.length, rs[0] && rs[0].root, rs[0] && rs[0].command, rs[0] && rs[0].global]),
        JSON.stringify([0, 1, d, 'ci', false]))
      const hooks = r.messages.filter(m => m.hook !== undefined && !m.hello)
      check('helper: the hook is installed when npm has loaded pacote, and said once', JSON.stringify(hooks), JSON.stringify([{ hook: 'on' }]))
      check('helper: only arborist\'s pacote, the one hashed, carries the hook',
        JSON.stringify(r.probe && r.probe.hooked.filter(([, on]) => on).map(([k]) => k)),
        JSON.stringify([path.relative(npmDir, pacoteMain)]))
      check('helper: the watcher on Module.prototype.require removed itself', r.probe && r.probe.requireRestored, true)
      check('helper: no pacote file was loaded before npm\'s lib/cli.js in npm ci either',
        JSON.stringify(r.probe && r.probe.beforeCli), JSON.stringify([]))
    }
    // An alias, deref'd by npm's own table.
    {
      const d = project('alias')
      const r = await helperRun({ args: ['clean-install'], rootAnswer: (m) => ({ ok: true, root: m.root }), cwd: d })
      check('helper: an alias is reported as npm\'s command name', JSON.stringify([r.code, roots(r).map(m => [m.command, m.typed])]), JSON.stringify([0, [['ci', 'clean-install']]]))
    }
    // A command that runs another (install-ci-test runs test through
    // npm.exec): the second exec passes through, the root is reported once.
    {
      const d = project('cit')
      const r = await helperRun({ args: ['install-ci-test'], rootAnswer: (m) => ({ ok: true, root: m.root }), cwd: d })
      check('helper: a nested npm.exec passes through (one root report)', JSON.stringify([r.code, roots(r).map(m => m.command)]), JSON.stringify([0, ['install-ci-test']]))
    }
    // The root refused: exit 90 before the command ran, node_modules as it
    // was, npm silent about its exit handler.
    {
      const d = project('refused')
      fs.mkdirSync(path.join(d, 'node_modules', 'sentinel'), { recursive: true })
      const r = await helperRun({ args: ['ci'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: d })
      check('helper: a refused root exits 90 before the command, node_modules untouched, no "Exit handler never called"',
        JSON.stringify([r.code, fs.existsSync(path.join(d, 'node_modules', 'sentinel')), /Exit handler never called/.test(r.err), roots(r).length]),
        JSON.stringify([90, true, false, 1]))
    }
    // npm's own values: global from the environment, --prefix, a workspace
    // member (its root is the workspace root).
    {
      const d = project('global')
      const r = await helperRun({ args: ['ci'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: d, env: { npm_config_global: 'true' } })
      check('helper: npm_config_global=true is reported as global', JSON.stringify(roots(r).map(m => m.global)), JSON.stringify([true]))
      const other = project('prefixed')
      const r2 = await helperRun({ args: ['ci', '--prefix', other], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: d })
      check('helper: --prefix <dir> is reported as the root', JSON.stringify(roots(r2).map(m => m.root)), JSON.stringify([other]))
      // npm's package-lock setting (what a repair after a broken run can
      // rebuild from): true by default, false with --no-package-lock.
      const r4 = await helperRun({ args: ['install', '--no-package-lock'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: d })
      check('helper: package-lock is reported (true for npm ci, false with --no-package-lock)',
        JSON.stringify([roots(r).map(m => m.package_lock), roots(r4).map(m => [m.command, m.package_lock])]), JSON.stringify([[true], [['install', false]]]))
      const ws = path.join(tmp, 'ws')
      fs.mkdirSync(path.join(ws, 'packages', 'a'), { recursive: true })
      fs.writeFileSync(path.join(ws, 'package.json'), JSON.stringify({ name: 'ws', version: '1.0.0', workspaces: ['packages/*'] }))
      fs.writeFileSync(path.join(ws, 'packages', 'a', 'package.json'), JSON.stringify({ name: 'a', version: '1.0.0' }))
      const r3 = await helperRun({ args: ['ci', '--dry-run'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: path.join(ws, 'packages', 'a') })
      check('helper: in a workspace member, npm\'s root is the workspace root; --dry-run is reported',
        JSON.stringify(roots(r3).map(m => [m.root, m.dry_run])), JSON.stringify([[ws, true]]))
      // save (false with --no-save) and the workspace filter (-w, or started
      // inside a member, where npm sets `workspace` itself): what a repair
      // after a broken run must rebuild (wrapper.rs).
      const r5 = await helperRun({ args: ['install', '--no-save'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: d })
      const r6 = await helperRun({ args: ['install', '-w', 'a'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: ws })
      check('helper: save and the workspace filter are reported',
        JSON.stringify([...[r, r5, r6, r3].map(x => roots(x).map(m => [m.save, m.workspace_filter])[0])]),
        JSON.stringify([[true, false], [false, false], [true, true], [true, true]]))
      // What the repair advice keeps of the command (src/inject.js
      // repairInfo), from npm's own parse: the workspace filter and the root
      // gone, abbreviations included (--prefi, --include-workspace-r), the
      // command word and package names gone, other options kept. And the
      // npmrc layer that sets a workspace filter.
      const r7 = await helperRun({ args: ['install', '--prefi=' + other, '-w', 'a', 'left-pad', '--omit', 'dev', '--include-workspace-r', '--legacy-peer-deps'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: d })
      fs.writeFileSync(path.join(ws, '.npmrc'), 'workspace=a\n')
      const r8 = await helperRun({ args: ['install', '--no-save'], rootAnswer: () => ({ ok: false, reason: 'test refusal' }), cwd: ws })
      fs.rmSync(path.join(ws, '.npmrc'))
      check('helper: the repair keeps the options npm parsed, without the filter and the root (abbreviations too), and names an npmrc filter',
        JSON.stringify([roots(r7).map(m => m.repair)[0], roots(r8).map(m => m.repair)[0]]),
        JSON.stringify([{ args: ['--omit', 'dev', '--legacy-peer-deps'], npmrc: [] }, { args: ['--no-save'], npmrc: ['project'] }]))
    }
    // The probe of the tarball server gives up on a listener that accepts
    // and never answers.
    {
      const silent = require('net').createServer(() => {})
      await new Promise(resolve => silent.listen(0, '127.0.0.1', resolve))
      const t0 = Date.now()
      const got = await reachable(`http://127.0.0.1:${silent.address().port}/x.tgz`)
      silent.close()
      check('the tarball-server probe gives up on a listener that never answers', JSON.stringify([got, Date.now() - t0 < 5000]), JSON.stringify([false, true]))
    }
    // A package given as a URL (the fixtures' tarball server, which
    // test-inject.sh starts): pacote reads its manifest by extracting it
    // into a temporary directory in npm's cache, through the hooked method.
    // That is outside the root: set aside, not a request or a note. The
    // install itself is a request (here refused) for a destination in the
    // project; with the hook off, a note that carries the destination.
    if (await reachable('http://127.0.0.1:48321/lc-s-1.0.0.tgz')) {
      for (const hook of ['on', 'off']) {
        const d = project('url-' + hook)
        const r = await helperRun({
          args: ['install', 'http://127.0.0.1:48321/lc-s-1.0.0.tgz'],
          rootAnswer: (m) => ({ ok: true, root: m.root }),
          cwd: d,
          env: hook === 'off' ? { MATCHAPACK_INJECT_HOOK: 'off' } : {},
        })
        const dests = r.messages.filter(m => m.dest !== undefined).map(m => [m.integrity !== undefined ? 'request' : 'note', m.dest])
        check(`helper: npm install <url>, hook ${hook}: the manifest read is set aside, the install has its destination`,
          JSON.stringify([r.code, r.messages.some(m => m.aside === 'outside-root'), [...new Set(dests.map(JSON.stringify))].map(JSON.parse)]),
          JSON.stringify([0, true, [[hook === 'on' ? 'request' : 'note', path.join(d, 'node_modules', 'lc-s')]]]))
      }
    } else {
      console.log('SKIP helper: npm install <url> (no tarball server on 127.0.0.1:48321; test-inject.sh starts one)')
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true })
  }
}

// Whether the fixtures' tarball server answers, within a second: a stale
// listener that accepts and never answers must not hang the cases (a
// request's 'timeout' event alone does not abort it).
function reachable (url) {
  return new Promise((resolve) => {
    const req = require('http').get(url, (res) => { res.resume(); clearTimeout(t); resolve(res.statusCode === 200) })
    const t = setTimeout(() => { req.destroy(); resolve(false) }, 1000)
    req.on('error', () => { clearTimeout(t); resolve(false) })
  })
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
  // and run time hash the same thing), and it is the SHA-256 of the bytes of
  // pacote's lib/fetcher.js, the file next to the one npm's arborist loads,
  // which holds the FetcherBase class whose work the hook replaces.
  const fp = fingerprint(npmDir)
  const printed = execFileSync(process.execPath, [injectJs, '--fingerprint', npmDir], { encoding: 'utf8' }).trim()
  check('fingerprint: --fingerprint prints what the hook computes', printed, fp)
  const fetcherText = fs.readFileSync(path.join(path.dirname(pacoteMain), 'fetcher.js'))
  check('fingerprint: SHA-256 of pacote\'s lib/fetcher.js', fp, crypto.createHash('sha256').update(fetcherText).digest('hex'))
  check('fingerprint: that file holds the FetcherBase class the hook is installed on',
    fetcherText.toString('utf8').includes(Function.prototype.toString.call(fetcherBase(pacote))), true)

  // The helper itself, started as matchapack starts it (a socket as fd 3),
  // with a stand-in for matchapack that answers its messages.
  await helperCases()

  process.exit(failed ? 1 : 0)
})()

