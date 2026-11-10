// Use NODE_OPTIONS=--require=<this file> with the installed scip-typescript CLI.
// The hooks record results of its existing checker queries, without adding queries.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const oracle = process.env.SCIP_ORACLE_DIR ?? path.join(os.homedir(), '.local/lib/node_modules/@sourcegraph/scip-typescript');
const ts = require(require.resolve('typescript', { paths: [oracle] }));
const { FileIndexer } = require(path.join(oracle, 'dist/src/FileIndexer.js'));
const { Range } = require(path.join(oracle, 'dist/src/Range.js'));
const out = fs.openSync(process.env.SCIP_ORACLE_TRACE, 'w');
const state = Symbol('spike trace');
const methods = FileIndexer.prototype;
const original = Object.fromEntries([
  'getTSSymbolAtLocation', 'visitSymbolOccurrence', 'getDeclarationsForPropertyAssignment', 'pushOccurrence',
].map((name) => [name, methods[name]]));

function parents(node) {
  const chain = [];
  for (let parent = node.parent; parent; parent = parent.parent) chain.push(ts.SyntaxKind[parent.kind]);
  return chain;
}

function describe(declarations = []) {
  return declarations.map((decl) => ({
    file: decl.getSourceFile()?.fileName, kind: ts.SyntaxKind[decl.kind],
    range: decl.pos < 0 ? null : Range.fromNode(decl).toLsif(), parents: parents(decl),
  }));
}

function row(indexer, node, symbol) {
  const token = ts.isConstructorDeclaration(node) ? node.getFirstToken() ?? node : node;
  return {
    file: indexer.document.relative_path, range: Range.fromNode(token).toLsif(),
    token: token.getText(), kind: ts.SyntaxKind[node.kind], parents: parents(node),
    checkerSymbol: symbol?.name ?? null, checkerDeclarations: describe(symbol?.declarations),
    chosenDeclarations: [], route: symbol ? 'checker declarations' : 'no checker symbol', emitted: [],
  };
}

function write(row) {
  fs.writeSync(out, `${JSON.stringify(row)}\n`);
}

methods.getTSSymbolAtLocation = function(node) {
  const symbol = original.getTSSymbolAtLocation.call(this, node);
  if (!symbol) write(row(this, node, symbol));
  return symbol;
};

methods.visitSymbolOccurrence = function(node, symbol) {
  const trace = this[state] = row(this, node, symbol);
  try {
    return original.visitSymbolOccurrence.call(this, node, symbol);
  } finally {
    if (trace.emitted.some(([, role]) => role & 1)) {
      trace.route = 'definition';
      trace.chosenDeclarations = describe([ts.isConstructorDeclaration(node) ? node : node.parent]);
    } else if (!trace.chosenDeclarations.length) {
      trace.chosenDeclarations = trace.checkerDeclarations;
    }
    write(trace);
    delete this[state];
  }
};

methods.getDeclarationsForPropertyAssignment = function(node) {
  const decls = original.getDeclarationsForPropertyAssignment.call(this, node);
  if (decls?.length) {
    this[state].route = 'contextual property';
    this[state].chosenDeclarations = describe(decls);
  }
  return decls;
};

methods.pushOccurrence = function(occurrence) {
  const count = this.document.occurrences.length;
  original.pushOccurrence.call(this, occurrence);
  if (this[state] && this.document.occurrences.length !== count) {
    this[state].emitted.push([occurrence.symbol, occurrence.symbol_roles ?? 0]);
  }
};
