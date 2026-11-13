const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const binary = process.env.SCIP_SPIKE_BIN ?? path.join(__dirname, 'target/release/scip-tsrs-spike');

function readIndex(file) {
  const lines = execFileSync(binary, ['dump', file], { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });
  const files = new Set();
  const tuples = new Map();
  for (const line of lines.trimEnd().split('\n')) {
    const tuple = JSON.parse(line);
    if (tuple.length === 1) files.add(tuple[0]);
    else tuples.set(line, tuple);
  }
  return { files, tuples };
}

function compare(oracle, native, predicate = () => true) {
  const oracleOnly = [];
  const nativeOnly = [];
  let exact = 0;
  for (const [key, tuple] of oracle) {
    if (!predicate(tuple)) continue;
    if (native.has(key)) exact++;
    else oracleOnly.push(tuple);
  }
  for (const [key, tuple] of native) {
    if (predicate(tuple) && !oracle.has(key)) nativeOnly.push(tuple);
  }
  return { exact, oracleOnly, nativeOnly };
}

function stats({ exact, oracleOnly, nativeOnly }) {
  const oracle = exact + oracleOnly.length;
  const native = exact + nativeOnly.length;
  const percent = (denom) => denom ? Number((100 * exact / denom).toFixed(3)) : null;
  return {
    oracle, native, exact, oracleOnly: oracleOnly.length, nativeOnly: nativeOnly.length,
    recall: percent(oracle), precision: percent(native), jaccard: percent(oracle + native - exact),
  };
}

function main() {
  const [oraclePath, nativePath, ledger] = process.argv.slice(2);
  if (!nativePath) throw new Error('usage: node compare.cjs ORACLE.scip NATIVE.scip [DIFF.jsonl]');
  const oracle = readIndex(oraclePath);
  const native = readIndex(nativePath);
  const result = compare(oracle.tuples, native.tuples);
  const only = (files, other) => [...files].filter((file) => !other.has(file)).sort();
  console.log(JSON.stringify({
    documents: {
      oracle: oracle.files.size, native: native.files.size,
      oracleOnly: only(oracle.files, native.files), nativeOnly: only(native.files, oracle.files),
    },
    all: stats(result),
    global: stats(compare(oracle.tuples, native.tuples, ([symbol]) => !symbol.startsWith('local '))),
    local: stats(compare(oracle.tuples, native.tuples, ([symbol]) => symbol.startsWith('local '))),
    definitions: stats(compare(oracle.tuples, native.tuples, ([,,, role]) => (role & 1) === 1)),
  }, null, 2));
  if (ledger) {
    const fd = fs.openSync(ledger, 'w');
    try {
      for (const [side, rows] of Object.entries({ oracle: result.oracleOnly, native: result.nativeOnly })) {
        for (const tuple of rows) fs.writeSync(fd, `${JSON.stringify({ side, tuple })}\n`);
      }
    } finally {
      fs.closeSync(fd);
    }
  }
}

if (require.main === module) main();
module.exports = { readIndex, compare };
