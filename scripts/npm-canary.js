#!/usr/bin/env node
// The npm release canary's logic (.github/workflows/npm-canary.yml): which
// npm releases to test, what changed in the code the inject hook depends on,
// and what goes into the tracking issue. No dependencies; `inspect` needs
// `node --expose-internals` (acorn, the parser inside Node, for function
// texts; the workflow pins the Node version).
//
//   state   <issue body file>                       the state JSON, from the issue body
//   poll    <state.json> [--head SHA] [--versions a,b] [--current] [--registry-json file]
//           [--node X.Y.Z] [--node-index-json file] [--node-uv-dir dir] [--yarn-registry-json file]
//                                                   the plan JSON: which npm versions to test, and
//                                                   the Node and Yarn 1 releases to test (the
//                                                   release watch; the three files are for tests)
//   inspect <npm package dir> <out.json>            versions, fingerprint, tracked function texts
//   compare <base.json> <new.json> <out dir>        compare.json, one .diff per changed function,
//                                                   changelog.md
//   result  <version dir>                           result.json from the files above + suites.json
//   decide  <results dir> <out dir>                 {"macos": [...]}
//   report  <state.json> <plan.json> <results dir> <out dir> [--run URL]
//                                                   new state.json, body.md, comments/NN.md (when a
//                                                   person should look)
//
// State (one JSON object, kept in the tracking issue's body; see the
// workflow's header for why there): the last successful poll, the month of
// the last monthly run, and per npm major the newest release tested, its
// results, and the last release that passed (the baseline of the next diff).
// `node`: per watched Node line its newest release seen, that release's
// libuv, the release last tested, its results and the hook's facts; `yarn`:
// the newest Yarn 1 release seen and the one last tested (NOTES "The Node and
// Yarn 1 release watch").
'use strict'
const fs = require('node:fs')
const path = require('node:path')
const crypto = require('node:crypto')
const { execFileSync, spawnSync } = require('node:child_process')

const REPO = path.resolve(__dirname, '..')
const STATE_OPEN = '<!-- npm-canary-state'
const STATE_CLOSE = '-->'
const HISTORY_MAX = 60

// ---------------------------------------------------------------------------
// The one coupling with the run-time half (the fingerprint gate in
// src/inject.js and its list); adapt here only:
//   node src/inject.js --fingerprint <npm package dir>
//     prints the hex sha256 of pacote's lib/fetcher.js (the file of its
//     FetcherBase class; until 2026-11-15 the class text alone), as the hook
//     computes it at run time. The reports still call it the "class text";
//   src/npm-fingerprints.json
//     {"fingerprints": {"<hex>": {"releases", "verified_on", "added"}}}, the
//     fingerprints the binary accepts without MATCHAPACK_NPM_FINGERPRINT.
// CANARY_INJECT_JS overrides the script (tests).
// ---------------------------------------------------------------------------
function fingerprintOf (npmDir) {
  const script = process.env.CANARY_INJECT_JS || path.join(REPO, 'src/inject.js')
  const r = spawnSync(process.execPath, [script, '--fingerprint', npmDir], { encoding: 'utf8', timeout: 60000 })
  const out = (r.stdout || '').trim()
  if (r.status === 0 && /^[0-9a-f]{64}$/.test(out)) {
    return { value: out }
  }
  return { value: null, error: `inject.js --fingerprint: status ${r.status}, ${(r.stderr || out).trim().split('\n').slice(-1)[0] || 'no output'}` }
}
function knownFingerprint (fp) {
  if (!fp) {
    return null
  }
  const list = readJson(path.join(REPO, 'src/npm-fingerprints.json'))
  return Object.prototype.hasOwnProperty.call(list.fingerprints || {}, fp)
}
// ---------------------------------------------------------------------------

const die = (msg) => {
  process.stderr.write(msg + '\n')
  process.exit(2)
}
const readJson = (f) => JSON.parse(fs.readFileSync(f, 'utf8'))
const writeJson = (f, v) => fs.writeFileSync(f, JSON.stringify(v, null, 2) + '\n')
const isVersion = (v) => /^\d+\.\d+\.\d+$/.test(v)
const parts = (v) => v.split('.').map(Number)
const cmp = (a, b) => {
  const pa = parts(a)
  const pb = parts(b)
  for (let i = 0; i < 3; i++) {
    if (pa[i] !== pb[i]) {
      return pa[i] - pb[i]
    }
  }
  return 0
}
const major = (v) => parts(v)[0]
const today = () => new Date().toISOString().slice(0, 10)
// Every fetch of the poll. CANARY_NO_NETWORK (the offline tests) makes one a
// failure, so a test that forgot to give a file cannot pass on the network.
const netFetch = (url, init) => {
  if (process.env.CANARY_NO_NETWORK) {
    die(`network disabled (CANARY_NO_NETWORK): ${url}`)
  }
  return fetch(url, init)
}

// The npm majors the inject engine runs on: INJECT_NPM_MAJORS in
// src/wrapper.rs, the one list.
function supportedMajors () {
  const src = fs.readFileSync(path.join(REPO, 'src/wrapper.rs'), 'utf8')
  const m = src.match(/INJECT_NPM_MAJORS:\s*&\[u32\]\s*=\s*&\[([\d,\s]+)\]/)
  if (!m) {
    die('INJECT_NPM_MAJORS not found in src/wrapper.rs')
  }
  return m[1].split(',').map((s) => Number(s.trim())).filter(Number.isInteger)
}

// --- state -----------------------------------------------------------------

function emptyState () {
  return { version: 1, polled: null, monthly: null, lines: {}, history: [], node: null, yarn: null }
}
function stateFromBody (body) {
  const i = body.indexOf(STATE_OPEN)
  if (i < 0) {
    return emptyState()
  }
  const j = body.indexOf(STATE_CLOSE, i)
  if (j < 0) {
    die('the state block in the issue body is not closed')
  }
  // A state that does not parse fails the run: "nothing new" must never come
  // from an unreadable state.
  const s = JSON.parse(body.slice(i + STATE_OPEN.length, j))
  return { ...emptyState(), ...s }
}

// --- poll ------------------------------------------------------------------

async function registryVersions (file) {
  if (file) {
    return Object.keys(readJson(file).versions)
  }
  // The version list, not the dist-tags: `latest` can trail, and a release
  // can be published without a tag moving. Any failure fails the job.
  const res = await netFetch('https://registry.npmjs.org/npm', { headers: { accept: 'application/vnd.npm.install-v1+json' } })
  if (!res.ok) {
    die(`registry: HTTP ${res.status}`)
  }
  const doc = await res.json()
  const vs = Object.keys(doc.versions || {})
  if (vs.length < 100) {
    die(`registry: only ${vs.length} versions of npm, refusing to decide on that`)
  }
  return vs
}

function baselineFor (state, version, all) {
  const m = major(version)
  const line = state.lines[m]
  if (line && line.verified && cmp(line.verified, version) < 0) {
    return line.verified
  }
  // No verified release of this line yet: the release before it in the line,
  // or (x.0.0, a new major) the last verified release of the line below.
  const before = all.filter((v) => major(v) === m && cmp(v, version) < 0)
  if (before.length) {
    return before[before.length - 1]
  }
  const below = Object.keys(state.lines).map(Number).filter((k) => k < m).sort((a, b) => b - a)
  for (const k of below) {
    if (state.lines[k].verified) {
      return state.lines[k].verified
    }
  }
  const older = all.filter((v) => cmp(v, version) < 0)
  return older.length ? older[older.length - 1] : null
}

// A release tested before is tested again when its check was incomplete
// (on the next poll), or when it was red and matchapack changed since.
function retryReason (line, head) {
  if (line.suites === 'incomplete') {
    return 'was incomplete (no comparison); again'
  }
  if (line.suites !== 'pass' && head && line.head !== head) {
    return `was ${line.suites}; matchapack changed since`
  }
  return null
}

async function poll (stateFile, opts) {
  const state = readJson(stateFile)
  const all = (await registryVersions(opts.registryJson)).filter(isVersion).sort(cmp)
  const supported = supportedMajors()
  const date = opts.date || today()
  const month = date.slice(0, 7)
  const todo = []
  const skipped = []
  const add = (version, reason, extra = {}) => {
    if (!todo.some((t) => t.version === version)) {
      todo.push({ version, major: major(version), reason, baseline: baselineFor(state, version, all), newMajor: false, ...extra })
    }
  }
  const newestOf = (m) => all.filter((v) => major(v) === m).pop()

  if (opts.versions) {
    for (const v of opts.versions) {
      if (!isVersion(v) || !all.includes(v)) {
        die(`not a published npm version: ${v}`)
      }
      add(v, 'requested', { newMajor: !supported.includes(major(v)) })
    }
  } else {
    for (const m of supported) {
      const newest = newestOf(m)
      if (!newest) {
        continue
      }
      const line = state.lines[m]
      if (!line) {
        add(newest, 'first run for this line')
        continue
      }
      const unseen = all.filter((v) => major(v) === m && cmp(v, line.newest) > 0)
      // Every unseen release, oldest first; more than three in a day means
      // the canary was off, and the older ones are named instead.
      skipped.push(...unseen.slice(0, -3))
      for (const v of unseen.slice(-3)) {
        add(v, 'new release')
      }
      const again = !unseen.length && retryReason(line, opts.head)
      if (again) {
        add(line.newest, again)
      }
    }
    // A new major: the poll says so and tests its newest release once, with
    // both overrides; enabling it stays a pull request by a person.
    const top = all.length ? major(all[all.length - 1]) : 0
    if (top > Math.max(...supported)) {
      const newest = newestOf(top)
      const line = state.lines[top]
      if (!line || cmp(newest, line.newest) > 0) {
        add(newest, `npm ${top} is not in INJECT_NPM_MAJORS`, { newMajor: true })
      } else {
        const again = retryReason(line, opts.head)
        if (again) {
          add(line.newest, again, { newMajor: true })
        }
      }
    }
  }
  // Once a month, the current release of every line again, so that a broken
  // job is found on a day when nothing depends on it.
  const monthly = opts.current || (!opts.versions && state.monthly !== month)
  if (monthly) {
    for (const m of supported) {
      const newest = newestOf(m)
      if (newest && !todo.some((t) => t.major === m)) {
        add(newest, opts.current ? 'requested: current releases' : 'monthly run')
      }
    }
  }
  const watch = await watchPoll(state, opts, all, supported, date)
  return { date, month: monthly ? month : null, head: opts.head || null, supported, newMajor: all.length && major(all[all.length - 1]) > Math.max(...supported) ? major(all[all.length - 1]) : null, todo, skipped, ...watch }
}

// --- the Node and Yarn 1 release watch -----------------------------------------
// The behaviour the Yarn 1 engine depends on is Node's fs.copyFile, which is
// libuv's uv_fs_copyfile, and libuv changes inside a Node line, not only at a
// major (22.11.0 -> 22.12.0: libuv 1.48.0 -> 1.49.1 made copies keep times).
// So a Node line is tested again when a new major appears or when the libuv
// version of its newest release changes; either runs the Yarn suites (macOS)
// and the npm suites (Linux, btrfs, one job per supported npm line at its
// last verified release) on that Node. A new stable Yarn 1 release runs the
// Yarn suites with it (none expected: the line is frozen at 1.22.22).

// A line is watched while it has had a release in this many days: on
// 2026-11-15 that is 22, 24 and 26 (Node 20's last release was 2026-03-24 and
// 25's 2026-03-31: both lines have ended). A line the state knows that stops
// releasing keeps its row, untested.
const NODE_WATCH_DAYS = 183
// The Yarn 1 the Node items run with, until the watch has verified a newer
// one (.github/workflows/ci.yml pins the same).
const YARN_PINNED = '1.22.22'
const isNodeVersion = (v) => /^v\d+\.\d+\.\d+$/.test(v)
const days = (from, to) => (Date.parse(to) - Date.parse(from)) / 86400000

// Node's releases (nodejs.org/dist/index.json): version and date, oldest
// first. Releases only: the list also has alphas (v27.0.0-alpha.0.1.0 on
// 2026-11-12), which a "newest major" taken from its first entry would read
// as Node 27. Its `uv` field is missing on the newest release of each line
// (2026-11-14), so the libuv version is read from Node's own source (uvOf).
async function nodeReleases (file) {
  let doc
  if (file) {
    doc = readJson(file)
  } else {
    const res = await netFetch('https://nodejs.org/dist/index.json')
    if (!res.ok) {
      die(`nodejs.org: HTTP ${res.status}`)
    }
    doc = await res.json()
  }
  if (!Array.isArray(doc)) {
    die('nodejs.org/dist/index.json is not a list')
  }
  const out = doc.filter((e) => e && typeof e.version === 'string' && isNodeVersion(e.version)).map((e) => ({ version: e.version.slice(1), date: String(e.date || '') }))
  if (out.length < 500) {
    die(`nodejs.org/dist/index.json: only ${out.length} releases, refusing to decide on that`)
  }
  return out.sort((a, b) => cmp(a.version, b.version))
}

// The libuv version of a Node release: deps/uv/include/uv/version.h at its
// tag (in tests: <dir>/v<version>.h). Any failure fails the poll: a libuv
// change must never be missed because a file could not be read.
async function uvOf (version, dir) {
  let text
  if (dir) {
    const f = path.join(dir, `v${version}.h`)
    if (!fs.existsSync(f)) {
      die(`libuv of Node ${version}: ${f} not found`)
    }
    text = fs.readFileSync(f, 'utf8')
  } else {
    const url = `https://raw.githubusercontent.com/nodejs/node/v${version}/deps/uv/include/uv/version.h`
    const res = await netFetch(url)
    if (!res.ok) {
      die(`libuv of Node ${version}: ${url}: HTTP ${res.status}`)
    }
    text = await res.text()
  }
  const n = (k) => (text.match(new RegExp(`#define UV_VERSION_${k}\\s+(\\d+)`)) || [])[1]
  const v = [n('MAJOR'), n('MINOR'), n('PATCH')]
  if (v.some((x) => x === undefined)) {
    die(`libuv of Node ${version}: version.h has no UV_VERSION_MAJOR, _MINOR and _PATCH`)
  }
  return v.join('.')
}

// The stable Yarn 1 releases (1.x.y), oldest first.
async function yarn1Versions (file) {
  let vs
  if (file) {
    vs = Object.keys(readJson(file).versions || {})
  } else {
    const res = await netFetch('https://registry.npmjs.org/yarn', { headers: { accept: 'application/vnd.npm.install-v1+json' } })
    if (!res.ok) {
      die(`registry (yarn): HTTP ${res.status}`)
    }
    vs = Object.keys((await res.json()).versions || {})
  }
  if (vs.length < 50) {
    die(`registry: only ${vs.length} versions of yarn, refusing to decide on that`)
  }
  return vs.filter((v) => /^1\.\d+\.\d+$/.test(v)).sort(cmp)
}

// A watched release tested before is tested again as an npm release is
// (retryReason): incomplete, or red with matchapack changed since. Never a
// line only recorded (no test yet).
const watchRetry = (line, head) => (line && line.tested ? retryReason(line, head) : null)

async function watchPoll (state, opts, npmAll, supported, date) {
  const releases = await nodeReleases(opts.nodeIndexJson)
  const lines = (state.node && state.node.lines) || {}
  const majors = [...new Set(releases.map((r) => major(r.version)))].sort((a, b) => a - b)
  const newestOf = (m) => releases.filter((r) => major(r.version) === m).pop()
  const watched = majors.filter((m) => days(newestOf(m).date, date) <= NODE_WATCH_DAYS)
  const top = majors[majors.length - 1]
  const todo = []
  const record = []
  const add = (t) => {
    if (!todo.some((x) => x.node === t.node)) {
      todo.push({ npm: true, yarn: true, newMajor: false, uvBefore: null, ...t })
    }
  }
  if (opts.node) {
    if (!isNodeVersion(`v${opts.node}`) || !releases.some((r) => r.version === opts.node)) {
      die(`not a Node release: ${opts.node}`)
    }
    const m = major(opts.node)
    add({ node: opts.node, line: m, uv: await uvOf(opts.node, opts.nodeUvDir), reason: 'requested', newMajor: !lines[m] })
  }
  if (!state.node) {
    // First run: record the watched lines; test only the newest once (Node
    // 22 and 24 run in pull-request CI already; 26 in no workflow).
    for (const m of watched) {
      const r = newestOf(m)
      const uv = await uvOf(r.version, opts.nodeUvDir)
      if (m === top) {
        add({ node: r.version, line: m, uv, reason: `first run: Node ${m}, the newest line` })
      } else {
        record.push({ line: m, newest: r.version, uv })
      }
    }
  } else {
    // Every watched line the state does not know (more than one new major
    // when the canary was off for a while; a line missing from an edited
    // state): tested, and known from then on.
    const known = Object.keys(lines).map(Number)
    const maxKnown = Math.max(0, ...known)
    for (const m of [...new Set([...watched, top])].filter((k) => !known.includes(k)).sort((a, b) => a - b)) {
      const r = newestOf(m)
      add({ node: r.version, line: m, uv: await uvOf(r.version, opts.nodeUvDir), reason: m > maxKnown ? `Node ${m} is new` : `Node ${m} is not in the state`, newMajor: m > maxKnown })
    }
    for (const m of known.filter((k) => watched.includes(k))) {
      const line = lines[m]
      const r = newestOf(m)
      if (!line.newest || cmp(r.version, line.newest) > 0) {
        const uv = await uvOf(r.version, opts.nodeUvDir)
        // A line whose last test is to be repeated (incomplete, or red with
        // matchapack changed) tests the new release instead.
        const again = watchRetry(line, opts.head)
        if (uv !== line.uv) {
          add({ node: r.version, line: m, uv, uvBefore: line.uv || null, reason: `libuv ${line.uv || '?'} → ${uv} in Node ${m}` })
        } else if (again) {
          add({ node: r.version, line: m, uv, reason: `${line.tested} ${again}: the newest release instead`, newMajor: !!line.newMajor })
        } else {
          record.push({ line: m, newest: r.version, uv })
        }
      } else {
        const again = watchRetry(line, opts.head)
        if (again) {
          add({ node: line.tested, line: m, uv: line.uvTested || line.uv, reason: again, newMajor: !!line.newMajor })
        }
      }
    }
  }

  const yv = await yarn1Versions(opts.yarnRegistryJson)
  const ynew = yv[yv.length - 1]
  const y = state.yarn
  const yarn = { todo: [], record: null }
  if (!y) {
    // First run: the current release, not tested (pull-request CI runs it).
    yarn.record = { newest: ynew }
  } else if (cmp(ynew, y.newest) > 0) {
    yarn.todo.push({ yarn: ynew, reason: `Yarn ${ynew} is new` })
  } else {
    const again = watchRetry(y, opts.head)
    if (again) {
      yarn.todo.push({ yarn: y.tested, reason: again })
    }
  }

  // The jobs. node-linux: per Node item, each supported npm line at the
  // release the npm canary verified last (else its newest). node-macos: per
  // Node item with the Yarn 1 the watch verified last, per Yarn item on the
  // workflow's pinned Node (node null).
  const yarnBase = (y && y.verified) || YARN_PINNED
  const nodeLinux = []
  for (const t of todo.filter((x) => x.npm)) {
    for (const m of supported) {
      const v = (state.lines[m] && state.lines[m].verified) || npmAll.filter((x) => major(x) === m).pop()
      if (v) {
        nodeLinux.push({ key: `node${t.node}-npm${v}`, node: t.node, npm: v })
      }
    }
  }
  const nodeMacos = [
    ...todo.filter((x) => x.yarn).map((t) => ({ key: `node${t.node}`, node: t.node, yarn: yarnBase })),
    ...yarn.todo.map((t) => ({ key: `yarn${t.yarn}`, node: null, yarn: t.yarn })),
  ]
  return { node: { todo, record, watched }, yarn, nodeLinux, nodeMacos }
}

// --- inspect ---------------------------------------------------------------

// arborist's methods around the call and the life of the destination
// (review of 2026-11-14, section 3: diagram 1 and the "A2" surface).
const CHAIN = ['extractOrLink', 'reifyNode', 'handleOptionalFailure', 'unpackNewModules', 'loadBundlesAndUpdateTrees', 'loadShrinkwrapsAndUpdateTrees', 'createSparseTree', 'rollbackCreateSparseTree', 'moveBackRetiredUnchanged', 'moveContents', 'validateNodeModules', 'reifyPackages', 'removeTrash', 'applyPatch', 'renamePath']
const DEPS = ['pacote', 'tar', 'cacache', '@npmcli/arborist', 'bin-links', '@npmcli/installed-package-contents']

function loadAcorn () {
  try {
    return require('internal/deps/acorn/acorn/dist/acorn')
  } catch {
    die('acorn not available: run with node --expose-internals')
  }
}

function pkgDir (name, from) {
  try {
    return path.dirname(require.resolve(`${name}/package.json`, { paths: [from] }))
  } catch {
    // A package whose `exports` hides package.json: walk up from its main.
    let d = path.dirname(require.resolve(name, { paths: [from] }))
    while (d !== path.dirname(d)) {
      const p = path.join(d, 'package.json')
      if (fs.existsSync(p) && JSON.parse(fs.readFileSync(p, 'utf8')).name === name) {
        return d
      }
      d = path.dirname(d)
    }
    throw new Error(`${name} not found from ${from}`)
  }
}

// Every named function in a file (class methods by their base name, function
// declarations, `const f = function/arrow`, `A.b = function/arrow`), with
// its exact source text; and the text of each class by name.
function functionsOf (acorn, src) {
  const ast = acorn.parse(src, { ecmaVersion: 'latest', sourceType: 'script', allowHashBang: true, allowReturnOutsideFunction: true })
  const fns = new Map()
  const classes = new Map()
  const add = (name, node) => {
    let n = name
    let i = 2
    while (fns.has(n)) {
      n = `${name}~${i++}`
    }
    fns.set(n, src.slice(node.start, node.end))
  }
  const base = (raw) => raw.replace(/^\[/, '').replace(/\]$/, '').replace(/^_\./, '').replace(/^[_#]+/, '')
  const isFn = (n) => n && (n.type === 'FunctionExpression' || n.type === 'ArrowFunctionExpression')
  const walk = (node) => {
    if (!node || typeof node.type !== 'string') {
      return
    }
    if ((node.type === 'ClassDeclaration' || node.type === 'ClassExpression') && node.id) {
      classes.set(node.id.name, src.slice(node.start, node.end))
    }
    if (node.type === 'MethodDefinition') {
      let raw
      if (node.key.type === 'PrivateIdentifier') {
        raw = '#' + node.key.name
      } else if (node.computed) {
        raw = '[' + src.slice(node.key.start, node.key.end) + ']'
      } else {
        raw = node.key.name || String(node.key.value)
      }
      let name = base(raw)
      if (node.kind === 'get' || node.kind === 'set') {
        name += ':' + node.kind
      }
      add(node.static ? 'static:' + name : name, node)
    } else if (node.type === 'FunctionDeclaration' && node.id) {
      add(node.id.name, node)
    } else if (node.type === 'VariableDeclarator' && isFn(node.init) && node.id.type === 'Identifier') {
      add(node.id.name, node)
    } else if (node.type === 'AssignmentExpression' && isFn(node.right) && node.left.type === 'MemberExpression') {
      add(src.slice(node.left.start, node.left.end), node)
    }
    for (const k of Object.keys(node)) {
      const v = node[k]
      if (Array.isArray(v)) {
        v.forEach(walk)
      } else if (v && typeof v.type === 'string') {
        walk(v)
      }
    }
  }
  walk(ast)
  return { fns, classes }
}

function inspect (npmDir, out) {
  const acorn = loadAcorn()
  npmDir = path.resolve(npmDir)
  const npmVersion = readJson(path.join(npmDir, 'package.json')).version
  const arb = pkgDir('@npmcli/arborist', npmDir)
  const pacote = pkgDir('pacote', arb)
  const from = { pacote: arb, tar: pacote, cacache: pacote, '@npmcli/arborist': npmDir, 'bin-links': arb, '@npmcli/installed-package-contents': pacote }
  const deps = {}
  for (const d of DEPS) {
    try {
      deps[d] = readJson(path.join(pkgDir(d, from[d]), 'package.json')).version
    } catch {
      deps[d] = null
    }
  }
  const tracked = {}
  const read = (f) => fs.readFileSync(f, 'utf8')
  const fetcher = functionsOf(acorn, read(path.join(pacote, 'lib/fetcher.js')))
  tracked['pacote/lib/fetcher.js: class FetcherBase'] = fetcher.classes.get('FetcherBase') || null
  const reify = functionsOf(acorn, read(path.join(arb, 'lib/arborist/reify.js')))
  for (const [n, t] of reify.fns) {
    if (CHAIN.includes(n.replace(/~\d+$/, ''))) {
      tracked[`arborist/lib/arborist/reify.js: ${n}`] = t
    }
  }
  const rebuild = functionsOf(acorn, read(path.join(arb, 'lib/arborist/rebuild.js')))
  for (const [n, t] of rebuild.fns) {
    // Not the mixin wrapper (`module.exports = cls => class ...`): it is the
    // whole file again.
    if (n !== 'module.exports') {
      tracked[`arborist/lib/arborist/rebuild.js: ${n}`] = t
    }
  }
  const ci = functionsOf(acorn, read(path.join(npmDir, 'lib/commands/ci.js')))
  tracked['npm/lib/commands/ci.js: exec'] = ci.fns.get('exec') || null
  const eol = tracked['arborist/lib/arborist/reify.js: extractOrLink']
  const fp = fingerprintOf(npmDir)
  const result = {
    npm: npmVersion,
    deps,
    fingerprint: fp.value,
    fingerprintError: fp.error || null,
    known: knownFingerprint(fp.value),
    // The first of the review's three questions, answered from the text:
    // every call is a bare `await pacote.extract(...)` statement, so arborist
    // never reads what the (replaced) extract resolves to.
    bareAwait: eol ? (eol.match(/pacote\.extract\(/g) || []).length > 0 && (eol.match(/pacote\.extract\(/g) || []).length === (eol.match(/^\s*await pacote\.extract\(/gm) || []).length : null,
    sha: Object.fromEntries(Object.entries(tracked).map(([k, v]) => [k, v === null ? null : crypto.createHash('sha256').update(v).digest('hex').slice(0, 16)])),
    tracked,
  }
  writeJson(out, result)
  console.log(`npm ${npmVersion}: pacote ${deps.pacote}, arborist ${deps['@npmcli/arborist']}, ${Object.keys(tracked).length} tracked functions, fingerprint ${fp.value ? fp.value.slice(0, 12) + (result.known ? ' (known)' : result.known === false ? ' (NEW)' : ' (no list)') : 'unavailable: ' + fp.error}`)
}

// --- compare ---------------------------------------------------------------

function udiff (a, b, label, dir) {
  const fa = path.join(dir, '.a')
  const fb = path.join(dir, '.b')
  fs.writeFileSync(fa, (a || '') + '\n')
  fs.writeFileSync(fb, (b || '') + '\n')
  const r = spawnSync('diff', ['-u', '--label', `${label} (base)`, '--label', `${label} (new)`, fa, fb], { encoding: 'utf8' })
  fs.rmSync(fa)
  fs.rmSync(fb)
  return r.stdout
}

async function changelog (base, ver) {
  // npm's CHANGELOG.md at the new release's tag: the sections after the
  // baseline, lines that name the packages the hook depends on. A fetch
  // failure is noted, not fatal.
  const url = `https://raw.githubusercontent.com/npm/cli/v${ver}/CHANGELOG.md`
  const compareUrl = `https://github.com/npm/cli/compare/v${base}...v${ver}`
  try {
    const res = await fetch(url)
    if (!res.ok) {
      return { url, compareUrl, lines: null, error: `HTTP ${res.status}` }
    }
    const text = await res.text()
    const lines = []
    let inRange = false
    for (const l of text.split('\n')) {
      const h = l.match(/^##\s+\[?(\d+\.\d+\.\d+)\]?/)
      if (h) {
        inRange = cmp(h[1], ver) <= 0 && cmp(h[1], base) > 0 && major(h[1]) === major(ver)
        if (inRange) {
          lines.push(`**${h[1]}**`)
        }
        continue
      }
      if (inRange && /arborist|pacote|\btar\b|cacache|bin-links|reify|installed-package-contents/i.test(l)) {
        // For a comment in this repository: no @mention may notify anyone
        // (a zero-width space after @), and a bare #N is npm/cli's, not ours.
        lines.push(l.trim().replace(/@/g, '@\u200b').replace(/(^|[\s(])#(\d+)/g, '$1npm/cli#$2'))
      }
    }
    return { url, compareUrl, lines: lines.filter((l, i, a) => !(l.startsWith('**') && (i === a.length - 1 || a[i + 1].startsWith('**')))).slice(0, 60) }
  } catch (e) {
    return { url, compareUrl, lines: null, error: e.message }
  }
}

async function compare (baseFile, newFile, outDir) {
  const A = readJson(baseFile)
  const B = readJson(newFile)
  fs.mkdirSync(path.join(outDir, 'diffs'), { recursive: true })
  const names = [...new Set([...Object.keys(A.tracked), ...Object.keys(B.tracked)])].sort()
  const items = []
  for (const n of names) {
    const a = A.tracked[n] ?? null
    const b = B.tracked[n] ?? null
    let status = 'same'
    if (a === null && b !== null) {
      status = 'added'
    } else if (a !== null && b === null) {
      status = 'removed'
    } else if (a !== b) {
      status = 'changed'
    }
    const item = { name: n, status, class: n.endsWith('class FetcherBase') }
    if (status !== 'same') {
      const d = udiff(a, b, n, outDir)
      item.file = 'diffs/' + n.replace(/[^\w.-]+/g, '_') + '.diff'
      item.lines = d.split('\n').filter((l) => /^[-+](?![-+]{2} )/.test(l)).length
      fs.writeFileSync(path.join(outDir, item.file), d)
    }
    items.push(item)
  }
  const deps = Object.fromEntries(DEPS.map((d) => [d, { base: A.deps[d], new: B.deps[d] }]))
  const cl = process.env.CANARY_NO_NETWORK ? { lines: null, error: 'not fetched (CANARY_NO_NETWORK)' } : await changelog(A.npm, B.npm)
  const result = {
    base: A.npm,
    new: B.npm,
    deps,
    classChanged: items.some((i) => i.class && i.status !== 'same'),
    callerChanged: items.some((i) => !i.class && i.status !== 'same'),
    items,
    changelog: cl,
  }
  writeJson(path.join(outDir, 'compare.json'), result)
  const changed = items.filter((i) => i.status !== 'same')
  console.log(`${A.npm} -> ${B.npm}: ${changed.length} of ${items.length} tracked functions differ${changed.length ? ': ' + changed.map((i) => `${i.name} (${i.status}${i.lines ? ', ' + i.lines + ' lines' : ''})`).join('; ') : ''}`)
}

// --- result / decide -------------------------------------------------------

// One version's directory -> result.json: the plan item, the suites, the
// fingerprint, the comparison. A missing piece is recorded as missing.
function result (dir) {
  const opt = (f) => (fs.existsSync(path.join(dir, f)) ? readJson(path.join(dir, f)) : null)
  const item = opt('item.json') || {}
  const suites = opt('suites.json')
  const insp = opt('inspect-new.json')
  const cmpr = opt('compare.json')
  const r = {
    version: item.version || (insp && insp.npm),
    baseline: item.baseline || null,
    reason: item.reason || null,
    newMajor: !!item.newMajor,
    os: suites ? suites.os : null,
    suites: suites ? suites.suites : null,
    suitesPass: suites ? Object.values(suites.suites).every((s) => s.status === 0) : false,
    fingerprint: insp ? insp.fingerprint : null,
    fingerprintError: insp ? insp.fingerprintError : 'not inspected',
    known: insp ? insp.known : null,
    bareAwait: insp ? insp.bareAwait : null,
    compare: cmpr ? { classChanged: cmpr.classChanged, callerChanged: cmpr.callerChanged, deps: cmpr.deps, changed: cmpr.items.filter((i) => i.status !== 'same'), changelog: cmpr.changelog, base: cmpr.base } : null,
  }
  writeJson(path.join(dir, 'result.json'), r)
  return r
}

function loadResults (dir) {
  const out = { linux: {}, macos: {}, nodelinux: {}, nodemacos: {} }
  if (!dir || !fs.existsSync(dir)) {
    return out
  }
  for (const d of fs.readdirSync(dir)) {
    // The release watch's jobs (canary-nodelinux-<key>, canary-nodemacos-<key>):
    // their suites.json, or null when the job left none.
    const w = d.match(/^canary-(nodelinux|nodemacos)-(.+)$/)
    if (w) {
      const f = path.join(dir, d, 'suites.json')
      out[w[1]][w[2]] = fs.existsSync(f) ? readJson(f) : null
      continue
    }
    const m = d.match(/^canary-(linux|macos)-(\d+\.\d+\.\d+)$/)
    if (!m) {
      continue
    }
    const full = path.join(dir, d)
    if (m[1] === 'linux') {
      const f = path.join(full, 'result.json')
      out.linux[m[2]] = fs.existsSync(f) ? { ...readJson(f), dir: full } : null
    } else {
      const f = path.join(full, 'suites.json')
      out.macos[m[2]] = fs.existsSync(f) ? readJson(f) : null
    }
  }
  return out
}

// macOS (10x) only when the pacote class text is new (or cannot be told) and
// for a new major: the one rule, for decide (which jobs run) and report
// (whether a missing macOS result is a failure).
const needsMacos = (r, item) => !!r && (r.known !== true || !!r.newMajor || !!(item && item.newMajor))

function decide (resultsDir, outDir) {
  const { linux } = loadResults(resultsDir)
  const macos = []
  for (const [v, r] of Object.entries(linux)) {
    if (r && needsMacos(r)) {
      macos.push(v)
    }
  }
  fs.mkdirSync(outDir, { recursive: true })
  const d = { macos: macos.sort(cmp) }
  writeJson(path.join(outDir, 'decide.json'), d)
  return d
}

// --- report ----------------------------------------------------------------

const yesno = (b) => (b === true ? 'yes' : b === false ? 'no' : '?')
function suiteCell (s) {
  if (!s) {
    return 'no result'
  }
  return Object.entries(s).map(([n, x]) => `${n} ${x.status === 0 ? 'pass' : 'FAIL'} (${x.pass}/${x.pass + x.fail})`).join(', ')
}

function report (stateFile, planFile, resultsDir, outDir, opts) {
  const state = readJson(stateFile)
  const plan = readJson(planFile)
  const res = loadResults(resultsDir)
  const run = opts.run || ''
  const rows = []
  let attention = false
  let red = false
  const sections = []
  for (const t of plan.todo) {
    const r = res.linux[t.version]
    const mac = Object.prototype.hasOwnProperty.call(res.macos, t.version) ? res.macos[t.version] : undefined
    const linuxOk = !!(r && r.suitesPass)
    // macOS required (needsMacos) but no result: the job failed before its
    // upload. A failure, never a skip.
    const macRequired = needsMacos(r, t)
    const macMissing = macRequired && !mac
    const macOk = macMissing ? false : mac === undefined ? null : !!(mac && Object.values(mac.suites).every((s) => s.status === 0))
    // A planned comparison (a baseline was named) that left no result: the
    // check is incomplete; the next poll runs the release again.
    const compareMissing = !!(r && t.baseline && !r.compare)
    let suites = !r ? 'error' : linuxOk && macOk !== false ? 'pass' : 'fail'
    if (suites === 'pass' && compareMissing) {
      suites = 'incomplete'
    }
    const classText = !r ? '?' : r.known === true ? 'known' : r.known === false ? 'new' : 'unknown'
    const caller = !r ? '?' : !r.compare ? (compareMissing ? 'missing' : '?') : r.compare.callerChanged ? 'changed' : 'same'
    const classChanged = !!(r && r.compare && r.compare.classChanged)
    // The release this line verified last, tested again (the monthly run, a
    // retest by hand): its diff against the release before was reported when
    // it was first tested, so a changed function asks for nobody again; a
    // failure or a new class text still does. Only that release: an older
    // one (a skipped release requested by hand) may never have been tested.
    const prior = state.lines[t.major] || {}
    const reviewed = prior.verified === t.version
    let action = 'none'
    if (suites !== 'pass' || classText !== 'known' || t.newMajor) {
      action = 'action needed'
    } else if (!reviewed && (caller === 'changed' || classChanged)) {
      action = 'read the diff'
    }
    const row = { date: plan.date, version: t.version, reason: t.reason, baseline: (r && r.compare && r.compare.base) || t.baseline, suites, linux: r ? (linuxOk ? 'pass' : 'fail') : 'error', macos: macMissing ? 'missing' : macOk === null ? '-' : macOk ? 'pass' : 'fail', classText, fingerprint: r ? r.fingerprint : null, caller, action, head: plan.head, run }
    rows.push(row)
    // The line's row: the newest release tested, unless this was an older
    // one (several releases in one day, or a requested version).
    const line = state.lines[t.major] || {}
    if (!line.newest || cmp(t.version, line.newest) >= 0) {
      state.lines[t.major] = { ...line, newest: t.version, checked: plan.date, suites, classText, caller, action, head: plan.head, run, newMajor: !!t.newMajor }
    }
    if (suites === 'pass' && (!line.verified || cmp(t.version, line.verified) >= 0)) {
      state.lines[t.major].verified = t.version
    }
    if (suites !== 'pass') {
      red = true
    }
    if (action !== 'none') {
      attention = true
      sections.push(versionSection(t, r, macMissing ? null : mac, row).join('\n'))
    }
  }
  const w = watchReport(state, plan, res, run)
  rows.push(...w.rows)
  sections.push(...w.sections)
  red = red || w.red
  attention = attention || w.attention
  state.polled = plan.date
  if (plan.month) {
    state.monthly = plan.month
  }
  state.history = [...rows.slice().reverse(), ...(state.history || [])].slice(0, HISTORY_MAX)
  fs.mkdirSync(outDir, { recursive: true })
  writeJson(path.join(outDir, 'state.json'), state)
  fs.writeFileSync(path.join(outDir, 'body.md'), body(state, plan))
  if (attention || plan.skipped.length) {
    const intro = []
    if (plan.skipped.length) {
      intro.push(`Not tested (more than three new releases of a line since the last run): ${plan.skipped.join(', ')}. Run the workflow with \`versions\` to test them.`, '')
    }
    // comments/NN.md, each under GitHub's comment limit; the workflow posts
    // them in order, before it writes the state.
    const dir = path.join(outDir, 'comments')
    fs.mkdirSync(dir, { recursive: true })
    packComments(plan, run, intro.join('\n'), sections).forEach((c, k) => fs.writeFileSync(path.join(dir, `${String(k + 1).padStart(2, '0')}.md`), c))
  }
  // notify: the workflow fails the run (its notice to the owner) when a
  // release is red or the comment asks for a person.
  const notify = red || attention || plan.skipped.length > 0
  writeJson(path.join(outDir, 'flags.json'), { red, attention, skipped: plan.skipped.length, notify })
  console.log(`${rows.length} versions reported; ${attention ? 'a comment asks for a person' : 'nothing for a person'}${red ? '; at least one red' : ''}`)
  return { red, attention }
}

// --- report: the Node and Yarn 1 release watch -----------------------------------

const suitesPass = (s) => !!(s && s.suites && Object.keys(s.suites).length && Object.values(s.suites).every((x) => x.status === 0))
const FACT_KEYS = ['empty_atime', 'empty_mtime', 'atime', 'mtime', 'group']
const sameFacts = (a, b) => FACT_KEYS.every((k) => (a ? a[k] : undefined) === (b ? b[k] : undefined))
function factsText (f) {
  if (!f) {
    return '-'
  }
  const kept = FACT_KEYS.filter((k) => f[k] === true)
  return kept.length ? `keeps ${kept.join(', ')}` : 'keeps nothing'
}

// Each job a watch item needed: its result, or a failure when it left none
// (the job failed before its upload: never a pass).
function jobResults (entries, results) {
  return entries.map((e) => ({ e, s: Object.prototype.hasOwnProperty.call(results, e.key) ? results[e.key] : null }))
}
function verdict (jobs) {
  if (jobs.some((j) => j.s && !suitesPass(j.s))) {
    return 'fail'
  }
  return jobs.some((j) => !j.s) ? 'incomplete' : 'pass'
}
const jobCell = (jobs) => (!jobs.length ? '-' : jobs.map((j) => (j.s ? (suitesPass(j.s) ? 'pass' : 'FAIL') : 'missing')).join(', '))

function watchReport (state, plan, res, run) {
  const out = { rows: [], sections: [], red: false, attention: false }
  const N = plan.node || { todo: [], record: [] }
  const Y = plan.yarn || { todo: [], record: null }
  if (plan.node) {
    state.node = state.node || { lines: {} }
  }
  // Releases seen with the libuv of the line's last: only recorded.
  for (const r of N.record) {
    const l = state.node.lines[r.line] || {}
    state.node.lines[r.line] = { ...l, newest: r.newest, uv: r.uv, checked: plan.date }
  }
  for (const t of N.todo) {
    const lin = jobResults((plan.nodeLinux || []).filter((e) => e.node === t.node), res.nodelinux)
    const mac = jobResults((plan.nodeMacos || []).filter((e) => e.node === t.node), res.nodemacos)
    const m = mac[0] && mac[0].s
    const facts = (m && m.facts) || null
    // The facts are part of the check: a green macOS result without them
    // is incomplete (not verified, tested again on the next poll).
    let suites = verdict([...lin, ...mac])
    if (suites === 'pass' && t.yarn && !facts) {
      suites = 'incomplete'
    }
    const prev = state.node.lines[t.line] || {}
    const factsChanged = !!(prev.facts && facts && !sameFacts(prev.facts, facts))
    // A new major always asks for a person (enabling it in pull-request CI
    // is a person's change); a libuv change only when something differs.
    const action = t.newMajor || suites !== 'pass' || factsChanged || (t.yarn && !facts) ? 'action needed' : 'none'
    const newer = !prev.newest || cmp(t.node, prev.newest) >= 0
    // An older release than the one the line last tested (one requested by
    // hand): its row and comment only; the line keeps the newer release's
    // results (a red one stays red, and is retried).
    const older = !!prev.tested && cmp(t.node, prev.tested) < 0
    if (!older) {
      state.node.lines[t.line] = {
        ...prev,
        newest: newer ? t.node : prev.newest,
        uv: newer ? (m && m.uv) || t.uv : prev.uv,
        tested: t.node,
        uvTested: (m && m.uv) || t.uv,
        checked: plan.date,
        suites,
        npm: jobCell(lin),
        yarn: jobCell(mac),
        facts: facts || prev.facts || null,
        action,
        head: plan.head,
        run,
        newMajor: !!t.newMajor,
      }
      if (suites === 'pass' && (!prev.verified || cmp(t.node, prev.verified) >= 0)) {
        state.node.lines[t.line].verified = t.node
      }
    }
    const row = { date: plan.date, version: `Node ${t.node}`, reason: t.reason, linux: jobCell(lin), macos: jobCell(mac), classText: '-', caller: '-', action, head: plan.head, run }
    out.rows.push(row)
    out.red = out.red || suites !== 'pass'
    if (action !== 'none') {
      out.attention = true
      out.sections.push(nodeSection(t, lin, mac, prev, facts, factsChanged, suites, action).join('\n'))
    }
  }
  if (Y.record) {
    state.yarn = { ...(state.yarn || {}), newest: Y.record.newest, checked: plan.date }
  }
  for (const t of Y.todo) {
    const mac = jobResults((plan.nodeMacos || []).filter((e) => e.yarn === t.yarn && e.node === null), res.nodemacos)
    const suites = verdict(mac)
    const m = mac[0] && mac[0].s
    const prev = state.yarn || {}
    // A new Yarn 1 release always asks for a person: none is expected (the
    // line is frozen), and the engine has never seen its code.
    const isNew = !prev.newest || cmp(t.yarn, prev.newest) > 0
    const action = isNew || suites !== 'pass' ? 'action needed' : 'none'
    state.yarn = { ...prev, newest: isNew ? t.yarn : prev.newest, tested: t.yarn, checked: plan.date, suites, node: m ? m.node : null, facts: m ? m.facts || null : null, action, head: plan.head, run }
    if (suites === 'pass') {
      state.yarn.verified = t.yarn
    }
    out.rows.push({ date: plan.date, version: `Yarn ${t.yarn}`, reason: t.reason, linux: '-', macos: jobCell(mac), classText: '-', caller: '-', action, head: plan.head, run })
    out.red = out.red || suites !== 'pass'
    if (action !== 'none') {
      out.attention = true
      const L = [`### Yarn ${t.yarn} (${t.reason}): ${action}`, '']
      L.push(`- Yarn suites (test-yarn1.sh, macOS${m && m.node ? `, Node ${m.node}` : ''}): ${jobCell(mac)}`)
      for (const j of mac) {
        for (const f of (j.s && j.s.suites ? Object.values(j.s.suites).flatMap((x) => x.failed || []) : [])) {
          L.push(`  - \`${f.replace(/`/g, "'")}\``)
        }
      }
      L.push(`- A new Yarn 1 release: nothing in the engine knows its code (NOTES "Yarn 1 commands", "Part 4"). Read its changelog; when the suites pass, move the pin in .github/workflows/ci.yml.`, '')
      out.sections.push(L.join('\n'))
    }
  }
  return out
}

function nodeSection (t, lin, mac, prev, facts, factsChanged, suites, action) {
  const out = [`### Node ${t.node} (${t.reason}): ${action}`, '']
  out.push(`- libuv: ${t.uvBefore ? `${t.uvBefore} → ` : ''}${(mac[0] && mac[0].s && mac[0].s.uv) || t.uv}`)
  out.push(`- Yarn suites (test-yarn1.sh, macOS): ${jobCell(mac)}`)
  out.push(`- npm suites (test-inject.sh and verify-npm.sh, Linux on btrfs): ${lin.map((j) => `npm ${j.e.npm} ${j.s ? (suitesPass(j.s) ? 'pass' : 'FAIL') : 'missing'}`).join(', ') || '-'}`)
  for (const j of [...mac, ...lin]) {
    for (const f of (j.s && j.s.suites ? Object.values(j.s.suites).flatMap((x) => x.failed || []) : [])) {
      out.push(`  - \`${f.replace(/`/g, "'")}\``)
    }
  }
  out.push(`- What Node's fs.copyFile keeps (the Yarn hook's probe): ${factsText(facts)}${factsChanged ? ` — **changed** (was: ${factsText(prev.facts)})` : ''}`)
  if (t.newMajor) {
    out.push(`- Node ${t.line} is a new major: adding it to pull-request CI is a person's change.`)
  }
  if (suites !== 'pass') {
    out.push(`- Not verified (${suites}); the next poll tests it again when matchapack changes${suites === 'incomplete' ? ' (incomplete: on the next poll)' : ''}.`)
  }
  out.push('')
  return out
}

// GitHub refuses a comment over 65,536 characters; keep well under it.
const COMMENT_MAX = 60000
function packComments (plan, run, intro, sections) {
  const title = (k, n) => `## npm canary, ${plan.date}${n > 1 ? ` (${k} of ${n})` : ''}${run ? ` ([run](${run}))` : ''}\n\n`
  const foot = '\n---\n_Posted by the npm-canary workflow._\n'
  const room = COMMENT_MAX - 200 - foot.length
  const cut = (s) => (s.length <= room ? s : s.slice(0, room - 200) + `\n\n… (cut to fit a comment; the whole report is in the run's artifacts${run ? `: ${run}` : ''})\n`)
  const groups = []
  let cur = intro ? intro + '\n' : ''
  for (const s of sections.map(cut)) {
    if (cur && cur.length + s.length + 1 > room) {
      groups.push(cur)
      cur = ''
    }
    cur += s + '\n'
  }
  if (cur || !groups.length) {
    groups.push(cur)
  }
  return groups.map((g, k) => title(k + 1, groups.length) + g + foot)
}

function fence (text, max) {
  let t = text
  if (t.length > max) {
    t = t.slice(0, max) + '\n… (cut here; the whole diff is in the run\'s artifacts)'
  }
  return ['```diff', t.replace(/```/g, '`​``'), '```']
}

function versionSection (t, r, mac, row) {
  const out = [`### npm ${t.version} (${t.reason}): ${row.action}`, '']
  if (!r) {
    out.push('The Linux job left no result; see the run.', '')
    return out
  }
  const c = r.compare
  out.push(`- Suites, Linux on btrfs: ${suiteCell(r.suites)}`)
  if (mac !== undefined) {
    out.push(`- Suites, macOS: ${mac ? suiteCell(mac.suites) : 'required (class text not listed, or a new major), but the job left no result: counted as a failure'}`)
  }
  for (const s of [...(r.suites ? Object.values(r.suites) : []), ...(mac && mac.suites ? Object.values(mac.suites) : [])]) {
    for (const f of s.failed || []) {
      out.push(`  - \`${f.replace(/`/g, "'")}\``)
    }
  }
  out.push(`- pacote class text: ${row.classText}${r.fingerprint ? ` (\`${r.fingerprint.slice(0, 16)}…\`)` : ''}${r.fingerprintError ? `; fingerprint: ${r.fingerprintError}` : ''}`)
  if (t.newMajor) {
    out.push(`- npm ${t.major} is a new major: tested with MATCHAPACK_NPM_VERSIONS=any and MATCHAPACK_NPM_FINGERPRINT=any. Enabling it is a pull request by a person.`)
  }
  if (!c && t.baseline) {
    out.push(`- Comparison with ${t.baseline}: missing (the baseline install or the inspection failed; see the run). Not verified; the next poll runs this release again.`)
  }
  if (c) {
    out.push(`- Compared with ${c.base}: ${c.changed.length ? c.changed.map((i) => `\`${i.name}\` ${i.status}${i.lines ? ` (${i.lines} lines)` : ''}`).join(', ') : 'no tracked function changed'}`)
    const dv = Object.entries(c.deps).filter(([, v]) => v.base !== v.new)
    out.push(`- Packages: ${dv.length ? dv.map(([k, v]) => `${k} ${v.base} → ${v.new}`).join(', ') : 'pacote, tar, cacache, arborist, bin-links and installed-package-contents unchanged'}`)
    out.push(`- Is \`pacote.extract(…)\` still a bare \`await\` in \`#extractOrLink\`? ${yesno(r.bareAwait)}`)
    out.push('- Two questions for the reader: does the pacote class read a new option? Does anything new touch the directory after extract? A "yes" to any of the three means a new fixture.')
    if (c.changelog) {
      out.push(`- Changelog: [compare view](${c.changelog.compareUrl})${c.changelog.lines ? (c.changelog.lines.length ? '; lines naming arborist, pacote, tar, cacache, bin-links or reify:' : '; no line names arborist, pacote, tar, cacache, bin-links or reify') : `; CHANGELOG.md not read (${c.changelog.error})`}`)
      for (const l of c.changelog.lines || []) {
        out.push(l.startsWith('**') ? `  - ${l}` : `    - ${l.replace(/^[*-]\s+/, '')}`)
      }
    }
    out.push('')
    // At most 20,000 characters of diffs per release (packComments keeps
    // each comment under GitHub's limit); the rest is in the artifacts.
    let budget = 20000
    for (const i of c.changed) {
      const f = path.join(r.dir, i.file)
      if (!fs.existsSync(f)) {
        continue
      }
      if (budget <= 0) {
        out.push(`- ${i.name}: diff not shown here (too long); see the run's artifacts.`)
        continue
      }
      const d = fs.readFileSync(f, 'utf8')
      out.push(`<details><summary>${i.name} (${i.status})</summary>`, '', ...fence(d, Math.min(budget, 12000)), '', '</details>')
      budget -= Math.min(d.length, 12000)
    }
  }
  out.push('')
  return out
}

function body (state, plan) {
  const L = []
  L.push('Tracking issue of the **npm release canary** (`.github/workflows/npm-canary.yml`), with its Node and Yarn 1 release watch. The workflow rewrites this body; edit only to reset the state (delete the state block to start over).', '')
  L.push(`Last successful poll: **${state.polled || 'never'}**. Last monthly run: ${state.monthly || 'never'}. Supported majors (INJECT_NPM_MAJORS): ${plan.supported.join(', ')}.${plan.newMajor ? ` **npm ${plan.newMajor} exists and is not enabled.**` : ''}`, '')
  L.push('| line | newest release tested | checked | suites | pacote class text | caller (vs last verified) | last verified | action |', '|---|---|---|---|---|---|---|---|')
  for (const m of Object.keys(state.lines).map(Number).sort((a, b) => a - b)) {
    const l = state.lines[m]
    L.push(`| ${m}${l.newMajor ? ' (not enabled)' : ''} | ${l.newest} | ${l.checked}${l.run ? ` ([run](${l.run}))` : ''} | ${l.suites} | ${l.classText} | ${l.caller} | ${l.verified || '-'} | ${l.action} |`)
  }
  if (state.node) {
    L.push('', '**Node release watch** (a new major, or a libuv change inside a watched line: the Yarn suites on macOS and the npm suites on Linux on that Node).', '')
    L.push('| Node line | newest release seen | libuv | last tested | Yarn suites | npm suites | what fs.copyFile keeps | last verified | action |', '|---|---|---|---|---|---|---|---|---|')
    for (const m of Object.keys(state.node.lines).map(Number).sort((a, b) => a - b)) {
      const l = state.node.lines[m]
      const watched = !plan.node || !plan.node.watched || plan.node.watched.includes(m)
      L.push(`| ${m}${watched ? '' : ' (no longer watched)'} | ${l.newest || '-'} | ${l.uv || '-'} | ${l.tested ? `${l.tested}, ${l.checked}${l.run ? ` ([run](${l.run}))` : ''}` : '-'} | ${l.tested ? l.yarn || '-' : '-'} | ${l.tested ? l.npm || '-' : '-'} | ${factsText(l.facts)} | ${l.verified || '-'} | ${l.action || '-'} |`)
    }
  }
  if (state.yarn) {
    const y = state.yarn
    L.push('', `**Yarn 1 release watch**: newest release seen ${y.newest || '-'} (${y.checked || '-'}); last tested ${y.tested ? `${y.tested}: ${y.suites}` : '-'}; last verified ${y.verified || '-'}; action ${y.action || '-'}.`)
  }
  L.push('', '<details><summary>History (newest first)</summary>', '', '| date | version | reason | Linux | macOS | class | caller | action |', '|---|---|---|---|---|---|---|---|')
  for (const h of state.history || []) {
    L.push(`| ${h.date} | ${h.run ? `[${h.version}](${h.run})` : h.version} | ${h.reason} | ${h.linux} | ${h.macos} | ${h.classText} | ${h.caller} | ${h.action} |`)
  }
  L.push('', '</details>', '', STATE_OPEN, JSON.stringify(state), STATE_CLOSE, '')
  return L.join('\n')
}

// --- main ------------------------------------------------------------------

async function main () {
  const [cmd, ...args] = process.argv.slice(2)
  const flag = (name) => {
    const i = args.indexOf(name)
    if (i < 0) {
      return undefined
    }
    const v = args[i + 1]
    args.splice(i, 2)
    return v
  }
  const bool = (name) => {
    const i = args.indexOf(name)
    if (i < 0) {
      return false
    }
    args.splice(i, 1)
    return true
  }
  switch (cmd) {
    case 'state':
      process.stdout.write(JSON.stringify(stateFromBody(fs.readFileSync(args[0], 'utf8')), null, 2) + '\n')
      break
    case 'poll': {
      const head = flag('--head')
      const versions = flag('--versions')
      const registryJson = flag('--registry-json')
      const date = flag('--date')
      const current = bool('--current')
      const node = flag('--node')
      const nodeIndexJson = flag('--node-index-json')
      const nodeUvDir = flag('--node-uv-dir')
      const yarnRegistryJson = flag('--yarn-registry-json')
      const plan = await poll(args[0], { head, versions: versions ? versions.split(',').map((s) => s.trim()).filter(Boolean) : null, registryJson, date, current, node: node || null, nodeIndexJson, nodeUvDir, yarnRegistryJson })
      process.stdout.write(JSON.stringify(plan, null, 2) + '\n')
      break
    }
    case 'inspect':
      inspect(args[0], args[1])
      break
    case 'compare':
      await compare(args[0], args[1], args[2])
      break
    case 'result':
      result(args[0])
      break
    case 'decide':
      process.stdout.write(JSON.stringify(decide(args[0], args[1])) + '\n')
      break
    case 'report': {
      report(args[0], args[1], args[2], args[3], { run: flag('--run') })
      break
    }
    default:
      die('usage: npm-canary.js state|poll|inspect|compare|result|decide|report ... (see the header)')
  }
}
main().catch((e) => die(e.stack || String(e)))
