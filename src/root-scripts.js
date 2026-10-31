// Run the root project's lifecycle scripts exactly as `npm ci` does after
// reify (npm/lib/commands/ci.js): these seven events, in this order, through
// npm's own @npmcli/run-script (NOT `npm run`, which would add pre/post hooks).
// Started via `npm exec` so the child sees npm's npm_config_* environment.
// argv: <npm install dir> <project root>
const path = require('path')
const [npmDir, where] = process.argv.slice(2)
const scriptShell = process.env.npm_config_script_shell || undefined
const runScript = require(path.join(npmDir, 'node_modules', '@npmcli', 'run-script'))
const events = ['preinstall', 'install', 'postinstall', 'prepublish', 'preprepare', 'prepare', 'postprepare']
;(async () => {
  for (const event of events) {
    await runScript({ path: where, args: [], scriptShell, stdio: 'inherit', event })
  }
})().catch(e => {
  console.error(e && e.message ? e.message : e)
  process.exit(typeof e.code === 'number' ? e.code : 1)
})
