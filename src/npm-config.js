// npm's configuration, as npm itself loads it (@npmcli/config with npm's own
// definitions; the same sources and precedence as `npm ci` run here):
//   changed:   every key this npm defines that a source other than npm's
//              defaults (global, user, project npmrc, environment) sets to a
//              value different from its default, with that source. Keys npm
//              does not define (another package manager's settings, scoped
//              registries, per-host tokens) are left out: npm ignores the
//              first and uses the others only to fetch;
//   effective: the value of every key npm defines, defaults included (NODE_ENV
//              changes the default of `omit`, for instance).
// Prints one JSON object. Values from `changed` are not printed: npmrc values
// can be tokens.
// argv: <npm install dir>
const path = require('path')
const [npmDir] = process.argv.slice(2)
const configDir = path.join(npmDir, 'node_modules', '@npmcli', 'config')
const Config = require(configDir)
const { definitions, flatten, nerfDarts, shorthands } = require(path.join(configDir, 'lib', 'definitions'))

;(async () => {
  const config = new Config({
    npmPath: npmDir,
    definitions,
    flatten,
    nerfDarts,
    shorthands,
    argv: [process.argv[0], path.join(npmDir, 'bin', 'npm-cli.js'), 'ci'],
    warn: false,
  })
  await config.load()
  const effective = {}
  for (const key of Object.keys(definitions)) {
    effective[key] = config.get(key)
  }
  const changed = {}
  for (const [where, layer] of config.data.entries()) {
    if (where === 'default' || where === 'builtin') {
      continue
    }
    for (const key of Object.keys(layer.data)) {
      if (Object.hasOwn(definitions, key) &&
          JSON.stringify(effective[key]) !== JSON.stringify(definitions[key].default)) {
        changed[key] = where
      }
    }
  }
  process.stdout.write(JSON.stringify({ changed, effective }))
})().catch((e) => {
  process.stderr.write(`matchapack npm-config helper: ${e && e.message ? e.message : e}\n`)
  process.exit(1)
})
