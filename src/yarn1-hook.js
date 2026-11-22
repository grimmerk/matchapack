// The Yarn 1 engine's node side (`matchapack yarn`; docs/architecture.md).
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
// argv: <yarn's bin/yarn.js> <yarn args...>, or with
//       MATCHAPACK_YARN_LAUNCHER=corepack: <corepack's dist/yarn.js> <yarn
//       args...> (corepack chooses Yarn and starts it in this process; the
//       hook takes over at that start, Module.runMain)
// env:  MATCHAPACK_YARN_FD         the socket to matchapack (src/yarn1.rs)
//       MATCHAPACK_YARN_PROBE_DIR  a private directory for the start-up probe
//       MATCHAPACK_YARN_HOOK       tests: `off` patches but copies every file;
//                                  `none` installs nothing (what a Yarn that
//                                  no longer calls the hook looks like)
//       MATCHAPACK_YARN_TIMEOUT_MS tests: when an answer counts as slow
//                                  (default 30000)
//       MATCHAPACK_YARN_SIMULATE   tests: `copy-keeps-times` makes the probe's
//                                  copies keep the source's times, as a libuv
//                                  that sets times after writing would;
//                                  `child-copy-keeps-times` the same in a
//                                  workspace command's child only;
//                                  `child-throw`: that child throws before
//                                  Yarn starts (outside decline())
//       MATCHAPACK_YARN_WRAP_CHILD `workspace`: the command is `yarn workspace
//                                  <name> <command> …`, whose own Yarn is a
//                                  child process (`wrapWorkspaceChild`)
//       MATCHAPACK_YARN_CHILD      `1`: this hook was started by another one
//                                  for that child (set by `wrapWorkspaceChild` only)
//       MATCHAPACK_STOP_GRACE_MS   tests: the wait before this process stops
//                                  itself (`stopSelf`; default 50)
// Exit status: Yarn's. 90 before Yarn starts: the hook declined (the reason
// is sent to matchapack and printed); matchapack then runs plain Yarn. In a
// child (MATCHAPACK_YARN_CHILD) the hook runs plain Yarn itself instead: the
// workspace command that started it is already running and would fail.
'use strict'
const fs = require('fs')
const path = require('path')

let sock = null
let yarnStarted = false
// In a child (MATCHAPACK_YARN_CHILD): runs Yarn without the hook, in this
// process, when the hook declines before Yarn has started.
let plainFallback = null

// Yarn stops when matchapack goes away (grimmerk/matchapack-private#64; src/signals.rs),
// as src/inject.js stops npm: when the socket closes while Yarn runs, or
// matchapack says a signal stopped it (`{"stop":"SIGINT"}`), this process
// signals itself (`stopSelf`), so that Yarn's own handling runs as for plain
// Yarn given that signal (SIGTERM: Yarn passes it on to the processes it
// started, then exits 1). Never on top of the same signal this process has
// already received (a group signal, or the one a parent Yarn passed on to a
// workspace command's Yarn): stops and received signals are counted. Seen through process.emit (src/inject.js says
// why): no listener of its own is added.
const STOP_SIGNALS = ['SIGTERM', 'SIGINT', 'SIGHUP']
// How many of each of STOP_SIGNALS this process has received
// (`watchSignals`): its own (from the group, a terminal, a parent) and those
// the hook sent itself. Compared with the stop lines of the same signal
// (`onStop`).
const received = { SIGTERM: 0, SIGINT: 0, SIGHUP: 0 }
const receivedAny = () => received.SIGTERM + received.SIGINT + received.SIGHUP > 0
function watchSignals () {
  const emit = process.emit
  process.emit = function (type) {
    if (STOP_SIGNALS.includes(type)) {
      received[type]++
    }
    return emit.apply(this, arguments)
  }
  // A listener added before this script ran (a NODE_OPTIONS --require
  // preload: node runs preloads before its main script) made Node bind the
  // process.emit of that moment for its signal, past this wrapper. Removed
  // and added again, in order and as they are (rawListeners keeps `once`
  // wrappers), they make Node bind the current process.emit: Node stops a
  // signal's watcher at its last listener and starts it at the first.
  for (const sig of STOP_SIGNALS) {
    const ls = process.rawListeners(sig)
    if (ls.length > 0) {
      process.removeAllListeners(sig)
      for (const l of ls) {
        process.on(sig, l)
      }
    }
  }
}
// After a stop line (`stops`, `lastStop`: matchapack ends right after it,
// by a second signal) or the close (`closed`).
let stops = 0
const stopsOf = { SIGTERM: 0, SIGINT: 0, SIGHUP: 0 }
let lastStop = false
let closed = false
function later (graceMs, fn) {
  setTimeout(() => setImmediate(fn), graceMs)
}
// After the wait, this process signals itself with `sig` when `due()` still
// holds. `say`: a line for stderr when it does stop (none when matchapack
// said why).
function stopSelf (sig, graceMs, say, due) {
  later(graceMs, () => {
    if (due()) {
      if (say) {
        process.stderr.write(say)
      }
      process.kill(process.pid, sig)
    }
  })
}
// A stop line: every signal matchapack gets while this process runs.
// The n-th stop line of signal S: this process signals itself with S unless
// it has already received S n times, its own and the hook's together. So
// each of matchapack's signals reaches it once, from the group with
// matchapack's (counted when it came) or from the hook; a signal of another
// kind, or an earlier one, does not hide it.
function onStop (msg, graceMs) {
  stops++
  if (msg.last === true) {
    lastStop = true
  }
  const sig = msg.stop
  const n = ++stopsOf[sig]
  stopSelf(sig, graceMs, null, () => received[sig] < n)
}
// The socket has closed with no line after the last one. With no stop
// before: SIGTERM (matchapack was killed or crashed), unless a signal came.
// After one: SIGKILL, as plain npm or Yarn gets from a supervisor that sent
// SIGTERM and then SIGKILL; such a close can only mean matchapack was killed
// or crashed after it asked this process to stop. After matchapack's last
// line (a second signal ended it): nothing more.
function onClose (graceMs, what, noun) {
  if (closed) {
    return
  }
  closed = true
  if (lastStop) {
    return
  }
  if (stops === 0) {
    stopSelf('SIGTERM', graceMs, `${what}; stopping ${noun}(SIGTERM)\n`, () => !receivedAny())
    return
  }
  later(graceMs, () => {
    process.stderr.write(`${what} after asking ${noun || 'this Yarn '}to stop; ending ${noun || 'it '}(SIGKILL)\n`)
    process.kill(process.pid, 'SIGKILL')
  })
}

function send (obj) {
  if (sock && !sock.destroyed && sock.writable) {
    sock.write(JSON.stringify(obj) + '\n')
  }
}

function decline (message) {
  process.stderr.write(`matchapack yarn hook: ${message}\n`)
  if (plainFallback) {
    const run = plainFallback
    plainFallback = null
    send({ declined: `in the workspace's Yarn: ${message}` })
    run()
    return new Promise(() => {})
  }
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
  const [target, ...yarnArgs] = process.argv.slice(2)
  const launcher = process.env.MATCHAPACK_YARN_LAUNCHER || ''
  const fd = Number(process.env.MATCHAPACK_YARN_FD)
  const probeDir = process.env.MATCHAPACK_YARN_PROBE_DIR
  const hookMode = process.env.MATCHAPACK_YARN_HOOK || 'on'
  const simulate = process.env.MATCHAPACK_YARN_SIMULATE || ''
  const timeoutMs = Number(process.env.MATCHAPACK_YARN_TIMEOUT_MS) || 30000
  const wrapChild = process.env.MATCHAPACK_YARN_WRAP_CHILD === 'workspace'
  const isChild = process.env.MATCHAPACK_YARN_CHILD === '1'
  const graceMs = Number(process.env.MATCHAPACK_STOP_GRACE_MS) || 50
  // Not for the scripts Yarn runs.
  for (const k of ['MATCHAPACK_YARN_FD', 'MATCHAPACK_YARN_PROBE_DIR', 'MATCHAPACK_YARN_HOOK', 'MATCHAPACK_YARN_SIMULATE', 'MATCHAPACK_YARN_TIMEOUT_MS',
    'MATCHAPACK_YARN_LAUNCHER', 'MATCHAPACK_YARN_WRAP_CHILD', 'MATCHAPACK_YARN_CHILD', 'MATCHAPACK_STOP_GRACE_MS']) {
    delete process.env[k]
  }
  // Before Yarn (or corepack) adds any signal listener.
  watchSignals()
  if (isChild && target) {
    // As the workspace command would have started it: `node <target> <args>`.
    plainFallback = () => {
      // The socket stays open, with no request sent on it: this Yarn still
      // stops when the Yarn relaying for it goes away (`gone`; Yarn 1 passes
      // only SIGTERM on to its children), and ends with it when it ends.
      process.argv = [process.argv[0], target, ...yarnArgs]
      require('module').runMain(target)
    }
  }
  if (!target || !Number.isInteger(fd) || fd < 3 || !probeDir) {
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
      // matchapack was stopped by this signal (src/signals.rs): Yarn stops
      // as if it had got it; matchapack goes on answering until it exits.
      if (msg && STOP_SIGNALS.includes(msg.stop)) {
        onStop(msg, graceMs)
        continue
      }
      settle(msg.id, msg)
    }
  })
  // The other end has gone, and Yarn stops (`stopSelf`; SIGTERM: no signal
  // was said). Here it is matchapack, whose end closes only when its process
  // ends (inject.rs `run_serving`), and its workers with it: what is still
  // asked is Node's to copy, as Yarn asked. In a workspace command's Yarn it
  // is the Yarn that relays its requests (`relayFrom`); matchapack may still
  // be cloning or fixing up the paths asked, so nothing is copied over them:
  // the requests stay unanswered, and no further copy is made (`hooked`),
  // while this Yarn stops.
  const gone = (why) => () => {
    down = down || why
    if (!isChild) {
      for (const id of [...pending.keys()]) {
        settle(id, { ok: false, reason: why })
      }
    }
    onClose(graceMs, isChild
      ? 'matchapack yarn hook: the Yarn that started this workspace command\'s Yarn has gone'
      : 'matchapack yarn hook: matchapack has gone', isChild ? '' : 'Yarn ')
  }
  sock.on('error', gone('server-gone'))
  sock.on('close', gone('server-gone'))
  // A slow answer: later copies are Node's, but this request still waits for
  // its own answer. Copying it now could race matchapack's worker, which may
  // be between its clone and the clone's fix-up (or its removal) on this very
  // path. Only a closed socket ends the wait (matchapack is gone, and its
  // workers with it); a hung matchapack hangs the install, as a hung parent
  // would.
  const TIMEOUT_MS = timeoutMs
  const request = (req) => new Promise((resolve) => {
    if (sock.destroyed || !sock.writable) {
      return resolve({ ok: false, reason: 'server-gone' })
    }
    const id = nextId++
    if (pending.size === 0) {
      sock.ref()
    }
    const timer = setTimeout(() => {
      down = down || 'server-slow'
    }, TIMEOUT_MS)
    timer.unref()
    pending.set(id, { resolve, timer })
    send({ id, ...req })
  })

  // A child's socket (fd 3 of the Yarn a workspace command starts): each
  // request is passed on to matchapack as one of this process's own, and
  // the answer back under the child's id; a message without an id (hello,
  // link step, decline) is passed on as it is.
  const relayFrom = (s) => {
    s.setEncoding('utf8')
    s.on('error', () => {})
    let b = ''
    s.on('data', (chunk) => {
      b += chunk
      let i
      while ((i = b.indexOf('\n')) >= 0) {
        const line = b.slice(0, i)
        b = b.slice(i + 1)
        let msg
        try {
          msg = JSON.parse(line)
        } catch {
          continue
        }
        if (msg === null || typeof msg !== 'object') {
          continue
        }
        if (msg.id === undefined) {
          send(msg)
          continue
        }
        const { id, ...req } = msg
        request(req).then((answer) => {
          if (!s.destroyed && s.writable) {
            s.write(JSON.stringify(Object.assign({}, answer, { id })) + '\n')
          }
        })
      }
    })
  }

  // `yarn workspace <name> <command> …` (MATCHAPACK_YARN_WRAP_CHILD): Yarn's
  // workspace command starts `node <its bundle> <command> …` in the
  // workspace's directory (src/cli/commands/workspace.js: NODE_BIN_PATH,
  // YARN_BIN_PATH, stdio 'inherit') and waits for it; that Yarn links. That
  // one spawn is made `node <this hook> <its bundle> <command> …` with one
  // more descriptor, fd 3: a socket pair to this process (libuv makes stdio
  // 'pipe' a socket pair). The child's hook runs that Yarn as this one runs
  // Yarn (probe, first copies, clones; in a child a decline runs plain
  // Yarn), and this process relays its requests (relayFrom). The rest of the
  // spawn is Yarn's: the program, the bundle as the main module, the
  // directory, the environment (plus the hook's variables, which the child's
  // hook removes before Yarn starts), stdin, stdout, stderr.
  const wrapWorkspaceChild = (bundle) => {
    const cp = require('child_process')
    const spawn = cp.spawn
    let wrapped = false
    cp.spawn = function (file, args, options) {
      if (wrapped || file !== process.execPath || !Array.isArray(args) || args[0] !== bundle ||
          !options || typeof options.cwd !== 'string' || options.stdio !== 'inherit') {
        return spawn.apply(this, arguments)
      }
      wrapped = true
      const env = Object.assign({}, options.env || process.env, {
        MATCHAPACK_YARN_FD: '3',
        MATCHAPACK_YARN_PROBE_DIR: probeDir,
        MATCHAPACK_YARN_CHILD: '1',
        MATCHAPACK_YARN_HOOK: hookMode,
        MATCHAPACK_YARN_TIMEOUT_MS: String(timeoutMs),
        MATCHAPACK_STOP_GRACE_MS: String(graceMs),
      }, simulate ? { MATCHAPACK_YARN_SIMULATE: simulate } : {})
      const child = spawn.call(this, file, [__filename, ...args], Object.assign({}, options, { env, stdio: ['inherit', 'inherit', 'inherit', 'pipe'] }))
      if (child.stdio && child.stdio[3]) {
        relayFrom(child.stdio[3])
        // Yarn waits for the child's 'close', which needs fd 3 closed too.
        // Where the child's fd 3 is not close-on-exec (libuv before 1.45,
        // Node before 20, on macOS), a process a lifecycle script leaves
        // behind can hold it open and Yarn would wait forever where plain
        // Yarn does not. So once the child has exited, its socket is
        // destroyed (after a moment, for its last messages still in
        // flight).
        const fd3 = child.stdio[3]
        child.on('exit', () => {
          setTimeout(() => {
            if (!fd3.destroyed) {
              fd3.destroy()
            }
          }, 1000).unref()
        })
      }
      return child
    }
  }

  // Starts Yarn 1 from its bin/yarn.js (in a workspace command's child: from
  // its bundle, lib/cli.js, as that command starts it), with the hook
  // installed.
  const start = async (yarnJs) => {
    // The bundle bin/yarn.js loads (`require(__dirname + '/../lib/cli')`).
    let bundle
    let pkg = {}
    try {
      pkg = JSON.parse(fs.readFileSync(path.join(path.dirname(yarnJs), '..', 'package.json'), 'utf8'))
    } catch {}
    const yarnVersion = typeof pkg.version === 'string' ? pkg.version : null
    // matchapack checked this for a Yarn it found itself; corepack's choice
    // is checked here (a Yarn 2+ release is a single file, no package.json).
    if (pkg.name !== 'yarn' || !/^1\./.test(yarnVersion || '')) {
      return decline(`${yarnJs} is not Yarn 1's npm package (${launcher ? `${launcher} chose ` : ''}yarn ${yarnVersion || 'of an unknown version'})`)
    }
    // The version floor, as src/yarn1.rs `yarn1_clones` (a Yarn corepack
    // chose is checked only here): an older Yarn 1 asks for no clone. The
    // minor number as a number.
    const minor = Number((/^1\.(\d+)\./.exec(yarnVersion) || [])[1])
    if (!(minor >= 10)) {
      return decline(`yarn ${yarnVersion} is older than 1.10.0, the first Yarn 1 that asks for a clone: nothing could be cloned`)
    }
    try {
      bundle = require.resolve(path.join(path.dirname(yarnJs), '..', 'lib', 'cli'))
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
      p = probe(probeDir, isChild && simulate === 'child-copy-keeps-times' ? 'copy-keeps-times' : simulate)
    } catch (e) {
      return decline(`the start-up probe of Node's copy failed: ${e.message}`)
    }
    if (isChild && simulate === 'child-throw') {
      // Tests only: an error outside decline(), before Yarn starts (as a
      // failed removal of the probe's directory would be).
      fs.rmSync(p.dir, { recursive: true, force: true })
      throw new Error('simulated (MATCHAPACK_YARN_SIMULATE=child-throw)')
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
      if (down && isChild && sock.destroyed) {
        // (`gone`) No copy at all: this Yarn is stopping.
        return
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
        if (answer.reason === 'server-gone') {
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
          // This link step's counts: matchapack adds them up (`yarn
          // upgrade-interactive` runs several link steps in one process).
          send({ link_step: 'end', counts })
          for (const k of Object.keys(counts)) {
            delete counts[k]
          }
        })
      },
    })
    if (hookMode !== 'none') {
      fs.copyFile = hooked
      global.experimentalYarnHooks = hooks
      if (wrapChild) {
        wrapWorkspaceChild(bundle)
      }
    }

    yarnStarted = true
    plainFallback = null
    // `child`, `cwd`: a workspace command's Yarn says where it runs (its
    // cache options are relative to that directory: src/yarn1.rs `run`).
    send({ hello: true, yarn: yarnVersion, yarn_js: yarnJs, node: process.version, node_path: process.execPath, first: firstN, facts: p.facts, hook: hookMode, child: isChild, cwd: process.cwd() })
    // Run Yarn's bin/yarn.js as the main module, as `node yarn.js` (or the
    // `yarn` symlink, which Node resolves to it) would, and as corepack starts
    // it: argv[1] and process.mainModule are Yarn's own. Yarn gives its
    // lifecycle scripts `npm_execpath = process.mainModule.filename`, so a
    // script running `node "$npm_execpath" …` must reach Yarn, not this hook.
    process.argv = [process.argv[0], yarnJs, ...yarnArgs]
    require('module').runMain(yarnJs)
  }

  if (launcher !== 'corepack') {
    return start(target)
  }
  // corepack's launcher (dist/yarn.js) picks the project's Yarn
  // (packageManager, its defaults), installs it into its cache when needed,
  // and starts it in this process: `process.nextTick(Module.runMain, <the
  // chosen Yarn's bin/yarn.js>)` after setting process.argv. That call is
  // where the hook takes over, with the Yarn corepack chose; a corepack
  // error before it is corepack's own (its message, its exit status), as
  // with plain corepack.
  const Module = require('module')
  const runMain = Module.runMain
  Module.runMain = function (chosen) {
    Module.runMain = runMain
    start(String(chosen)).catch(beforeYarn)
  }
  process.argv = [process.argv[0], target, ...yarnArgs]
  runMain.call(Module, target)
}

function beforeYarn (e) {
  if (yarnStarted) {
    throw e
  }
  process.stderr.write(`matchapack yarn hook: before Yarn started: ${e && e.stack ? e.stack : e}\n`)
  if (plainFallback) {
    const run = plainFallback
    plainFallback = null
    // As decline() does: the run is then reported as the workspace's Yarn
    // run plain, not as a link step the hook never saw.
    send({ declined: `in the workspace's Yarn: an error before Yarn started: ${e && e.message ? e.message : e}` })
    return run()
  }
  process.exit(90)
}

if (require.main === module) {
  main().catch(beforeYarn)
} else {
  module.exports = { copyConcurrency, probe }
}
