#!/usr/bin/env node
// Builds the npm package of one release from dist's archives:
//
//   node scripts/npm-packages.js --version 0.1.0 --archives <dir> --out <dir>
//
// <dir> holds dist's matchapack-<target>.tar.xz archives and their .sha256
// files (the release workflow's artifacts). <out> gets the package directory,
// <out>/matchapack, ready for `npm pack` / `npm publish`; the script prints
// its path.
//
// One package, `@grimmerk/matchapack` (NAME), with all four binaries in it:
// native/<os>-<cpu>/matchapack, and bin/matchapack, a POSIX sh launcher
// (npm/launcher.sh) that execs the one for this machine; its command is
// `matchapack`. Nothing runs at install time and nothing is downloaded after
// it; every install carries all four binaries, so the package is about four
// times the size of one. Global install (`npm i -g @grimmerk/matchapack`) is
// the supported way to use it.
'use strict'
const fs = require('fs')
const path = require('path')
const crypto = require('crypto')
const { execFileSync } = require('child_process')

const REPO = 'https://github.com/grimmerk/matchapack'
// The package's name. npm refused the unscoped `matchapack` as too similar to
// another package (issue #18), so it is published in its owner's scope. The
// launcher (npm/launcher.sh) and src/channel.rs recognise both names.
const NAME = '@grimmerk/matchapack'
// Rust target → directory under native/ (Node's process.platform-process.arch,
// as the launcher names them), and the binary format it must be.
const TARGETS = {
  'aarch64-apple-darwin': { plat: 'darwin-arm64', format: 'Mach-O arm64' },
  'x86_64-apple-darwin': { plat: 'darwin-x64', format: 'Mach-O x86_64' },
  'aarch64-unknown-linux-musl': { plat: 'linux-arm64', format: 'ELF aarch64' },
  'x86_64-unknown-linux-musl': { plat: 'linux-x64', format: 'ELF x86_64' },
}

function die(msg) {
  console.error(`npm-packages: ${msg}`)
  process.exit(1)
}

function args() {
  const usage = 'usage: npm-packages.js --version <x.y.z> --archives <dir> --out <dir>'
  const a = {}
  const argv = process.argv.slice(2)
  for (let i = 0; i < argv.length; i += 2) {
    const k = argv[i]
    if (!['--version', '--archives', '--out'].includes(k) || argv[i + 1] === undefined) die(usage)
    a[k.slice(2)] = argv[i + 1]
  }
  if (!a.version || !a.archives || !a.out) die(usage)
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

// What the file's header says it is: a 64-bit Mach-O or ELF executable for
// arm64 or x86_64, so a binary in the wrong directory is caught here, not by
// a user's "exec format error".
function binaryFormat(file) {
  const h = Buffer.alloc(20)
  const fd = fs.openSync(file, 'r')
  fs.readSync(fd, h, 0, 20, 0)
  fs.closeSync(fd)
  if (h.readUInt32LE(0) === 0xfeedfacf) {
    const cpu = { 0x01000007: 'x86_64', 0x0100000c: 'arm64' }[h.readUInt32LE(4)]
    return `Mach-O ${cpu || 'cpu ' + h.readUInt32LE(4).toString(16)}`
  }
  if (h.readUInt32BE(0) === 0x7f454c46 && h[4] === 2 && h[5] === 1) {
    const m = h.readUInt16LE(18)
    return `ELF ${{ 0x3e: 'x86_64', 0xb7: 'aarch64' }[m] || 'machine ' + m}`
  }
  return 'unknown'
}

function main() {
  const a = args()
  const root = path.resolve(__dirname, '..')
  const meta = cargoMeta(root)
  if (meta.version !== a.version) die(`--version ${a.version}, but Cargo.toml says ${meta.version}`)
  const out = path.resolve(a.out)
  if (fs.existsSync(out) && fs.readdirSync(out).length) die(`${out} is not empty`)
  const pkg = path.join(out, 'matchapack')
  const tmp = path.join(out, '.extract')
  fs.mkdirSync(path.join(pkg, 'bin'), { recursive: true })
  const licenses = ['LICENSE-MIT', 'LICENSE-APACHE']

  for (const [target, t] of Object.entries(TARGETS)) {
    const archive = path.join(a.archives, `matchapack-${target}.tar.xz`)
    if (!fs.existsSync(archive)) die(`missing ${archive}`)
    checkSha256(archive)
    fs.mkdirSync(tmp)
    // The archive holds matchapack-<target>/{matchapack,README.md,LICENSE-*}.
    execFileSync('tar', ['-xf', archive, '-C', tmp, '--strip-components', '1'], { stdio: 'inherit' })
    const format = binaryFormat(path.join(tmp, 'matchapack'))
    if (format !== t.format) die(`${path.basename(archive)}: the binary is ${format}, not ${t.format}`)
    const dir = path.join(pkg, 'native', t.plat)
    fs.mkdirSync(dir, { recursive: true })
    fs.copyFileSync(path.join(tmp, 'matchapack'), path.join(dir, 'matchapack'))
    fs.chmodSync(path.join(dir, 'matchapack'), 0o755)
    fs.rmSync(tmp, { recursive: true })
  }

  fs.copyFileSync(path.join(root, 'npm', 'launcher.sh'), path.join(pkg, 'bin', 'matchapack'))
  fs.chmodSync(path.join(pkg, 'bin', 'matchapack'), 0o755)
  fs.copyFileSync(path.join(root, 'npm', 'README.md'), path.join(pkg, 'README.md'))
  for (const l of licenses) fs.copyFileSync(path.join(root, l), path.join(pkg, l))
  fs.writeFileSync(
    path.join(pkg, 'package.json'),
    JSON.stringify(
      {
        name: NAME,
        version: a.version,
        description: meta.description,
        keywords: meta.keywords,
        license: meta.license,
        homepage: REPO,
        // npm's provenance check compares this with the repository the
        // workflow ran in.
        repository: { type: 'git', url: `git+${REPO}.git` },
        bugs: `${REPO}/issues`,
        bin: { matchapack: 'bin/matchapack' },
        // npm refuses the install elsewhere (EBADPLATFORM) instead of
        // leaving a command that cannot run.
        os: ['darwin', 'linux'],
        cpu: ['arm64', 'x64'],
        files: ['bin/matchapack', 'native/', 'README.md', ...licenses],
        // A scoped package is private unless published as public.
        publishConfig: { access: 'public' },
      },
      null,
      2,
    ) + '\n',
  )
  console.log(pkg)
}

main()
