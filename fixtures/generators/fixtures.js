// Shared scaffolding for the golden-fixture generators.
//
// Every fixture under `fixtures/` is produced by evaluating the original
// JavaScript from `commandcode-proxy/proxy.mjs` verbatim, not by re-deriving its
// behavior from a description: a hand-written expectation would encode the same
// misunderstanding as a hand-written port.
//
// The generators are not part of the build. Run one by hand (see
// `fixtures/README.md`) whenever a fixture needs to be refreshed; cargo ignores
// this directory because it holds no Rust targets.
const fs = require('fs');
const path = require('path');

const PROXY =
  process.env.CC_PROXY_SOURCE || path.resolve(__dirname, '../../../commandcode-proxy/proxy.mjs');

const source = fs.readFileSync(PROXY, 'utf8');

/** The region between two markers, verbatim. */
function slice(startMarker, endMarker) {
  const start = source.indexOf(startMarker);
  const end = source.indexOf(endMarker, start);
  if (start < 0 || end < 0 || end <= start) throw new Error(`could not locate ${startMarker}`);
  return source.slice(start, end);
}

/**
 * Evaluate a region of the original and re-export the named symbols.
 *
 * The region is a slice of a module, so its helpers arrive as bare function
 * declarations that no caller can see from the outside; `exports` names them so
 * the generated function can hand them back. `globals` are the free variables
 * the region closes over (`CFG`, `log`, `crypto`, ...) and are injected as
 * positional parameters named by `names`, in the same order as their values.
 */
function evaluate(region, names, globals, exports) {
  const returned = (exports || Object.keys(globals)).join(', ');
  const build = new Function(...names, `${region}\nreturn { ${returned} };`);
  return build(...Object.values(globals));
}

function write(name, value) {
  fs.writeFileSync(path.resolve(__dirname, '..', name), `${JSON.stringify(value, null, 2)}\n`);
  console.log(`wrote ${name}`);
}

module.exports = { PROXY, slice, evaluate, write };
