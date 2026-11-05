// What `npm ci` does after placing the files, in npm's own code and in one
// process (npm/lib/commands/ci.js and arborist reify):
//   1. dependency scripts and bin links: npm's rebuild command, for the
//      packages thinclone placed;
//   2. the hidden lockfile (node_modules/.package-lock.json), whose text
//      thinclone computed: written after the dependency scripts, as reify does,
//      since npm compares its mtime with the package folders;
//   3. the root project's seven lifecycle events, in ci.js's order, through
//      npm's @npmcli/run-script (NOT `npm run`, which adds pre/post hooks).
// npm is loaded in this process with the same arguments as the user's
// `npm ci` (so scripts get the same npm_config_* environment) and
// npm_command=ci, which is what npm ci's own scripts see. Running `npm
// rebuild` and `npm exec` instead made it `rebuild` and `exec`.
// argv: <npm install dir> <job file>. The job file (JSON) holds root, specs,
// hiddenLockfile (path of the text to write, or null), status (path) and
// npmArgs. The status file gets {ok, failed: "rebuild"|"scripts"|null,
// message, code, phase, secs: {rebuild, scripts}}. It is written once with
// phase "scripts" before the root scripts start: @npmcli/run-script re-raises
// a signal that killed a script on this process, which then dies without
// reaching its own status write, and thinclone must still treat that as a
// root script failure (reported, not retried), as npm ci would die with it.
// All files live in a private (0700) directory thinclone made for this run.
const path = require('path')
const fs = require('fs')
const [npmDir, jobFile] = process.argv.slice(2)
const job = JSON.parse(fs.readFileSync(jobFile, 'utf8'))
const status = { ok: false, failed: null, message: '', code: 0, phase: 'rebuild', secs: { rebuild: 0, scripts: 0 } }
const writeStatus = () => fs.writeFileSync(job.status, JSON.stringify(status))
const now = () => Number(process.hrtime.bigint()) / 1e9

process.chdir(job.root)
process.argv = [process.argv[0], path.join(npmDir, 'bin', 'npm-cli.js'), 'ci', ...job.npmArgs]
const Npm = require(path.join(npmDir, 'lib', 'npm.js'))
const Rebuild = require(path.join(npmDir, 'lib', 'commands', 'rebuild.js'))
const runScript = require(path.join(npmDir, 'node_modules', '@npmcli', 'run-script'))

;(async () => {
  const npm = new Npm()
  await npm.load()
  process.env.npm_command = 'ci'

  let t = now()
  if (job.specs.length) {
    // npm ci prints nothing for this step; the rebuild command ends with
    // `rebuilt dependencies successfully` (proc-log output event): drop
    // exactly that line, keep everything else (warnings, script output).
    const emit = process.emit
    process.emit = function (ev, level, ...args) {
      if (ev === 'output' && level === 'standard' && args[0] === 'rebuilt dependencies successfully') {
        return false
      }
      return emit.call(this, ev, level, ...args)
    }
    try {
      await new Rebuild(npm).exec(job.specs)
    } catch (e) {
      status.failed = 'rebuild'
      status.message = e && e.message ? e.message : String(e)
      return
    } finally {
      process.emit = emit
    }
  }
  status.secs.rebuild = now() - t

  if (job.hiddenLockfile) {
    fs.writeFileSync(path.join(job.root, 'node_modules', '.package-lock.json'), fs.readFileSync(job.hiddenLockfile))
  }

  t = now()
  status.phase = 'scripts'
  writeStatus()
  if (!npm.config.get('ignore-scripts')) {
    const scriptShell = npm.config.get('script-shell') || undefined
    const events = ['preinstall', 'install', 'postinstall', 'prepublish', 'preprepare', 'prepare', 'postprepare']
    for (const event of events) {
      try {
        await runScript({ path: job.root, args: [], scriptShell, stdio: 'inherit', event })
      } catch (e) {
        status.failed = 'scripts'
        status.message = e && e.message ? e.message : String(e)
        status.code = typeof e.code === 'number' ? e.code : 1
        return
      }
    }
  }
  status.secs.scripts = now() - t
  status.ok = true
})().catch((e) => {
  status.failed = status.failed || 'rebuild'
  status.message = e && e.message ? e.message : String(e)
}).finally(async () => {
  writeStatus()
  // Let npm's own output (warnings, banners) reach the terminal before
  // exiting: process.exit does not wait for pending writes to a pipe.
  await new Promise((resolve) => process.stdout.write('', resolve))
  await new Promise((resolve) => process.stderr.write('', resolve))
  process.exit(status.ok ? 0 : 1)
})
