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
  // Optional packages npm leaves out because their engines or platform do not
  // match (with what only they need) are marked inert while npm 11 builds the
  // tree, and the dry-run reify removes them; skip any still marked. npm 10.9
  // decides this only while extracting, which a dry run never reaches, so its
  // tree here would still hold them: npm 10 is not in VERIFIED_NPM (see NOTES
  // "npm 10").
  const skip = new Set([...tree.inventory.values()].filter(n => n.inert))
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
