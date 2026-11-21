#!/usr/bin/env node
// Builds the npm packages of one release from dist's archives:
//
//   node scripts/npm-packages.js --version 0.1.0 --archives <dir> --out <dir>
//
// <dir> holds dist's matchapack-<target>.tar.xz archives and their .sha256
// files (the release workflow's artifacts). <out> gets one directory per
// package, ready for `npm publish <dir>`, and the script prints them in
// publish order: the four platform packages, then `matchapack`.
//
// The layout is the one esbuild and Biome use, without their install script:
// each platform package (@matchapack/darwin-arm64, …) holds one binary and
// says by its "os", "cpu" (and "libc") fields where it runs, so npm installs
// only the matching one; `matchapack` lists all four as optional dependencies
// of its exact version and links bin/matchapack, a POSIX sh launcher
// (npm/launcher.sh) that execs the binary. Nothing runs at install time and
// nothing is downloaded after it.
'use strict'
const fs = require('fs')
const path = require('path')
const crypto = require('crypto')
const { execFileSync } = require('child_process')

const SCOPE = '@matchapack'
const REPO = 'https://github.com/grimmerk/matchapack'
// Rust target → npm platform package (Node's process.platform-process.arch).
const TARGETS = {
  'aarch64-apple-darwin': { plat: 'darwin-arm64', os: 'darwin', cpu: 'arm64' },
  'x86_64-apple-darwin': { plat: 'darwin-x64', os: 'darwin', cpu: 'x64' },
  'aarch64-unknown-linux-gnu': { plat: 'linux-arm64', os: 'linux', cpu: 'arm64', libc: 'glibc' },
  'x86_64-unknown-linux-gnu': { plat: 'linux-x64', os: 'linux', cpu: 'x64', libc: 'glibc' },
}

function die(msg) {
  console.error(`npm-packages: ${msg}`)
  process.exit(1)
}

function args() {
  const a = {}
  const argv = process.argv.slice(2)
  for (let i = 0; i < argv.length; i += 2) {
    const k = argv[i]
    if (!['--version', '--archives', '--out'].includes(k) || argv[i + 1] === undefined) {
      die('usage: npm-packages.js --version <x.y.z> --archives <dir> --out <dir>')
    }
    a[k.slice(2)] = argv[i + 1]
  }
  if (!a.version || !a.archives || !a.out) die('usage: npm-packages.js --version <x.y.z> --archives <dir> --out <dir>')
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(a.version)) die(`not a version: ${a.version}`)
  return a
}

// The crate's description and keywords, so npm says what crates.io says.
function cargoMeta(root) {
  const toml = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8')
  const pkg = toml.split(/^\[/m).find((s) => s.startsWith('package]'))
  const str = (k) => (pkg.match(new RegExp(`^${k} = "(.*)"$`, 'm')) || die(`Cargo.toml: no ${k}`))[1]
  const keywords = JSON.parse((pkg.match(/^keywords = (\[.*\])$/m) || die('Cargo.toml: no keywords'))[1])
  return { version: str('version'), description: str('description'), license: str('license'), keywords }
}

// The archive's .sha256 file is "<hex>  <name>" (or "<hex> *<name>").
function checkSha256(file) {
  const want = fs.readFileSync(`${file}.sha256`, 'utf8').trim().split(/\s+/)[0].toLowerCase()
  const got = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
  if (want !== got) die(`${path.basename(file)}: sha256 ${got}, its .sha256 file says ${want}`)
}

function writeJson(file, value) {
  fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n')
}

function main() {
  const a = args()
  const root = path.resolve(__dirname, '..')
  const meta = cargoMeta(root)
  if (meta.version !== a.version) die(`--version ${a.version}, but Cargo.toml says ${meta.version}`)
  const out = path.resolve(a.out)
  if (fs.existsSync(out) && fs.readdirSync(out).length) die(`${out} is not empty`)
  fs.mkdirSync(out, { recursive: true })
  const common = {
    version: a.version,
    license: meta.license,
    homepage: REPO,
    // npm's provenance check compares this with the repository the
    // workflow ran in.
    repository: { type: 'git', url: `git+${REPO}.git` },
    bugs: `${REPO}/issues`,
  }
  const licenses = ['LICENSE-MIT', 'LICENSE-APACHE']
  const order = []
  const optional = {}

  for (const [target, t] of Object.entries(TARGETS)) {
    const archive = path.join(a.archives, `matchapack-${target}.tar.xz`)
    if (!fs.existsSync(archive)) die(`missing ${archive}`)
    checkSha256(archive)
    const name = `${SCOPE}/${t.plat}`
    const dir = path.join(out, `matchapack-${t.plat}`)
    const tmp = path.join(out, `.x-${t.plat}`)
    fs.mkdirSync(path.join(dir, 'bin'), { recursive: true })
    fs.mkdirSync(tmp)
    // The archive holds matchapack-<target>/{matchapack,README.md,LICENSE-*}.
    execFileSync('tar', ['-xf', archive, '-C', tmp, '--strip-components', '1'], { stdio: 'inherit' })
    fs.copyFileSync(path.join(tmp, 'matchapack'), path.join(dir, 'bin', 'matchapack'))
    fs.chmodSync(path.join(dir, 'bin', 'matchapack'), 0o755)
    for (const l of licenses) fs.copyFileSync(path.join(root, l), path.join(dir, l))
    fs.rmSync(tmp, { recursive: true })
    fs.writeFileSync(
      path.join(dir, 'README.md'),
      `# ${name}\n\nThe ${t.plat} binary of [matchapack](${REPO}). Install \`matchapack\` instead: npm picks this package for you.\n`,
    )
    writeJson(path.join(dir, 'package.json'), {
      name,
      ...common,
      description: `The ${t.plat} binary of matchapack`,
      os: [t.os],
      cpu: [t.cpu],
      ...(t.libc ? { libc: [t.libc] } : {}),
      // Yarn 2+ (Plug'n'Play): keep the binary a real file on disk.
      preferUnplugged: true,
      files: ['bin/matchapack', ...licenses],
    })
    optional[name] = a.version
    order.push(dir)
  }

  const main = path.join(out, 'matchapack')
  fs.mkdirSync(path.join(main, 'bin'), { recursive: true })
  fs.copyFileSync(path.join(root, 'npm', 'launcher.sh'), path.join(main, 'bin', 'matchapack'))
  fs.chmodSync(path.join(main, 'bin', 'matchapack'), 0o755)
  fs.copyFileSync(path.join(root, 'npm', 'README.md'), path.join(main, 'README.md'))
  for (const l of licenses) fs.copyFileSync(path.join(root, l), path.join(main, l))
  writeJson(path.join(main, 'package.json'), {
    name: 'matchapack',
    ...common,
    description: meta.description,
    keywords: meta.keywords,
    bin: { matchapack: 'bin/matchapack' },
    os: ['darwin', 'linux'],
    cpu: ['arm64', 'x64'],
    optionalDependencies: optional,
    files: ['bin/matchapack', 'README.md', ...licenses],
  })
  order.push(main)
  for (const d of order) console.log(d)
}

main()
