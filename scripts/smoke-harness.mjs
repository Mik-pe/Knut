import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import {spawn, spawnSync} from 'node:child_process';
import {createInterface} from 'node:readline';
import {fileURLToPath} from 'node:url';

const binary = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../target/debug/knut');
const root = fs.mkdtempSync('/var/tmp/knut-harness-smoke-');
const source = 'pub fn value() -> u8 { 0 }\n#[cfg(test)] mod tests { #[test] fn value_is_seven() { assert_eq!(super::value(), 7); } }\n';
fs.mkdirSync(path.join(root, 'src'));
fs.writeFileSync(path.join(root, 'Cargo.toml'), '[package]\nname="harness_smoke"\nversion="0.1.0"\nedition="2024"\n');
fs.writeFileSync(path.join(root, '.gitignore'), '/target\n');
fs.writeFileSync(path.join(root, 'src/lib.rs'), source);
fs.writeFileSync(path.join(root, 'AGENTS.md'), 'Preserve the public API. Keep the existing test unchanged.\n');

const ref = (pointer) => ({$ref: 'read', kind: 'text', pointer});
const read = {type: 'tool', id: 'read', capability: 'files', tool_id: 'read', input: {path: 'src/lib.rs'}};
const plan = {type: 'sequence', id: 'root', children: [
  read,
  {type: 'generate', id: 'patch', tier: 'reasoner', instruction: 'Return only the exact edit array to fix value().', input: {source: {$ref: 'read', kind: 'json'}}},
  {type: 'tool', id: 'edit', capability: 'files', tool_id: 'edit', input: {path: ref('/path'), expect_hash: ref('/content_hash'), changes: {$ref: 'patch', kind: 'text'}}},
]};
let planningCalls = 0;
let patchCalls = 0;
let repairObserved = false;
let diagnosticObserved = false;
const server = http.createServer(async (req, res) => {
  try {
    let body = '';
    for await (const chunk of req) body += chunk;
    const request = JSON.parse(body);
    const prompt = request.messages.map(message => message.content).join('\n');
    let content;
    if (prompt.includes('node_forms')) {
      planningCalls++;
      if (prompt.includes('Inspect the result')) {
        content = JSON.stringify(read);
      } else if (planningCalls === 1) {
        content = JSON.stringify({...read, input: {file: 'src/lib.rs'}});
      } else {
        if (planningCalls === 2) {
          assert(prompt.includes('rejected_plan'));
          assert(prompt.includes('missing required field'));
          repairObserved = true;
        } else {
          assert(prompt.includes('failed_checks'));
          assert(prompt.includes('value_is_seven'));
          diagnosticObserved = true;
        }
        content = JSON.stringify(plan);
      }
    } else {
      patchCalls++;
      assert(prompt.includes('Preserve the public API'));
      const old = patchCalls === 1 ? '{ 0 }' : '{ 6 }';
      const value = patchCalls === 1 ? 6 : 7;
      content = JSON.stringify([{old, new: `{ ${value} }`}]);
    }
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({choices: [{index: 0, message: {role: 'assistant', content}, finish_reason: 'stop'}], usage: {prompt_tokens: 100, completion_tokens: 50}}));
  } catch (error) {
    res.writeHead(500);
    res.end(JSON.stringify({error: String(error)}));
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const baseUrl = `http://127.0.0.1:${server.address().port}/v1`;
const env = {...process.env, KNUT_CONFIG_DIR: path.join(root, 'config'), KNUT_PROVIDER_API_KEY: 'local-fixture', KNUT_PROVIDER_MODEL: 'fixture', KNUT_PROVIDER_BASE_URL: baseUrl, CARGO_NET_OFFLINE: 'true'};
delete env.TYPESAFE_API_KEY;
delete env.KNUT_PROVIDER_REASONING_EFFORT;
const lock = spawnSync('cargo', ['generate-lockfile', '--offline'], {cwd: root, env, encoding: 'utf8'});
assert.equal(lock.status, 0, lock.stderr);

if (process.argv.includes('--serve')) {
  fs.writeFileSync(path.join(root, 'connection.json'), JSON.stringify({root, baseUrl}));
  console.log(JSON.stringify({root, baseUrl, ready: true}));
} else {
  const child = spawn(binary, ['jsonl'], {cwd: root, env, stdio: ['pipe', 'pipe', 'pipe']});
  const events = [];
  let stderr = '';
  let approvals = 0;
  let completed = 0;
  const send = command => child.stdin.write(`${JSON.stringify(command)}\n`);
  child.stderr.on('data', chunk => { stderr += chunk; });
  const deadline = setTimeout(() => child.kill('SIGKILL'), 120_000);
  const exited = new Promise(resolve => child.on('exit', (code, signal) => resolve({code, signal})));
  try {
    for await (const line of createInterface({input: child.stdout})) {
      const message = JSON.parse(line);
      if (message.type === 'ready') send({type: 'submit', prompt: 'Fix value() to return seven; preserve the existing test.'});
      const event = message.event;
      if (!event) continue;
      events.push(event);
      assert.notEqual(event.kind, 'task_failed', JSON.stringify(event));
      assert.notEqual(event.kind, 'runtime_error', JSON.stringify(event));
      if (event.kind === 'waiting_for_user') {
        assert(event.wait.approval);
        if (++approvals === 1) {
          send({type: 'queue', prompt: 'short'});
          send({type: 'update_queued', id: 1, prompt: 'Inspect the result and run the checks.'});
          send({type: 'queue', prompt: 'remove me'});
          send({type: 'remove_queued', id: 2});
        }
        const proposal = events.findLast(item => item.kind === 'tool_call_proposed');
        assert.equal(proposal.arguments.path, 'src/lib.rs');
        assert(proposal.arguments.expect_hash);
        send({type: 'approve', approval_key: event.wait.approval.approval_key});
      }
      if (event.kind === 'task_completed' && ++completed === 2) {
        send({type: 'close'});
        child.stdin.end();
      }
    }
    const exit = await exited;
    assert.equal(exit.code, 0, `${JSON.stringify(exit)}\n${stderr}`);
    assert.equal(approvals, 2);
    assert.equal(completed, 2);
    assert(repairObserved && diagnosticObserved);
    assert.equal(events.filter(event => event.kind === 'task_started').length, 2);
    assert.equal(events.filter(event => event.kind === 'task_steered').length, 0);
    assert.equal(fs.readFileSync(path.join(root, 'src/lib.rs'), 'utf8'), source.replace('{ 0 }', '{ 7 }'));
    const verified = spawnSync('cargo', ['test', '--quiet', '--offline'], {cwd: root, env, encoding: 'utf8'});
    assert.equal(verified.status, 0, verified.stderr);
    console.log(JSON.stringify({root, approvals, completed, planningCalls, patchCalls, independentChecks: 'passed'}));
  } finally {
    clearTimeout(deadline);
    if (child.exitCode === null) child.kill('SIGTERM');
    fs.writeFileSync(path.join(root, 'events.json'), JSON.stringify(events, null, 2));
    server.close();
    server.closeAllConnections();
  }
}
