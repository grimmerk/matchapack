// npm ci's ideal tree, from npm itself. Runs npm's own CLI in this process as
// `npm ci --dry-run --ignore-scripts` (same config loading, same lockfile
// validation, same errors as a real `npm ci`), keeps the tree arborist's
// buildIdealTree returns, and at exit writes, for every node in it, the dep
// flags npm would write to the hidden lockfile. npm recomputes those flags
// (calcDepFlags) instead of copying the lockfile's, so a lockfile with stale
// flags gives a different hidden lockfile than its text suggests.
// The flags are taken through npm's own serializer (Shrinkwrap.metaFromNode):
// a node's in-memory flags are not what is written (npm 10 keeps devOptional
// set on dev nodes and drops it on write).
// argv: <npm install dir> <output file> [extra npm args...]
// Exit status is npm's; the output file is written only on success.
const path = require('path')
const fs = require('fs')
const [npmDir, outFile, ...extra] = process.argv.slice(2)
const arborist = path.join(npmDir, 'node_modules', '@npmcli', 'arborist')
const Arborist = require(arborist)
const Shrinkwrap = require(path.join(arborist, 'lib', 'shrinkwrap.js'))
const FLAGS = ['dev', 'optional', 'devOptional', 'peer']

// Keep the tree of the Arborist that reifies: the one whose tree npm would
// write to the hidden lockfile. ci.js builds it first, then reify() asks for
// it again and gets the same object back (buildIdealTree returns
// this.idealTree when set; npm 10.9 and 11.19). A tree another Arborist builds
// (ci.js loads the lockfile with a separate one) is ignored. reify() under
// --dry-run returns after diffing and clears idealTree, so it is kept here.
let reifying = null
let tree = null
const reify = Arborist.prototype.reify
Arborist.prototype.reify = function (...args) {
  reifying = this
  return reify.apply(this, args)
}
const buildIdealTree = Arborist.prototype.buildIdealTree
Arborist.prototype.buildIdealTree = async function (...args) {
  const t = await buildIdealTree.apply(this, args)
  if (this === reifying) {
    tree = t
  }
  return t
}

process.on('exit', (code) => {
  if (code !== 0) {
    return
  }
  if (!tree) {
    process.stderr.write('thinclone ideal-tree helper: npm built no ideal tree\n')
    process.exitCode = 1
    return
  }
  // Optional packages npm skips because their engines or platform do not
  // match, together with what only they need (optionalSet). npm 11 marks them
  // inert while building the tree and the dry-run reify removes them; npm 10.9
  // checks only in reify's per-node step, which a dry run never reaches, so
  // the tree here still holds them. Repeat npm's own check, with its options
  // and ignoring --force / --engine-strict as it does (reify.js _reifyNode in
  // 10.9, build-ideal-tree.js #checkEngineAndPlatform in 11.19).
  const { checkEngine, checkPlatform } = require(path.join(npmDir, 'node_modules', 'npm-install-checks'))
  const optionalSet = require(path.join(arborist, 'lib', 'optional-set.js'))
  const { npmVersion, nodeVersion, cpu, os, libc } = reifying.options
  const skip = new Set()
  for (const node of tree.inventory.values()) {
    if (node.inert) {
      skip.add(node)
    } else if (node.optional && !skip.has(node)) {
      try {
        checkEngine(node.package, npmVersion, nodeVersion, false)
        checkPlatform(node.package, false, { cpu, os, libc })
      } catch {
        for (const n of optionalSet(node)) {
          skip.add(n)
        }
      }
    }
  }
  const nodes = {}
  for (const [location, node] of tree.inventory.entries()) {
    if (!location || skip.has(node)) {
      continue
    }
    const meta = Shrinkwrap.metaFromNode(node, tree.path)
    const flags = {}
    for (const f of FLAGS) {
      if (meta[f]) {
        flags[f] = true
      }
    }
    nodes[location] = flags
  }
  fs.writeFileSync(outFile, JSON.stringify({ nodes }))
})

process.argv = [
  process.argv[0],
  path.join(npmDir, 'bin', 'npm-cli.js'),
  'ci', '--dry-run', '--ignore-scripts', '--no-audit', '--no-fund',
  ...extra,
]
require(path.join(npmDir, 'lib', 'cli.js'))(process)
