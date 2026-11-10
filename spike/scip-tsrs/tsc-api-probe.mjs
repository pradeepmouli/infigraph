import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdirSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [tsRoot, output] = process.argv.slice(2).map((value) => resolve(value));
if (!output) throw new Error('usage: node tsc-api-probe.mjs TYPESCRIPT_PACKAGE SCRATCH_OUTPUT_DIR');
const load = (name) => import(pathToFileURL(resolve(tsRoot, name)).href);
const exe = (await load('lib/getExePath.js')).default();
const { API } = await load('dist/api/sync/api.js');
const { SyncRpcChannel } = await load('dist/api/syncChannel.js');
mkdirSync(output, { recursive: true });
const apiArgs = ['--api', '--cwd', output, '--useCaseSensitiveFileNames=false'];
const version = spawnSync(exe, ['--version'], { encoding: 'utf8', timeout: 5000 });
assert.equal(version.status, 0, version.stderr);
assert.match(version.stdout, /7\.1\.0-beta/);

const bin8 = (value) => Buffer.concat([Buffer.from([0xc4, Buffer.byteLength(value)]), Buffer.from(value)]);
const syncRequest = Buffer.concat([Buffer.from([0x93, 1]), bin8('initialize'), bin8('null')]);
const sync = spawnSync(exe, apiArgs, { input: syncRequest, timeout: 5000 });
assert.equal(sync.status, 0, sync.error?.message ?? sync.stderr.toString());
assert.equal(sync.stdout[0], 0x93);
assert.equal(sync.stdout[1], 4);
assert.equal(sync.stdout[2], 0xc4);
const methodEnd = 4 + sync.stdout[3];
assert.equal(sync.stdout.subarray(4, methodEnd).toString(), 'initialize');
assert.equal(sync.stdout[methodEnd], 0xc4);
const syncReply = JSON.parse(sync.stdout.subarray(methodEnd + 2).toString());
assert.equal(syncReply.currentDirectory, output);
writeFileSync(resolve(output, 'sync-request.bin'), syncRequest);
writeFileSync(resolve(output, 'sync-response.bin'), sync.stdout);

const asyncRequest = Buffer.from(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: null }));
const child = spawn(exe, [...apiArgs, '--async'], { stdio: ['pipe', 'pipe', 'inherit'] });
const exited = once(child, 'exit');
const timeout = setTimeout(() => child.kill(), 5000);
let response = Buffer.alloc(0);
try {
  child.stdin.write(Buffer.concat([Buffer.from(`Content-Length: ${asyncRequest.length}\r\n\r\n`), asyncRequest]));
  for await (const chunk of child.stdout) {
    response = Buffer.concat([response, chunk]);
    const headerEnd = response.indexOf('\r\n\r\n');
    if (headerEnd < 0) continue;
    const header = response.subarray(0, headerEnd).toString();
    const length = Number(/^content-length:\s*(\d+)/im.exec(header)?.[1]);
    if (response.length < headerEnd + 4 + length) continue;
    const reply = JSON.parse(response.subarray(headerEnd + 4).toString());
    assert.equal(reply.id, 1);
    assert.equal(reply.result.currentDirectory, output);
    child.stdin.end();
    break;
  }
  assert.notEqual(response.length, 0);
  assert.deepEqual(await exited, [0, null]);
} finally {
  clearTimeout(timeout);
  child.kill();
}
writeFileSync(resolve(output, 'async-response.bin'), response);

const source = 'const prefix = "é😀"; const target = prefix;\ntarget;\n';
const file = resolve(output, 'unicode.ts');
writeFileSync(file, source);
const trace = [];
const request = SyncRpcChannel.prototype.requestBytesSync;
SyncRpcChannel.prototype.requestBytesSync = function(method, payload) {
  const reply = request.call(this, method, payload);
  trace.push({ method, requestBytes: Buffer.byteLength(payload), responseBytes: reply.length });
  if (method === 'getSourceFile') writeFileSync(resolve(output, 'source-file-payload.bin'), reply);
  return reply;
};
const api = new API({ cwd: output, tsserverPath: exe, useCaseSensitiveFileNames: false });
try {
  const program = api.createProgram([file], { noLib: true, types: [] });
  const ast = program.getSourceFile(file);
  const name = ast.statements[1].declarationList.declarations[0].name;
  const use = ast.statements[2].expression;
  const [definition, reference] = program.getProject().checker.getSymbolAtLocation([name, use]);
  const prefix = source.slice(0, source.indexOf('target'));
  assert.equal(ast.end, source.length);
  assert.equal(name.pos, prefix.length - 1);
  assert.equal(name.getStart(ast), prefix.length);
  assert.equal(name.end, prefix.length + 'target'.length);
  assert.equal(definition.name, 'target');
  assert.equal(definition.id, reference.id);
  assert.equal(definition.declarations.length, 1);
  console.log(JSON.stringify({
    version: version.stdout.trim(),
    syncRequestHex: syncRequest.toString('hex'),
    syncResponsePrefixHex: sync.stdout.subarray(0, methodEnd + 2).toString('hex'),
    asyncResponseHeader: response.subarray(0, response.indexOf('\r\n\r\n')).toString(),
    sourceFileEnd: ast.end, utf16Length: source.length, utf8Length: Buffer.byteLength(source),
    name: { pos: name.pos, start: name.getStart(ast), end: name.end, byteStart: Buffer.byteLength(prefix) },
    symbol: { name: definition.name, id: definition.id, declarationCount: definition.declarations.length },
    requests: trace,
  }, null, 2));
  program.dispose();
} finally {
  api.close();
}
