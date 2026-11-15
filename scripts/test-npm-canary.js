#!/usr/bin/env node
// Offline tests of the npm canary's decisions (scripts/npm-canary.js): poll
// and report on hand-made states, plans and job results, the Node and Yarn 1
// release watch included. No network (CANARY_NO_NETWORK: a fetch fails the
// command), no npm.
//   node scripts/test-npm-canary.js
// Above all the fail-closed rules: a release is verified only when every
// check it needed left a result.
'use strict'
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const crypto = require('node:crypto')
const { execFileSync, spawnSync } = require('node:child_process')

const canary = path.join(__dirname, 'npm-canary.js')
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'npm-canary-test-'))
let pass = 0
let fail = 0
const check = (name, cond, detail) => {
  if (cond) {
    pass++
    console.log(`PASS ${name}`)
  } else {
    fail++
    console.log(`FAIL ${name}${detail ? ': ' + detail : ''}`)
  }
}
const write = (f, v) => {
  fs.mkdirSync(path.dirname(f), { recursive: true })
  fs.writeFileSync(f, JSON.stringify(v))
}
const env = { ...process.env, CANARY_NO_NETWORK: '1' }
const run = (...args) => execFileSync(process.execPath, [canary, ...args], { encoding: 'utf8', env })
const runStatus = (...args) => spawnSync(process.execPath, [canary, ...args], { encoding: 'utf8', env })

// The watch's inputs, hand-made: Node's release list (500 old 10.x releases
// as filler, then the lines of a test), libuv's version.h and fs.c per
// release (fs.c: by default one text per libuv version), and Yarn's version
// list.
const W = path.join(tmp, 'watch')
fs.mkdirSync(path.join(W, 'uv'), { recursive: true })
const filler = Array.from({ length: 500 }, (_, i) => ({ version: `v10.0.${i}`, date: '2019-01-01', lts: false }))
const nodeIndex = (name, entries) => {
  const f = path.join(W, `${name}.json`)
  write(f, [...entries.map(([v, date]) => ({ version: `v${v}`, date, lts: false })), ...filler])
  return f
}
const fsc = (uv) => `/* libuv ${uv}: src/unix/fs.c */\n`
const sha = (text) => crypto.createHash('sha256').update(text).digest('hex')
const uvh = (version, uv, src = fsc(uv)) => {
  const [a, b, c] = uv.split('.')
  fs.writeFileSync(path.join(W, 'uv', `v${version}.h`), `#define UV_VERSION_MAJOR ${a}\n#define UV_VERSION_MINOR ${b}\n#define UV_VERSION_PATCH ${c}\n#define UV_VERSION_IS_RELEASE 1\n`)
  if (src !== null) {
    fs.writeFileSync(path.join(W, 'uv', `v${version}.fs.c`), src)
  }
}
const yarnReg = path.join(W, 'yarn.json')
const yarnVersions = (extra = []) => write(yarnReg, { versions: Object.fromEntries([...Array.from({ length: 60 }, (_, i) => `1.${Math.floor(i / 5)}.${i % 5}`), '1.22.22', '2.4.3', '4.5.0', ...extra].map((v) => [v, {}])) })
yarnVersions()
// The ordinary index: 20 ended, 22/24/26 live, 27 only as an alpha.
const lines = [['20.20.2', '2026-03-24'], ['22.23.5', '2026-11-11'], ['24.22.3', '2026-11-10'], ['26.13.1', '2026-11-10'], ['27.0.0-alpha.0.1.0', '2026-11-12']]
for (const [v, uv] of [['20.20.2', '1.46.0'], ['22.23.5', '1.51.0'], ['24.22.3', '1.52.1'], ['26.13.1', '1.52.1']]) {
  uvh(v, uv)
}
const npmReg = path.join(W, 'npm.json')
write(npmReg, { versions: Object.fromEntries(['10.9.10', '11.21.1', '12.2.1'].map((v) => [v, {}])) })
const watchState = (node, yarn = { newest: '1.22.22', checked: '2026-11-13' }) => ({ ...state0, node, yarn })
const nl = (newest, uv, extra = {}) => ({ newest, uv, uvSrc: sha(fsc(uv)), checked: '2026-11-13', ...extra })
const steady = { lines: { 22: nl('22.23.5', '1.51.0'), 24: nl('24.22.3', '1.52.1'), 26: nl('26.13.1', '1.52.1', { tested: '26.13.1', uvTested: '1.52.1', suites: 'pass', verified: '26.13.1', head: 'h', facts: { empty_atime: true, empty_mtime: true, atime: true, mtime: false, group: true } }) } }
let pk = 0
function wpoll (state, index, extra = []) {
  const f = path.join(W, `state-${pk++}.json`)
  write(f, state)
  return runStatus('poll', f, '--registry-json', npmReg, '--date', '2026-11-15', '--head', 'h', '--node-index-json', index, '--node-uv-dir', path.join(W, 'uv'), '--yarn-registry-json', yarnReg, ...extra)
}
const plan = (r) => (r.status === 0 ? JSON.parse(r.stdout) : { error: r.stderr })
// A failed read of the watch: the poll itself succeeds (the npm half), the
// plan has no watch item and no job for it, and names the failure.
const watchFailed = (r, re) => {
  if (r.status !== 0) {
    return false
  }
  const p = JSON.parse(r.stdout)
  return p.node === null && p.yarn === null && p.nodeLinux.length === 0 && p.nodeMacos.length === 0 && re.test(p.watchError || '')
}

const suitesOk = { 'test-inject': { status: 0, pass: 70, fail: 0, skip: 0, failed: [] }, 'verify-npm': { status: 0, pass: 14, fail: 0, skip: 0, failed: [] } }
const cmpSame = (base) => ({ base, classChanged: false, callerChanged: false, deps: {}, changed: [], changelog: null })
const L = (newest, suites, head) => ({ newest, checked: '2026-11-13', suites, head, verified: newest, classText: 'known', caller: 'same', action: 'none' })
const state0 = { version: 1, polled: '2026-11-13', monthly: '2026-11', lines: { 10: L('10.9.10', 'pass', 'h'), 11: L('11.21.1', 'pass', 'h'), 12: L('12.2.1', 'pass', 'h') }, history: [] }

// The watch's files for a poll that is about npm (state0 has no `node`: a
// first run of the watch, from these files).
const watchFiles = () => ['--node-index-json', nodeIndex('ordinary', lines), '--node-uv-dir', path.join(W, 'uv'), '--yarn-registry-json', yarnReg]

// report: one release, a Linux result (or none), a macOS result (or none).
let n = 0
function report ({ item, linux, macos, items }) {
  const d = path.join(tmp, `r${n++}`)
  write(path.join(d, 'state.json'), state0)
  const todo = items || [{ item, linux }]
  write(path.join(d, 'plan.json'), { date: '2026-11-14', month: null, head: 'h', supported: [10, 11, 12], newMajor: null, skipped: [], todo: todo.map((x) => x.item) })
  for (const x of todo) {
    if (x.linux) {
      const rd = path.join(d, 'results', `canary-linux-${x.item.version}`)
      write(path.join(rd, 'result.json'), { version: x.item.version, baseline: x.item.baseline, newMajor: !!x.item.newMajor, suitesPass: true, suites: suitesOk, fingerprint: 'f'.repeat(64), fingerprintError: null, bareAwait: true, ...x.linux })
      for (const [f, text] of Object.entries(x.files || {})) {
        fs.mkdirSync(path.dirname(path.join(rd, f)), { recursive: true })
        fs.writeFileSync(path.join(rd, f), text)
      }
    }
  }
  if (macos) {
    write(path.join(d, 'results', `canary-macos-${item.version}`, 'suites.json'), { suites: macos })
  }
  run('report', path.join(d, 'state.json'), path.join(d, 'plan.json'), path.join(d, 'results'), path.join(d, 'out'))
  const out = path.join(d, 'out')
  const cdir = path.join(out, 'comments')
  const comments = fs.existsSync(cdir) ? fs.readdirSync(cdir).sort().map((f) => fs.readFileSync(path.join(cdir, f), 'utf8')) : []
  return {
    comments,
    state: JSON.parse(fs.readFileSync(path.join(out, 'state.json'), 'utf8')),
    flags: JSON.parse(fs.readFileSync(path.join(out, 'flags.json'), 'utf8')),
    comment: comments.join('\n'),
  }
}
const item = (version, extra = {}) => ({ version, major: Number(version.split('.')[0]), reason: 'new release', baseline: `${version.split('.').slice(0, 2).join('.')}.1`, newMajor: false, ...extra })

// 1. The ordinary case: a listed fingerprint, Linux green, comparison done.
let r = report({ item: item('11.21.2'), linux: { known: true, compare: cmpSame('11.21.1') } })
check('known, Linux green, compared: verified, nobody asked', r.state.lines[11].verified === '11.21.2' && r.state.lines[11].suites === 'pass' && !r.flags.attention && !r.flags.red && r.flags.notify === false, JSON.stringify(r.state.lines[11]))

// 2. macOS required (fingerprint not listed) and its result missing: a
//    failure, not a skip.
r = report({ item: item('11.21.2'), linux: { known: false, compare: cmpSame('11.21.1') } })
check('macOS required, no result: fail, not verified, red, a comment', r.state.lines[11].suites === 'fail' && r.state.lines[11].verified === '11.21.1' && r.flags.red && r.flags.attention && /left no result: counted as a failure/.test(r.comment), JSON.stringify({ line: r.state.lines[11], flags: r.flags }))
check('macOS required, no result: the history says missing', r.state.history[0].macos === 'missing', r.state.history[0].macos)

// 3. A new major with a listed fingerprint still needs macOS.
r = report({ item: item('13.0.0', { major: 13, baseline: '12.2.2', newMajor: true }), linux: { known: true, newMajor: true, compare: cmpSame('12.2.2') } })
check('new major, no macOS result: fail, not verified', r.state.lines[13].suites === 'fail' && !r.state.lines[13].verified && r.flags.red, JSON.stringify(r.state.lines[13]))

// 4. macOS required and green: verified (but a new class text still asks).
r = report({ item: item('11.21.2'), linux: { known: false, compare: cmpSame('11.21.1') }, macos: suitesOk })
check('macOS required and green: pass, verified, a person asked about the new text', r.state.lines[11].suites === 'pass' && r.state.lines[11].verified === '11.21.2' && r.flags.attention && !r.flags.red && r.flags.notify === true, JSON.stringify(r.state.lines[11]))

// 5. The planned comparison missing (baseline install or inspection
//    failed): incomplete, not verified, a comment, and the next poll
//    runs it again.
r = report({ item: item('11.21.2'), linux: { known: true } })
check('comparison missing: incomplete, not verified, red, a comment', r.state.lines[11].suites === 'incomplete' && r.state.lines[11].verified === '11.21.1' && r.state.lines[11].caller === 'missing' && r.flags.red && r.flags.attention && /Comparison with 11\.21\.1: missing/.test(r.comment), JSON.stringify({ line: r.state.lines[11], flags: r.flags }))
const reg = path.join(tmp, 'registry.json')
write(reg, { versions: Object.fromEntries(['10.9.10', '11.21.1', '11.21.2', '12.2.1'].map((v) => [v, {}])) })
write(path.join(tmp, 'state-after.json'), r.state)
const plan1 = JSON.parse(run('poll', path.join(tmp, 'state-after.json'), '--registry-json', reg, '--date', '2026-11-15', '--head', 'h', ...watchFiles()))
check('comparison missing: the next poll, same HEAD, runs the release again', plan1.todo.some((t) => t.version === '11.21.2' && /incomplete/.test(t.reason)), JSON.stringify(plan1.todo))

// 6. No baseline planned (the first release ever of a line): nothing missing.
r = report({ item: item('11.21.2', { baseline: null }), linux: { known: true } })
check('no baseline planned: pass, verified', r.state.lines[11].suites === 'pass' && r.state.lines[11].verified === '11.21.2', JSON.stringify(r.state.lines[11]))

// 7. No Linux result at all: error, red.
r = report({ item: item('11.21.2') })
check('no Linux result: error, not verified, red', r.state.lines[11].suites === 'error' && r.state.lines[11].verified === '11.21.1' && r.flags.red, JSON.stringify(r.state.lines[11]))

// 8. Many releases with long diffs: every comment stays under GitHub's
//    65,536-character limit, and every release is in one of them.
const big = (v, k) => ({
  item: item(v),
  linux: { known: false, compare: { ...cmpSame(`${v.split('.').slice(0, 2).join('.')}.1`), callerChanged: true, changed: [0, 1, 2].map((j) => ({ name: `f${k}${j}`, status: 'changed', file: `diffs/f${j}.diff`, lines: 900 })) } },
  files: Object.fromEntries([0, 1, 2].map((j) => [`diffs/f${j}.diff`, ('+' + 'x'.repeat(79) + '\n').repeat(400)])),
})
r = report({ items: ['10.9.11', '11.21.3', '11.22.0', '12.2.3', '12.3.0'].map(big) })
const lens = r.comments.map((c) => c.length)
check('long diffs of five releases: several comments, each under 65,536', r.comments.length > 1 && lens.every((l) => l < 65536), JSON.stringify(lens))
check('long diffs of five releases: every release reported', ['10.9.11', '11.21.3', '11.22.0', '12.2.3', '12.3.0'].every((v) => r.comment.includes(`### npm ${v} `)))

// 9. The workflow posts the comments before it writes the state, so a failed
//    post leaves the releases unseen (and the next poll reports them).
const wf = fs.readFileSync(path.join(__dirname, '../.github/workflows/npm-canary.yml'), 'utf8')
const posts = [...wf.matchAll(/for f in \S*comments\/\*\.md; do[^\n]*gh issue comment|gh issue edit [^\n]*body\.md/g)].map((m) => (m[0].startsWith('gh issue edit') ? 'state' : 'comments'))
check('workflow: comments posted before the state is written, in both places', posts.join(',') === 'comments,state,comments,state', posts.join(','))

// 10. A new major (not in INJECT_NPM_MAJORS) is retried like a supported
//     line: incomplete on the next poll, red when matchapack changed.
write(reg, { versions: Object.fromEntries(['10.9.10', '11.21.1', '12.2.1', '13.0.0'].map((v) => [v, {}])) })
const poll13 = (suites, head) => {
  write(path.join(tmp, 'state13.json'), { ...state0, lines: { ...state0.lines, 13: { ...L('13.0.0', suites, 'h'), newMajor: true } } })
  return JSON.parse(run('poll', path.join(tmp, 'state13.json'), '--registry-json', reg, '--date', '2026-11-15', '--head', head, ...watchFiles())).todo.filter((t) => t.version === '13.0.0')
}
let t13 = poll13('incomplete', 'h')
check('new major, incomplete: the next poll runs it again', t13.length === 1 && t13[0].newMajor === true, JSON.stringify(t13))
t13 = poll13('fail', 'h2')
check('new major, red, matchapack changed: run again', t13.length === 1 && t13[0].newMajor === true, JSON.stringify(t13))
t13 = poll13('fail', 'h')
check('new major, red, matchapack unchanged: not run again', t13.length === 0, JSON.stringify(t13))

// 11. The monthly run retests a release the line has verified: the diff
//     against the release before was reported then, so a changed function
//     asks for nobody (and the run stays green); a new one still does.
const changedFn = (base) => ({ ...cmpSame(base), callerChanged: true, changed: [{ name: 'f', status: 'changed', file: 'diffs/f.diff', lines: 2 }] })
r = report({ items: [{ item: item('11.21.1', { reason: 'monthly run', baseline: '11.21.0' }), linux: { known: true, compare: changedFn('11.21.0') }, files: { 'diffs/f.diff': '+x\n' } }] })
check('monthly retest of a verified release, function changed: nobody asked, green', r.state.lines[11].suites === 'pass' && r.state.lines[11].action === 'none' && r.flags.notify === false && r.comments.length === 0, JSON.stringify({ line: r.state.lines[11], flags: r.flags }))
r = report({ items: [{ item: item('11.21.2'), linux: { known: true, compare: changedFn('11.21.1') }, files: { 'diffs/f.diff': '+x\n' } }] })
check('new release, function changed: read the diff, a comment, the run notifies', r.state.lines[11].action === 'read the diff' && r.flags.notify === true && r.comments.length === 1, JSON.stringify({ line: r.state.lines[11], flags: r.flags }))
r = report({ items: [{ item: item('11.20.0', { reason: 'requested', baseline: '11.19.1' }), linux: { known: true, compare: changedFn('11.19.1') }, files: { 'diffs/f.diff': '+x\n' } }] })
check('older release requested by hand (below the verified one), function changed: still asks', r.state.history[0].action === 'read the diff' && r.flags.notify === true && r.comments.length === 1, JSON.stringify({ history: r.state.history[0], flags: r.flags }))

// --- The Node and Yarn 1 release watch --------------------------------------
// 12. First run (no `node` in the state): the live lines recorded (22, 24),
//     the newest (26) tested once; Node 20 (last release 2026-03-24) not
//     watched; the alpha of 27 is no release; Yarn's newest recorded.
let pl = plan(wpoll(state0, nodeIndex('ordinary', lines)))
check('watch, first run: 22 and 24 recorded, 26 tested once, 20 not watched', pl.node && JSON.stringify(pl.node.record.map((r) => [r.line, r.newest, r.uv])) === JSON.stringify([[22, '22.23.5', '1.51.0'], [24, '24.22.3', '1.52.1']]) &&
  pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.1' && pl.node.todo[0].npm && pl.node.todo[0].yarn && JSON.stringify(pl.node.watched) === '[22,24,26]', JSON.stringify(pl.node || pl))
check('watch, first run: Yarn 1.22.22 recorded, not tested', pl.yarn && pl.yarn.record && pl.yarn.record.newest === '1.22.22' && pl.yarn.todo.length === 0, JSON.stringify(pl.yarn))
check('watch, first run: the jobs: npm suites per supported line at its verified release, Yarn suites once', JSON.stringify(pl.nodeLinux.map((e) => e.npm)) === '["10.9.10","11.21.1","12.2.1"]' && pl.nodeLinux.every((e) => e.node === '26.13.1') &&
  pl.nodeMacos.length === 1 && pl.nodeMacos[0].node === '26.13.1' && pl.nodeMacos[0].yarn === '1.22.22', JSON.stringify({ l: pl.nodeLinux, m: pl.nodeMacos }))

// 13. Nothing new: no item. The alpha of 27 is not a new major.
pl = plan(wpoll(watchState(steady), nodeIndex('ordinary', lines)))
check('watch: v27.0.0-alpha.0.1.0 newest, no v27.0.0: no Node item', pl.node && pl.node.todo.length === 0 && pl.node.record.length === 0 && pl.nodeLinux.length === 0 && pl.nodeMacos.length === 0, JSON.stringify(pl.node || pl))

// 14. Node 27.0.0 is released: one item, both suites.
uvh('27.0.0', '1.53.0')
pl = plan(wpoll(watchState(steady), nodeIndex('n27', [...lines, ['27.0.0', '2026-11-14']])))
check('watch: v27.0.0 appears: one item, a new major, npm and Yarn suites', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '27.0.0' && pl.node.todo[0].newMajor === true && pl.node.todo[0].npm && pl.node.todo[0].yarn &&
  pl.nodeLinux.length === 3 && pl.nodeMacos.length === 1, JSON.stringify(pl.node || pl))

// 14b. Two new majors since the last poll (the canary was off): both
//      tested, not only the highest.
const upTo24 = { lines: { 22: steady.lines[22], 24: steady.lines[24] } }
uvh('25.9.1', '1.51.0')
pl = plan(wpoll(watchState(upTo24), nodeIndex('n25-26', [...lines, ['25.9.1', '2026-10-01']])))
check('watch: Node 25 and 26 both new since the last poll: both tested as new majors', pl.node && JSON.stringify(pl.node.todo.map((t) => [t.line, t.newMajor])) === '[[25,true],[26,true]]' && pl.nodeMacos.length === 2, JSON.stringify(pl.node || pl))

// 15. A new 26.x with the same libuv: recorded, no job. With another libuv:
//     one item, both suites.
uvh('26.13.2', '1.52.1')
pl = plan(wpoll(watchState(steady), nodeIndex('same-uv', [...lines, ['26.13.2', '2026-11-14']])))
check('watch: a new 26.x, same libuv: recorded, no job', pl.node && pl.node.todo.length === 0 && JSON.stringify(pl.node.record) === JSON.stringify([{ line: 26, newest: '26.13.2', uv: '1.52.1', uvSrc: sha(fsc('1.52.1')) }]) && pl.nodeMacos.length === 0, JSON.stringify(pl.node || pl))
uvh('26.14.0', '1.54.0')
pl = plan(wpoll(watchState(steady), nodeIndex('new-uv', [...lines, ['26.14.0', '2026-11-14']])))
check('watch: a new 26.x with another libuv: one item, npm and Yarn suites', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '26.14.0' && pl.node.todo[0].uvBefore === '1.52.1' && pl.node.todo[0].uv === '1.54.0' &&
  /libuv 1\.52\.1 → 1\.54\.0/.test(pl.node.todo[0].reason) && pl.nodeLinux.length === 3 && pl.nodeMacos.length === 1, JSON.stringify(pl.node || pl))

// 16. Fail closed: version.h unreadable, a short release list.
let r16 = wpoll(watchState(steady), nodeIndex('no-uv', [...lines, ['26.15.0', '2026-11-14']]))
check('watch: version.h of a new release cannot be read: the watch fails, the npm half goes on', watchFailed(r16, /libuv of Node 26\.15\.0/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)
fs.writeFileSync(path.join(W, 'uv', 'v26.15.1.h'), '/* no version here */\n')
r16 = wpoll(watchState(steady), nodeIndex('bad-uv', [...lines, ['26.15.1', '2026-11-14']]))
check('watch: a version.h without the version: the watch fails, the npm half goes on', watchFailed(r16, /version\.h has no UV_VERSION_MAJOR/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)
const short = path.join(W, 'short.json')
write(short, lines.map(([v, date]) => ({ version: `v${v}`, date })))
r16 = wpoll(watchState(steady), short)
check('watch: a release list of 4 entries: the watch fails, the npm half goes on', watchFailed(r16, /only 4 releases/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)
r16 = runStatus('poll', path.join(W, 'state-0.json'), '--registry-json', npmReg, '--date', '2026-11-15')
check('watch: with no file given and no network allowed, the watch fails (never decides without the list), the npm half goes on', watchFailed(r16, /CANARY_NO_NETWORK/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)

// 17. Retries: a red line again when matchapack changed; a line only
//     recorded (never tested) is not "red".
const red26 = { lines: { ...steady.lines, 26: { ...steady.lines[26], suites: 'fail', head: 'old' } } }
pl = plan(wpoll(watchState(red26), nodeIndex('ordinary', lines)))
check('watch: a red line, matchapack changed: tested again', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.1' && /was fail/.test(pl.node.todo[0].reason), JSON.stringify(pl.node || pl))
pl = plan(wpoll(watchState({ lines: { ...red26.lines, 26: { ...red26.lines[26], head: 'h' } } }), nodeIndex('ordinary', lines)))
check('watch: a red line, matchapack unchanged: not again; recorded lines never', pl.node && pl.node.todo.length === 0, JSON.stringify(pl.node || pl))

// 17b. A line to be retested gets a new release with the same libuv: the
//      new release is tested (not only recorded, which would wait a poll).
uvh('26.13.3', '1.52.1')
const inc26 = { lines: { ...steady.lines, 26: { ...steady.lines[26], suites: 'incomplete' } } }
pl = plan(wpoll(watchState(inc26), nodeIndex('inc-new', [...lines, ['26.13.3', '2026-11-14']])))
check('watch: an incomplete line and a new same-libuv release: the new release is tested', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.3' && pl.node.record.length === 0 && /incomplete/.test(pl.node.todo[0].reason), JSON.stringify(pl.node || pl))

// 17c. A Node release requested by hand: on a first run (no watch state)
//      not a new major; above every known line, a new major.
pl = plan(wpoll(state0, nodeIndex('ordinary', lines), ['--node', '26.13.1']))
check('watch: --node 26.13.1 on a first run: tested, not a new major', pl.node && pl.node.todo.some((t) => t.node === '26.13.1' && /^requested(; |$)/.test(t.reason) && t.newMajor === false), JSON.stringify(pl.node || pl))
pl = plan(wpoll(watchState(upTo24), nodeIndex('ordinary', lines), ['--node', '26.13.1']))
check('watch: --node 26.13.1 with lines 22 and 24 known: a new major', pl.node && pl.node.todo.some((t) => t.node === '26.13.1' && t.newMajor === true), JSON.stringify(pl.node || pl))

// 17d. A red line retried when matchapack changed tests the line's newest
//      release (a later one with the same libuv was recorded meanwhile).
const redOld = { lines: { ...steady.lines, 26: { ...steady.lines[26], newest: '26.13.2', suites: 'fail', head: 'old' } } }
pl = plan(wpoll(watchState(redOld), nodeIndex('same-uv', [...lines, ['26.13.2', '2026-11-14']])))
check('watch: a red line retried tests its newest release', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.2', JSON.stringify(pl.node || pl))

// 17e. A line no longer watched (Node 20) whose test, requested by hand,
//      left no result: tested again on the next poll all the same.
const inc20 = { lines: { ...steady.lines, 20: nl('20.20.2', '1.46.0', { tested: '20.20.2', uvTested: '1.46.0', suites: 'incomplete', head: 'h' }) } }
pl = plan(wpoll(watchState(inc20), nodeIndex('ordinary', lines)))
check('watch: an incomplete test of a line no longer watched is repeated', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '20.20.2' && /incomplete/.test(pl.node.todo[0].reason), JSON.stringify(pl.node || pl))

// 17f. libuv's fs.c (Node cherry-picks libuv fixes without changing
//      libuv's version: fs.c changed at v26.0.0 -> v26.1.0 -> v26.2.0 with
//      version.h at 1.52.1): a new release with the same version and
//      another fs.c is an item, both suites; both changed, the reason names
//      both.
uvh('26.13.4', '1.52.1', '/* libuv 1.52.1, src/unix/fs.c with a cherry-picked copyfile fix */\n')
pl = plan(wpoll(watchState(steady), nodeIndex('new-src', [...lines, ['26.13.4', '2026-11-14']])))
check('watch: a new 26.x, same libuv version, another fs.c: one item, npm and Yarn suites, the reason names fs.c', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.4' && pl.node.todo[0].uvChange === true &&
  pl.node.todo[0].uvBefore === null && pl.node.todo[0].uvSrcBefore === sha(fsc('1.52.1')) && pl.node.todo[0].uvSrc === sha('/* libuv 1.52.1, src/unix/fs.c with a cherry-picked copyfile fix */\n') &&
  /libuv's fs\.c [0-9a-f]{12} → [0-9a-f]{12} \(libuv still 1\.52\.1\) in Node 26/.test(pl.node.todo[0].reason) && pl.nodeLinux.length === 3 && pl.nodeMacos.length === 1, JSON.stringify(pl.node || pl))
uvh('26.14.1', '1.54.0', '/* libuv 1.54.0, patched */\n')
pl = plan(wpoll(watchState(steady), nodeIndex('new-both', [...lines, ['26.14.1', '2026-11-14']])))
check('watch: a new 26.x, another libuv version and fs.c: one item, the reason names both', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].uvBefore === '1.52.1' &&
  /^libuv 1\.52\.1 → 1\.54\.0, libuv's fs\.c [0-9a-f]{12} → [0-9a-f]{12} in Node 26$/.test(pl.node.todo[0].reason), JSON.stringify(pl.node || pl))

// 17g. A state from before the hash was kept: each watched line's hash is
//      seeded from its newest release (recorded, no test); a new release is
//      compared with that seed.
const noSrc = { lines: Object.fromEntries(Object.entries(steady.lines).map(([k, l]) => [k, Object.fromEntries(Object.entries(l).filter(([f]) => f !== 'uvSrc'))])) }
pl = plan(wpoll(watchState(noSrc), nodeIndex('ordinary', lines)))
check('watch: lines with no fs.c hash yet: each seeded from its newest release, no item', pl.node && pl.node.todo.length === 0 && pl.nodeMacos.length === 0 &&
  JSON.stringify(pl.node.record.map((r) => [r.line, r.newest, r.uv, r.uvSrc])) === JSON.stringify([[22, '22.23.5', '1.51.0', sha(fsc('1.51.0'))], [24, '24.22.3', '1.52.1', sha(fsc('1.52.1'))], [26, '26.13.1', '1.52.1', sha(fsc('1.52.1'))]]), JSON.stringify(pl.node || pl))
pl = plan(wpoll(watchState(noSrc), nodeIndex('same-uv', [...lines, ['26.13.2', '2026-11-14']])))
check('watch: no fs.c hash yet, a new 26.x with the same libuv and fs.c: recorded with its hash, no item', pl.node && pl.node.todo.length === 0 &&
  pl.node.record.some((r) => r.line === 26 && r.newest === '26.13.2' && r.uvSrc === sha(fsc('1.52.1'))), JSON.stringify(pl.node || pl))
pl = plan(wpoll(watchState(noSrc), nodeIndex('new-src', [...lines, ['26.13.4', '2026-11-14']])))
check('watch: no fs.c hash yet, a new 26.x whose fs.c differs from the newest release\'s: an item', pl.node && pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.4' && pl.node.todo[0].uvChange === true, JSON.stringify(pl.node || pl))

// 17h. Fail closed on fs.c as on version.h: missing for a new release,
//      missing for a seed, empty.
uvh('26.15.2', '1.52.1', null)
r16 = wpoll(watchState(steady), nodeIndex('no-src', [...lines, ['26.15.2', '2026-11-14']]))
check('watch: fs.c of a new release cannot be read: the watch fails, the npm half goes on', watchFailed(r16, /libuv source of Node 26\.15\.2/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)
r16 = wpoll(watchState({ lines: { ...noSrc.lines, 26: { ...noSrc.lines[26], newest: '26.15.2' } } }), nodeIndex('no-src', [...lines, ['26.15.2', '2026-11-14']]))
check('watch: fs.c of the release a hash is seeded from cannot be read: the watch fails, the npm half goes on', watchFailed(r16, /libuv source of Node 26\.15\.2/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)
uvh('26.15.3', '1.52.1', '')
r16 = wpoll(watchState(steady), nodeIndex('empty-src', [...lines, ['26.15.3', '2026-11-14']]))
check('watch: an empty fs.c: the watch fails, the npm half goes on', watchFailed(r16, /fs\.c is empty/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)

// 17i. A release whose date does not parse fails the poll (its line would
//      otherwise drop out of the watched lines).
r16 = wpoll(watchState(steady), nodeIndex('undated', [...lines, ['26.13.2', 'soon']]))
check('watch: a release with a date that does not parse: the watch fails, the npm half goes on', watchFailed(r16, /v26\.13\.2 has no readable date/), `${r16.status} ${r16.stdout.slice(0, 300)} ${r16.stderr}`)

// 17j. `node: { lines: {} }` (an edited state) is a first run, not three
//      new majors.
pl = plan(wpoll(watchState({ lines: {} }), nodeIndex('ordinary', lines)))
check('watch: a state with no Node line is a first run: 22 and 24 recorded, 26 tested, no new major', pl.node && JSON.stringify(pl.node.record.map((r) => r.line)) === '[22,24]' &&
  pl.node.todo.length === 1 && pl.node.todo[0].node === '26.13.1' && pl.node.todo[0].newMajor === false, JSON.stringify(pl.node || pl))

// 18. Yarn: a new stable 1.x is an item; 2.x and prereleases are not.
yarnVersions(['1.22.23', '1.23.0-rc.1', '2.4.4'])
pl = plan(wpoll(watchState(steady), nodeIndex('ordinary', lines)))
check('watch: Yarn 1.22.23 published: one item on the pinned Node; 1.23.0-rc.1 and 2.x ignored', pl.yarn && pl.yarn.todo.length === 1 && pl.yarn.todo[0].yarn === '1.22.23' && pl.nodeMacos.length === 1 && pl.nodeMacos[0].node === null && pl.nodeMacos[0].yarn === '1.22.23', JSON.stringify({ y: pl.yarn, m: pl.nodeMacos }))
yarnVersions()

// report on watch items
const factsOk = { empty_atime: true, empty_mtime: true, atime: true, mtime: false, group: true }
const ysuites = (ok = true, facts = factsOk, node = 'v26.14.0', yarn = '1.22.22', uv = '1.54.0') => ({ node, uv, yarn, facts, suites: { 'test-yarn1': { status: ok ? 0 : 1, pass: 150, fail: ok ? 0 : 1, skip: 0, failed: ok ? [] : ['FAIL yarn1-many engine (exit 1)'] } } })
function wreport (state, pl, results) {
  const d = path.join(tmp, `w${n++}`)
  write(path.join(d, 'state.json'), state)
  write(path.join(d, 'plan.json'), { date: '2026-11-15', month: null, head: 'h', supported: [10, 11, 12], newMajor: null, skipped: [], todo: [], ...pl })
  for (const [k, v] of Object.entries(results)) {
    if (v) {
      write(path.join(d, 'results', `canary-${k}`, 'suites.json'), v)
    } else {
      fs.mkdirSync(path.join(d, 'results', `canary-${k}`), { recursive: true })
    }
  }
  run('report', path.join(d, 'state.json'), path.join(d, 'plan.json'), path.join(d, 'results'), path.join(d, 'out'))
  const out = path.join(d, 'out')
  const cdir = path.join(out, 'comments')
  return {
    comment: fs.existsSync(cdir) ? fs.readdirSync(cdir).map((f) => fs.readFileSync(path.join(cdir, f), 'utf8')).join('\n') : '',
    state: JSON.parse(fs.readFileSync(path.join(out, 'state.json'), 'utf8')),
    flags: JSON.parse(fs.readFileSync(path.join(out, 'flags.json'), 'utf8')),
    body: fs.readFileSync(path.join(out, 'body.md'), 'utf8'),
  }
}
const uvItem = { node: '26.14.0', line: 26, uv: '1.54.0', uvSrc: sha(fsc('1.54.0')), uvBefore: '1.52.1', uvSrcBefore: sha(fsc('1.52.1')), uvChange: true, reason: 'libuv 1.52.1 → 1.54.0 in Node 26', npm: true, yarn: true, newMajor: false }
const uvJobs = { nodeLinux: ['10.9.10', '11.21.1', '12.2.1'].map((v) => ({ key: `node26.14.0-npm${v}`, node: '26.14.0', npm: v })), nodeMacos: [{ key: 'node26.14.0', node: '26.14.0', yarn: '1.22.22' }] }
const linuxOk = Object.fromEntries(uvJobs.nodeLinux.map((e) => [`nodelinux-${e.key}`, { npm: e.npm, node: `v${e.node}`, suites: suitesOk }]))
const wplan = (todo, extra = {}) => ({ node: { todo, record: [], watched: [22, 24, 26] }, yarn: { todo: [], record: null }, ...uvJobs, ...extra })

// 19. libuv change, every suite green, the facts as recorded: nobody asked.
let w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites() })
check('watch report: libuv change, green, facts as recorded: no comment, green run, uv and verified updated', w.comment === '' && w.flags.notify === false && w.state.node.lines[26].uv === '1.54.0' && w.state.node.lines[26].verified === '26.14.0' && w.state.node.lines[26].action === 'none', JSON.stringify({ l: w.state.node.lines[26], f: w.flags }))
check('watch report: the body has the Node table and the Yarn line', /\| 26 \| 26\.14\.0 \| 1\.54\.0 \(fs\.c [0-9a-f]{12}\) \|/.test(w.body) && w.state.node.lines[26].uvSrc === sha(fsc('1.54.0')) && /Yarn 1 release watch\*\*: newest release seen 1\.22\.22/.test(w.body), w.body.split('\n').filter((l) => /Node|Yarn/.test(l)).join(' / '))

// 20. libuv change, the facts differ: a comment, notify.
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites(true, { ...factsOk, mtime: true }) })
check('watch report: libuv change, facts differ: a comment naming them, the run notifies', w.flags.notify === true && /\*\*changed\*\*/.test(w.comment) && w.state.node.lines[26].action === 'action needed', JSON.stringify({ f: w.flags, c: w.comment.slice(0, 300) }))

// 21. The macOS job left no result: a failure (incomplete), not a pass.
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': null })
check('watch report: node-macos result missing: incomplete, not verified, red', w.state.node.lines[26].suites === 'incomplete' && w.state.node.lines[26].verified === '26.13.1' && w.flags.red && w.flags.notify, JSON.stringify({ l: w.state.node.lines[26], f: w.flags }))
w = wreport(watchState(steady), wplan([uvItem]), { 'nodemacos-node26.14.0': ysuites() })
check('watch report: the npm suites on that Node missing: incomplete, red', w.state.node.lines[26].suites === 'incomplete' && w.flags.red, JSON.stringify(w.state.node.lines[26]))
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites(false) })
check('watch report: the Yarn suites red: fail, the failed case in the comment', w.state.node.lines[26].suites === 'fail' && w.flags.red && /FAIL yarn1-many engine/.test(w.comment), JSON.stringify(w.state.node.lines[26]))

// 21b. Every suite green, but the macOS result has no facts: incomplete
//      (not verified, retried on the next poll).
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites(true, null) })
check('watch report: green but no facts: incomplete, not verified, red', w.state.node.lines[26].suites === 'incomplete' && w.state.node.lines[26].verified === '26.13.1' && w.flags.red, JSON.stringify(w.state.node.lines[26]))

// 22. A new major, all green: still a comment and a notice.
const n27 = { node: '27.0.0', line: 27, uv: '1.53.0', reason: 'Node 27 is new', npm: true, yarn: true, newMajor: true }
const n27jobs = { nodeLinux: ['10.9.10'].map((v) => ({ key: `node27.0.0-npm${v}`, node: '27.0.0', npm: v })), nodeMacos: [{ key: 'node27.0.0', node: '27.0.0', yarn: '1.22.22' }] }
w = wreport(watchState(steady), { ...wplan([n27]), ...n27jobs }, { 'nodelinux-node27.0.0-npm10.9.10': { npm: '10.9.10', node: 'v27.0.0', suites: suitesOk }, 'nodemacos-node27.0.0': ysuites(true, factsOk, 'v27.0.0', '1.22.22', '1.53.0') })
check('watch report: new major, all green: a comment, the run notifies, the line added and verified', w.flags.notify === true && /Node 27\.0\.0 \(Node 27 is new\): action needed/.test(w.comment) && w.state.node.lines[27].verified === '27.0.0', JSON.stringify({ f: w.flags, l: w.state.node.lines[27] }))

// 22b. An older release requested by hand, green, while the line's newer
//      release is red: the line keeps the red result (and is retried); the
//      older one gets its history row.
const redNewer = { lines: { ...steady.lines, 26: { ...steady.lines[26], newest: '26.14.0', tested: '26.14.0', uv: '1.54.0', suites: 'fail', action: 'action needed', verified: '26.13.1' } } }
const reqOld = { node: '26.13.1', line: 26, uv: '1.52.1', reason: 'requested', npm: true, yarn: true, newMajor: false }
const reqJobs = { nodeLinux: [{ key: 'node26.13.1-npm10.9.10', node: '26.13.1', npm: '10.9.10' }], nodeMacos: [{ key: 'node26.13.1', node: '26.13.1', yarn: '1.22.22' }] }
w = wreport(watchState(redNewer), { ...wplan([reqOld]), ...reqJobs }, { 'nodelinux-node26.13.1-npm10.9.10': { npm: '10.9.10', node: 'v26.13.1', suites: suitesOk }, 'nodemacos-node26.13.1': ysuites(true, factsOk, 'v26.13.1', '1.22.22', '1.52.1') })
check('watch report: an older release requested and green does not hide the newer red one', w.state.node.lines[26].tested === '26.14.0' && w.state.node.lines[26].suites === 'fail' && w.state.node.lines[26].verified === '26.13.1' && w.state.history[0].version === 'Node 26.13.1', JSON.stringify({ l: w.state.node.lines[26], h: w.state.history[0] }))

// 22c. A result that is not of the release the job was for (another Node,
//      npm or Yarn ran): a failure, never a pass, and said so.
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites(true, factsOk, 'v24.22.1') })
check('watch report: the macOS result ran on another Node: fail, not verified, WRONG in the comment', w.state.node.lines[26].suites === 'fail' && w.state.node.lines[26].verified === '26.13.1' && w.flags.red && /not of the release asked for\*\* \(ran on Node v24\.22\.1, not 26\.14\.0\)/.test(w.comment), JSON.stringify({ l: w.state.node.lines[26], c: w.comment.slice(0, 400) }))
// Its probe's facts and libuv are another Node's: not kept for the line
// (the next test would compare with them).
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites(true, { ...factsOk, mtime: true, atime: false }, 'v24.22.1', '1.22.22', '1.52.1') })
check('watch report: a result of another Node: its facts and libuv are not stored for the line', JSON.stringify(w.state.node.lines[26].facts) === JSON.stringify(steady.lines[26].facts) && w.state.node.lines[26].uvRuntime === null, JSON.stringify(w.state.node.lines[26]))
w = wreport(watchState(steady), { node: { todo: [], record: [], watched: [22, 24, 26] }, yarn: { todo: [{ yarn: '1.22.23', reason: 'Yarn 1.22.23 is new' }], record: null }, nodeLinux: [], nodeMacos: [{ key: 'yarn1.22.23', node: null, yarn: '1.22.23' }] }, { 'nodemacos-yarn1.22.23': ysuites(true, factsOk, 'v24.22.1', '1.22.22', '1.52.1') })
check('watch report: a Yarn item whose result ran another Yarn: fail, and its facts and Node are not stored', w.state.yarn.suites === 'fail' && w.state.yarn.facts === null && w.state.yarn.node === null, JSON.stringify(w.state.yarn))
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodelinux-node26.14.0-npm11.21.1': { npm: '11.21.2', node: 'v26.14.0', suites: suitesOk }, 'nodemacos-node26.14.0': ysuites() })
check('watch report: a Linux result of another npm: fail', w.state.node.lines[26].suites === 'fail' && /ran npm 11\.21\.2, not 11\.21\.1/.test(w.comment), JSON.stringify(w.state.node.lines[26]))

// 22d. The libuv version kept for the next comparison is version.h's; a
//      job whose Node reports another asks for a person.
w = wreport(watchState(steady), wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites(true, factsOk, 'v26.14.0', '1.22.22', '1.54.0-dev') })
check('watch report: runtime libuv 1.54.0-dev, version.h 1.54.0: the line keeps 1.54.0, a person is asked', w.state.node.lines[26].uv === '1.54.0' && w.state.node.lines[26].uvRuntime === '1.54.0-dev' && w.flags.notify && /the job's Node reports 1\.54\.0-dev/.test(w.comment), JSON.stringify(w.state.node.lines[26]))

// 22e. A libuv change on a line recorded without facts (24 after the first
//      run): nothing to compare the new facts with, so a person is asked,
//      green or not; the same for an fs.c change.
const jobs24 = (v) => ({ nodeLinux: ['10.9.10', '11.21.1', '12.2.1'].map((x) => ({ key: `node${v}-npm${x}`, node: v, npm: x })), nodeMacos: [{ key: `node${v}`, node: v, yarn: '1.22.22' }] })
const res24 = (v, uv) => ({ ...Object.fromEntries(['10.9.10', '11.21.1', '12.2.1'].map((x) => [`nodelinux-node${v}-npm${x}`, { npm: x, node: `v${v}`, suites: suitesOk }])), [`nodemacos-node${v}`]: ysuites(true, factsOk, `v${v}`, '1.22.22', uv) })
const uv24 = { node: '24.23.0', line: 24, uv: '1.53.0', uvSrc: sha(fsc('1.53.0')), uvBefore: '1.52.1', uvSrcBefore: sha(fsc('1.52.1')), uvChange: true, reason: 'libuv 1.52.1 → 1.53.0 in Node 24', npm: true, yarn: true, newMajor: false }
w = wreport(watchState(steady), { ...wplan([uv24]), ...jobs24('24.23.0') }, res24('24.23.0', '1.53.0'))
check('watch report: a libuv change on a line recorded without facts: verified, but action needed, said in the comment', w.state.node.lines[24].suites === 'pass' && w.state.node.lines[24].verified === '24.23.0' &&
  w.state.node.lines[24].action === 'action needed' && w.flags.notify && /No facts to compare with\*\*: Node 24 was recorded without a test/.test(w.comment) && w.state.node.lines[24].facts && w.state.node.lines[24].facts.atime === true, JSON.stringify({ l: w.state.node.lines[24], f: w.flags }))
const src24 = { ...uv24, node: '24.22.4', uv: '1.52.1', uvSrc: sha('patched'), uvBefore: null, reason: "libuv's fs.c … in Node 24" }
w = wreport(watchState(steady), { ...wplan([src24]), ...jobs24('24.22.4') }, res24('24.22.4', '1.52.1'))
check('watch report: an fs.c change on a line recorded without facts: action needed; the line keeps the new hash', w.state.node.lines[24].action === 'action needed' && /No facts to compare with/.test(w.comment) && /fs\.c [0-9a-f]{12} → [0-9a-f]{12}/.test(w.comment) && w.state.node.lines[24].uvSrc === sha('patched'), JSON.stringify({ l: w.state.node.lines[24], c: w.comment.slice(0, 400) }))

// 22f. An item with no job at all (a plan that names none for it) is not
//      a pass: verdict of no jobs is incomplete.
w = wreport(watchState(steady), { node: { todo: [], record: [], watched: [22, 24, 26] }, yarn: { todo: [{ yarn: '1.22.22', reason: 'requested' }], record: null }, nodeLinux: [], nodeMacos: [] }, {})
check('watch report: an item with no job: incomplete, not verified, red', w.state.yarn.suites === 'incomplete' && !w.state.yarn.verified && w.flags.red, JSON.stringify({ y: w.state.yarn, f: w.flags }))

// 22g. A release requested by hand that is also a libuv change on a line
//      without facts: one item that keeps both triggers (the libuv change
//      is not dropped as a duplicate), and the report asks for a person.
uvh('24.23.0', '1.53.0')
pl = plan(wpoll(watchState(steady), nodeIndex('req-uv24', [...lines, ['24.23.0', '2026-11-14']]), ['--node', '24.23.0']))
const t24 = pl.node ? pl.node.todo.filter((t) => t.node === '24.23.0') : []
check('watch: a requested release that is also a libuv change: one item with both reasons and the libuv change', t24.length === 1 && t24[0].uvChange === true && t24[0].uvBefore === '1.52.1' &&
  /^requested; libuv 1\.52\.1 → 1\.53\.0/.test(t24[0].reason) && pl.nodeMacos.filter((e) => e.node === '24.23.0').length === 1, JSON.stringify(pl.node || pl))
w = wreport(watchState(steady), { node: pl.node, yarn: pl.yarn, nodeLinux: pl.nodeLinux, nodeMacos: pl.nodeMacos }, res24('24.23.0', '1.53.0'))
check('watch report: that item, green, on a line recorded without facts: action needed, said in the comment', w.state.node.lines[24].suites === 'pass' && w.state.node.lines[24].action === 'action needed' && /No facts to compare with/.test(w.comment), JSON.stringify({ l: w.state.node.lines[24], f: w.flags }))

// 23. Recorded releases only (the "nothing to test" path): state updated,
//     green.
w = wreport(watchState(steady), { node: { todo: [], record: [{ line: 26, newest: '26.13.2', uv: '1.52.1' }, { line: 24, newest: '24.22.3', uv: '1.52.1', uvSrc: sha('seeded') }], watched: [22, 24, 26] }, yarn: { todo: [], record: null }, nodeLinux: [], nodeMacos: [] }, {})
check('watch report: a release only recorded: newest updated, tested and verified kept, green', w.state.node.lines[26].newest === '26.13.2' && w.state.node.lines[26].tested === '26.13.1' && w.state.node.lines[26].verified === '26.13.1' && w.flags.notify === false, JSON.stringify(w.state.node.lines[26]))
check('watch report: a recorded release keeps its fs.c hash (a seed too); one without keeps the line\'s', w.state.node.lines[24].uvSrc === sha('seeded') && w.state.node.lines[26].uvSrc === sha(fsc('1.52.1')), JSON.stringify(w.state.node.lines))

// 24. A new Yarn 1 release: always a person, even when green.
w = wreport(watchState(steady), { node: { todo: [], record: [], watched: [22, 24, 26] }, yarn: { todo: [{ yarn: '1.22.23', reason: 'Yarn 1.22.23 is new' }], record: null }, nodeLinux: [], nodeMacos: [{ key: 'yarn1.22.23', node: null, yarn: '1.22.23' }] }, { 'nodemacos-yarn1.22.23': ysuites(true, factsOk, 'v24.22.1', '1.22.23', '1.52.1') })
check('watch report: Yarn 1.22.23, green: verified, and a comment asks for a person', w.state.yarn.newest === '1.22.23' && w.state.yarn.verified === '1.22.23' && w.flags.notify === true && /Yarn 1\.22\.23 \(Yarn 1\.22\.23 is new\): action needed/.test(w.comment), JSON.stringify({ y: w.state.yarn, f: w.flags }))

// 25. The workflow: the watch's jobs and the report wait for them.
check('workflow: node-linux and node-macos jobs; the report needs both', /\n  node-linux:\n/.test(wf) && /\n  node-macos:\n/.test(wf) && /needs: \[poll, linux, decide, macos, node-linux, node-macos\]/.test(wf), '')

// 26. A read of the release watch fails (nodejs.org or raw.githubusercontent.com
//     down) on a day with a new npm release: the npm release is planned and
//     reported as usual; the watch plans nothing, records nothing (its state
//     unchanged), says what failed (comment, body, state), and the run is red.
const npmReg2 = path.join(W, 'npm2.json')
write(npmReg2, { versions: Object.fromEntries(['10.9.10', '11.21.1', '11.21.2', '12.2.1'].map((v) => [v, {}])) })
const st26 = watchState(steady)
const f26 = path.join(W, 'state-26.json')
write(f26, st26)
const r26 = runStatus('poll', f26, '--registry-json', npmReg2, '--date', '2026-11-15', '--head', 'h', '--node-index-json', nodeIndex('no-uv-26', [...lines, ['26.15.0', '2026-11-14']]), '--node-uv-dir', path.join(W, 'uv'), '--yarn-registry-json', yarnReg)
const p26 = plan(r26)
check('watch read fails, a new npm release: the npm release is planned, the watch plans nothing and names the failure', watchFailed(r26, /libuv of Node 26\.15\.0/) && p26.todo.length === 1 && p26.todo[0].version === '11.21.2', `${r26.status} ${r26.stdout.slice(0, 300)} ${r26.stderr}`)
const d26 = path.join(tmp, 'w26')
write(path.join(d26, 'state.json'), st26)
write(path.join(d26, 'plan.json'), p26)
if (p26.todo && p26.todo[0]) {
  write(path.join(d26, 'results', 'canary-linux-11.21.2', 'result.json'), { version: '11.21.2', baseline: p26.todo[0].baseline, newMajor: false, suitesPass: true, suites: suitesOk, fingerprint: 'f'.repeat(64), fingerprintError: null, bareAwait: true, known: true, compare: cmpSame(p26.todo[0].baseline) })
}
const rr26 = runStatus('report', path.join(d26, 'state.json'), path.join(d26, 'plan.json'), path.join(d26, 'results'), path.join(d26, 'out'))
const o26 = (f) => fs.readFileSync(path.join(d26, 'out', f), 'utf8')
const s26 = rr26.status === 0 ? JSON.parse(o26('state.json')) : {}
const g26 = rr26.status === 0 ? JSON.parse(o26('flags.json')) : {}
const c26 = rr26.status === 0 && fs.existsSync(path.join(d26, 'out', 'comments')) ? fs.readdirSync(path.join(d26, 'out', 'comments')).map((f) => o26(path.join('comments', f))).join('\n') : ''
check('watch read fails: the npm release verified; the watch state unchanged; the run red; comment, body and state say what failed', rr26.status === 0 && s26.lines[11].verified === '11.21.2' &&
  JSON.stringify(s26.node) === JSON.stringify(st26.node) && JSON.stringify(s26.yarn) === JSON.stringify(st26.yarn) && g26.watchFailed === true && g26.notify === true &&
  /Release watch: not polled \(2026-11-15\)/.test(c26) && /libuv of Node 26\.15\.0/.test(c26) && /the poll of 2026-11-15 failed/.test(o26('body.md')) && s26.watchFailed && s26.watchFailed.date === '2026-11-15', `${rr26.stderr} ${JSON.stringify({ g26, wf: s26.watchFailed })}`)
w = wreport({ ...watchState(steady), watchFailed: { date: '2026-11-14', error: 'x' } }, wplan([uvItem]), { ...linuxOk, 'nodemacos-node26.14.0': ysuites() })
check('watch report: a watch that polls again clears the failure', w.state.watchFailed === null && !/poll of 2026-11-14 failed/.test(w.body), JSON.stringify(w.state.watchFailed))
check('watch report: a Node row of the history has "-" in the tar code column (npm rows only have a tar code)', w.state.history.some((h) => /^Node /.test(h.version) && h.tar === '-') && !w.body.split('\n').some((l) => /^\| [^|]+ \| Node /.test(l) && / \| \? \| /.test(l)), JSON.stringify(w.state.history.slice(0, 2)))
// The npm half's own read failing still fails the poll.
const r26b = runStatus('poll', f26, '--date', '2026-11-15', '--node-index-json', nodeIndex('ordinary', lines), '--node-uv-dir', path.join(W, 'uv'), '--yarn-registry-json', yarnReg)
check('the npm registry read fails: the poll fails', r26b.status !== 0 && /CANARY_NO_NETWORK/.test(r26b.stderr), `${r26b.status} ${r26b.stderr}`)
check('workflow: on a day with nothing to test, the poll job fails the run when the report asks for a person; the plan line survives a failed watch', /record the poll date[\s\S]*?jq -e '\.notify == false' canary\/out\/flags\.json/.test(wf) && /if \.watchError then/.test(wf), '')

// 27. New tar code (Q9 of the C plan): reported in the history, the body and
//     a comment, and it changes no action: a release otherwise green and
//     known is verified, nobody is asked.
const cmpTar = (base, changed) => ({ ...cmpSame(base), tar: { base: 'a'.repeat(64), new: (changed ? 'b' : 'a').repeat(64), changed } })
r = report({ item: item('11.21.2'), linux: { known: true, tarFingerprint: 'b'.repeat(64), compare: cmpTar('11.21.1', true) } })
check('new tar code: reported, no action, verified', r.state.history[0].tar === 'new' && r.state.lines[11].verified === '11.21.2' && r.state.lines[11].action === 'none' && r.flags.notify === false, JSON.stringify({ h: r.state.history[0], flags: r.flags }))
const bodyTar = fs.readFileSync(path.join(tmp, `r${n - 1}`, 'out', 'body.md'), 'utf8')
check('new tar code: the history table has a tar code column', /\| class \| tar code \| caller \|/.test(bodyTar) && /\| known \| new \| same \| none \|/.test(bodyTar), bodyTar.split('\n').filter((l) => l.includes('11.21.2')).join(' / '))
r = report({ item: item('11.21.2'), linux: { known: false, tarFingerprint: 'b'.repeat(64), compare: cmpTar('11.21.1', false) } })
check('tar code the same, shown in the comment of a release that asks', r.state.history[0].tar === 'same' && /tar code \(part of the store key; compared per store entry at run time, no action\): the same \(`bbbbbbbbbbbbbbbb…`\)/.test(r.comment), r.comment.split('\n').filter((l) => l.includes('tar code')).join(' / '))

fs.rmSync(tmp, { recursive: true, force: true })
console.log(`${pass} passed, ${fail} failed`)
process.exit(fail ? 1 : 0)
