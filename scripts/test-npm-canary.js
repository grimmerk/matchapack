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
function report ({ item, linux, macos }) {
  const d = path.join(tmp, `r${n++}`)
  write(path.join(d, 'state.json'), state0)
  write(path.join(d, 'plan.json'), { date: '2026-11-14', month: null, head: 'h', supported: [10, 11, 12], newMajor: null, skipped: [], todo: [item] })
  if (linux) {
    write(path.join(d, 'results', `canary-linux-${item.version}`, 'result.json'), { version: item.version, baseline: item.baseline, newMajor: !!item.newMajor, suitesPass: true, suites: suitesOk, fingerprint: 'f'.repeat(64), fingerprintError: null, bareAwait: true, ...linux })
  }
  if (macos) {
    write(path.join(d, 'results', `canary-macos-${item.version}`, 'suites.json'), { suites: macos })
  }
  run('report', path.join(d, 'state.json'), path.join(d, 'plan.json'), path.join(d, 'results'), path.join(d, 'out'))
  const out = path.join(d, 'out')
  return {
    state: JSON.parse(fs.readFileSync(path.join(out, 'state.json'), 'utf8')),
    flags: JSON.parse(fs.readFileSync(path.join(out, 'flags.json'), 'utf8')),
    comment: fs.existsSync(path.join(out, 'comment.md')) ? fs.readFileSync(path.join(out, 'comment.md'), 'utf8') : '',
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

fs.rmSync(tmp, { recursive: true, force: true })
console.log(`${pass} passed, ${fail} failed`)
process.exit(fail ? 1 : 0)
