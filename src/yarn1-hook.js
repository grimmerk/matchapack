// The Yarn 1 engine's node side (`matchapack yarn`; NOTES "Yarn 1 engine").
// Runs the user's Yarn 1 in this process, the way its bin/yarn.js does, with
// one behaviour changed: while Yarn's link step runs (Yarn's own extension
// point, global.experimentalYarnHooks.linkStep, tells when), each
// fs.copyFile(src, dest, COPYFILE_FICLONE) Yarn issues to copy a file from its
// cache into node_modules is answered by a copy-on-write clone of the cache
// file, made by the matchapack process that started this one (Node cannot
// clone on macOS: COPYFILE_FICLONE_FORCE gives ENOSYS). Everything else is
// Yarn's: which files to copy, its skip rule for unchanged files, extraneous
// removal, bin links, scripts, .yarn-integrity, and its time fix-up
// (fixTimes), which runs after a clone exactly as after a copy.
//
// A clone is made to look like the copy Node would have made here. What
// Node's copy keeps is measured at start (`probe`), not assumed: libuv's copy
// sets the destination's times and owner from the source BEFORE it writes
// the data, so an empty file keeps the source's times and a non-empty one
// ends with mtime = now (Node >= 22; Node 20's libuv sets nothing), and the
// group follows the source under Node >= 22 only. The facts go to matchapack
// in the hello; it applies them to every clone. Yarn decides once per
// process, on its first copies, whether copies keep times (fixTimes); so the
// first N copies of the link step, N being Yarn's own copy concurrency read
// from the bundle that runs, are left to Node, and that decision is plain
// Yarn's.
//
// argv: <yarn's bin/yarn.js> <yarn args...>
// env:  MATCHAPACK_YARN_FD         the socket to matchapack (src/yarn1.rs)
//       MATCHAPACK_YARN_PROBE_DIR  a private directory for the start-up probe
//       MATCHAPACK_YARN_HOOK       tests: `off` patches but copies every file;
//                                  `none` installs nothing (what a Yarn that
//                                  no longer calls the hook looks like)
//       MATCHAPACK_YARN_SIMULATE   tests: `copy-keeps-times` makes the probe's
//                                  copies keep the source's times, as a libuv
//                                  that sets times after writing would
// Exit status: Yarn's. 90 before Yarn starts: the hook declined (the reason
// is sent to matchapack and printed); matchapack then runs plain Yarn.
'use strict'
const fs = require('fs')
const path = require('path')

let sock = null
let yarnStarted = false

function send (obj) {
  if (sock && !sock.destroyed && sock.writable) {
    sock.write(JSON.stringify(obj) + '\n')
  }
}

function decline (message) {
  process.stderr.write(`matchapack yarn hook: ${message}\n`)
  if (sock && !sock.destroyed && sock.writable) {
    // Exit once the reason is written: matchapack logs it.
    sock.write(JSON.stringify({ declined: message }) + '\n', () => process.exit(90))
    setTimeout(() => process.exit(90), 2000).unref()
    return new Promise(() => {})
  }
  process.exit(90)
}

// Yarn's copy concurrency: `CONCURRENT_QUEUE_ITEMS = fs.copyFile ? 128 : 4`
// (yarn-classic src/util/fs.js), from the bundle that will run. Only copies
// started before Yarn's time probe has its answer can take part in it, and
// Yarn starts at most this many at once.
function copyConcurrency (bundle) {
  const text = fs.readFileSync(bundle, 'utf8')
  const re = /\bCONCURRENT_QUEUE_ITEMS\s*=\s*[^;]*?\.copyFile\s*\?\s*(\d+)\s*:\s*\d+\s*;/g
  const found = [...text.matchAll(re)]
  if (found.length !== 1) {
    return null
  }
  const n = Number(found[0][1])
  return Number.isInteger(n) && n > 0 ? n : null
}

// What Node's own copy keeps, measured with Node's own fs.copyFileSync and
// the flag Yarn passes: an empty and a 1-byte file with an old time, each
// copied once. `group`: the copy takes the source's group (null when the
// user is in one group only: then the question cannot arise).
function probe (dir, simulate) {
  const d = fs.mkdtempSync(path.join(dir, 'probe-'))
  const dirGid = fs.statSync(d).gid
  const other = (process.getgroups ? process.getgroups() : []).find(g => g !== dirGid)
  const facts = {}
  let groupTested = false
  const files = {}
  for (const [name, data] of [['empty', ''], ['nonempty', 'x']]) {
    const src = path.join(d, name)
    const dst = src + '.copy'
    fs.writeFileSync(src, data)
    if (other !== undefined) {
      try {
        fs.chownSync(src, process.getuid(), other)
      } catch {}
    }
    // 1000000000.25 s is exact in binary: the set time is known to the ns.
    fs.utimesSync(src, 1000000000.25, 1000000000.25)
    const s = fs.statSync(src, { bigint: true })
    fs.copyFileSync(src, dst, fs.constants.COPYFILE_FICLONE)
    if (simulate === 'copy-keeps-times') {
      fs.utimesSync(dst, 1000000000.25, 1000000000.25)
    }
    const c = fs.statSync(dst, { bigint: true })
    facts[name === 'empty' ? 'empty_atime' : 'atime'] = c.atimeNs === s.atimeNs
    facts[name === 'empty' ? 'empty_mtime' : 'mtime'] = c.mtimeNs === s.mtimeNs
    if (name === 'nonempty' && s.gid !== BigInt(dirGid)) {
      groupTested = true
      facts.group = c.gid === s.gid
    }
    files[name] = { src, dst }
  }
  if (!groupTested) {
    facts.group = null
  }
  return { facts, dir: d, src: files.nonempty.src, dst: files.nonempty.dst }
}

async function main () {
  const [yarnJs, ...yarnArgs] = process.argv.slice(2)
  const fd = Number(process.env.MATCHAPACK_YARN_FD)
  const probeDir = process.env.MATCHAPACK_YARN_PROBE_DIR
  const hookMode = process.env.MATCHAPACK_YARN_HOOK || 'on'
  const simulate = process.env.MATCHAPACK_YARN_SIMULATE || ''
  // Not for the scripts Yarn runs.
  for (const k of ['MATCHAPACK_YARN_FD', 'MATCHAPACK_YARN_PROBE_DIR', 'MATCHAPACK_YARN_HOOK', 'MATCHAPACK_YARN_SIMULATE']) {
    delete process.env[k]
  }
  if (!yarnJs || !Number.isInteger(fd) || fd < 3 || !probeDir) {
    return decline('usage: MATCHAPACK_YARN_FD=<fd> MATCHAPACK_YARN_PROBE_DIR=<dir> node yarn1-hook.js <yarn.js> <args...>')
  }

  const net = require('net')
  try {
    sock = new net.Socket({ fd, readable: true, writable: true })
  } catch (e) {
    return decline(`cannot open the socket to matchapack: ${e.message}`)
  }
  sock.setEncoding('utf8')
  // The socket keeps this process alive only while an answer is awaited
  // (Yarn ends by setting process.exitCode and letting the loop drain).
  sock.unref()
  const pending = new Map()
  let nextId = 0
  let buf = ''
  // Set when no further request can succeed; later copies are Node's.
  let down = hookMode === 'off' ? 'hook-off' : null
  const settle = (id, value) => {
    const p = pending.get(id)
    if (!p) {
      return
    }
    pending.delete(id)
    clearTimeout(p.timer)
    if (pending.size === 0) {
      sock.unref()
    }
    p.resolve(value)
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
      settle(msg.id, msg)
    }
  })
  const gone = (why) => () => {
    down = down || why
    for (const id of [...pending.keys()]) {
      settle(id, { ok: false, reason: why })
    }
  }
  sock.on('error', gone('server-gone'))
  sock.on('close', gone('server-gone'))
  // An answer that does not come is a copy, never a wait for ever. matchapack
  // answers a request that waited in its queue for seconds without cloning
  // (src/yarn1.rs), so a late clone cannot land after this copy.
  const TIMEOUT_MS = 30000
  const request = (req) => new Promise((resolve) => {
    if (sock.destroyed || !sock.writable) {
      return resolve({ ok: false, reason: 'server-gone' })
    }
    const id = nextId++
    if (pending.size === 0) {
      sock.ref()
    }
    const timer = setTimeout(() => {
      down = down || 'server-timeout'
      settle(id, { ok: false, reason: 'server-timeout' })
    }, TIMEOUT_MS)
    timer.unref()
    pending.set(id, { resolve, timer })
    send({ id, ...req })
  })

  // The bundle bin/yarn.js loads (`require(__dirname + '/../lib/cli')`).
  let bundle
  let yarnVersion = null
  try {
    bundle = require.resolve(path.join(path.dirname(yarnJs), '..', 'lib', 'cli'))
    yarnVersion = JSON.parse(fs.readFileSync(path.join(path.dirname(yarnJs), '..', 'package.json'), 'utf8')).version
  } catch (e) {
    return decline(`cannot find Yarn's bundle next to ${yarnJs}: ${e.message}`)
  }
  let firstN
  try {
    firstN = copyConcurrency(bundle)
  } catch (e) {
    return decline(`cannot read ${bundle}: ${e.message}`)
  }
  if (firstN === null) {
    return decline(`${bundle} does not define CONCURRENT_QUEUE_ITEMS as expected (fs.copyFile ? N : M): this Yarn is not handled`)
  }

  let p
  try {
    p = probe(probeDir, simulate)
  } catch (e) {
    return decline(`the start-up probe of Node's copy failed: ${e.message}`)
  }
  try {
    if (p.facts.mtime) {
      return decline(`Node ${process.version}'s fs.copyFile kept a non-empty file's mtime: its copy has changed (a libuv that clones or keeps times); plain Yarn`)
    }
    const answer = await request({ probe: [p.src, p.dst] })
    if (answer.shares) {
      return decline(`Node ${process.version}'s fs.copyFile shares blocks with its source: plain Yarn already clones here`)
    }
  } finally {
    fs.rmSync(p.dir, { recursive: true, force: true })
  }

  // Counted here, sent at the end of each link step.
  const counts = {}
  const note = (why) => { counts[why] = (counts[why] || 0) + 1 }
  const FICLONE = fs.constants.COPYFILE_FICLONE
  const origCopyFile = fs.copyFile
  let active = false
  let linkCopies = 0

  const hooked = function copyFile (src, dest, flags, cb) {
    if (!active || typeof flags === 'function' || typeof cb !== 'function') {
      return origCopyFile.apply(this, arguments)
    }
    linkCopies++
    if (linkCopies <= firstN) {
      note('first-copies')
      return origCopyFile.apply(this, arguments)
    }
    if (flags !== FICLONE || typeof src !== 'string' || typeof dest !== 'string' ||
        !path.isAbsolute(src) || !path.isAbsolute(dest)) {
      note('not-eligible')
      return origCopyFile.apply(this, arguments)
    }
    if (down) {
      note(down)
      return origCopyFile.apply(this, arguments)
    }
    request({ src, dest }).then((answer) => {
      if (answer.ok) {
        return cb(null)
      }
      if (answer.fatal) {
        down = down || answer.reason || 'fatal'
      }
      // Counted by matchapack when it refused; here when it never answered.
      if (answer.reason === 'server-gone' || answer.reason === 'server-timeout') {
        note(answer.reason)
      }
      // Node's own copy, as Yarn asked; it opens and truncates whatever is
      // at dest.
      origCopyFile.call(fs, src, dest, flags, cb)
    })
  }

  // Yarn 1's own extension point (src/util/hooks.js): each install step runs
  // through hook(fn). Scopes the patch to the link step and tells "the hook
  // never fired" apart from "nothing to copy".
  const prev = global.experimentalYarnHooks
  const hooks = Object.assign({}, prev, {
    linkStep (fn, context) {
      send({ link_step: 'start' })
      active = true
      const run = prev && typeof prev.linkStep === 'function' ? prev.linkStep(fn, context) : fn()
      return Promise.resolve(run).finally(() => {
        active = false
        send({ link_step: 'end', counts })
      })
    },
  })
  if (hookMode !== 'none') {
    fs.copyFile = hooked
    global.experimentalYarnHooks = hooks
  }

  yarnStarted = true
  send({ hello: true, yarn: yarnVersion, node: process.version, first: firstN, facts: p.facts, hook: hookMode })
  // Run Yarn's CLI the way its bin/yarn.js is run: argv as if invoked directly.
  process.argv = [process.argv[0], yarnJs, ...yarnArgs]
  require(yarnJs)
}

if (require.main === module) {
  main().catch((e) => {
    if (yarnStarted) {
      throw e
    }
    process.stderr.write(`matchapack yarn hook: before Yarn started: ${e && e.stack ? e.stack : e}\n`)
    process.exit(90)
  })
} else {
  module.exports = { copyConcurrency, probe }
}
