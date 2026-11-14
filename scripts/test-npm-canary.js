#!/usr/bin/env node
// Offline tests of the npm canary's decisions (scripts/npm-canary.js): poll
// and report on hand-made states, plans and job results. No network, no npm.
//   node scripts/test-npm-canary.js
// Above all the fail-closed rules: a release is verified only when every
// check it needed left a result.
'use strict'
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { execFileSync } = require('node:child_process')

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
const run = (...args) => execFileSync(process.execPath, [canary, ...args], { encoding: 'utf8' })

const suitesOk = { 'test-inject': { status: 0, pass: 70, fail: 0, skip: 0, failed: [] }, 'verify-npm': { status: 0, pass: 14, fail: 0, skip: 0, failed: [] } }
const cmpSame = (base) => ({ base, classChanged: false, callerChanged: false, deps: {}, changed: [], changelog: null })
const L = (newest, suites, head) => ({ newest, checked: '2026-11-13', suites, head, verified: newest, classText: 'known', caller: 'same', action: 'none' })
const state0 = { version: 1, polled: '2026-11-13', monthly: '2026-11', lines: { 10: L('10.9.10', 'pass', 'h'), 11: L('11.21.1', 'pass', 'h'), 12: L('12.2.1', 'pass', 'h') }, history: [] }

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
check('known, Linux green, compared: verified, nobody asked', r.state.lines[11].verified === '11.21.2' && r.state.lines[11].suites === 'pass' && !r.flags.attention && !r.flags.red, JSON.stringify(r.state.lines[11]))

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
check('macOS required and green: pass, verified, a person asked about the new text', r.state.lines[11].suites === 'pass' && r.state.lines[11].verified === '11.21.2' && r.flags.attention && !r.flags.red, JSON.stringify(r.state.lines[11]))

// 5. The planned comparison missing (baseline install or inspection
//    failed): incomplete, not verified, a comment, and the next poll
//    runs it again.
r = report({ item: item('11.21.2'), linux: { known: true } })
check('comparison missing: incomplete, not verified, red, a comment', r.state.lines[11].suites === 'incomplete' && r.state.lines[11].verified === '11.21.1' && r.state.lines[11].caller === 'missing' && r.flags.red && r.flags.attention && /Comparison with 11\.21\.1: missing/.test(r.comment), JSON.stringify({ line: r.state.lines[11], flags: r.flags }))
const reg = path.join(tmp, 'registry.json')
write(reg, { versions: Object.fromEntries(['10.9.10', '11.21.1', '11.21.2', '12.2.1'].map((v) => [v, {}])) })
write(path.join(tmp, 'state-after.json'), r.state)
const plan = JSON.parse(run('poll', path.join(tmp, 'state-after.json'), '--registry-json', reg, '--date', '2026-11-15', '--head', 'h'))
check('comparison missing: the next poll, same HEAD, runs the release again', plan.todo.some((t) => t.version === '11.21.2' && /incomplete/.test(t.reason)), JSON.stringify(plan.todo))

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
  return JSON.parse(run('poll', path.join(tmp, 'state13.json'), '--registry-json', reg, '--date', '2026-11-15', '--head', head)).todo.filter((t) => t.version === '13.0.0')
}
let t13 = poll13('incomplete', 'h')
check('new major, incomplete: the next poll runs it again', t13.length === 1 && t13[0].newMajor === true, JSON.stringify(t13))
t13 = poll13('fail', 'h2')
check('new major, red, matchapack changed: run again', t13.length === 1 && t13[0].newMajor === true, JSON.stringify(t13))
t13 = poll13('fail', 'h')
check('new major, red, matchapack unchanged: not run again', t13.length === 0, JSON.stringify(t13))

fs.rmSync(tmp, { recursive: true, force: true })
console.log(`${pass} passed, ${fail} failed`)
process.exit(fail ? 1 : 0)
