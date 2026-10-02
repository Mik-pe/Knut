import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { knutBinary } from './binary.mjs';
import { readRequest, sendCompletion, startJsonlSession } from './smoke-fixtures.mjs';

const binary = knutBinary();
const root = fs.mkdtempSync('/var/tmp/knut-harness-smoke-');
const source = 'pub fn value() -> u8 { 0 }\n#[cfg(test)] mod tests { #[test] fn value_is_seven() { assert_eq!(super::value(), 7); } }\n';
fs.mkdirSync(path.join(root, 'src'));
fs.writeFileSync(path.join(root, 'Cargo.toml'), '[package]\nname="harness_smoke"\nversion="0.1.0"\nedition="2024"\n');
fs.writeFileSync(path.join(root, '.gitignore'), '/target\n');
fs.writeFileSync(path.join(root, 'src/lib.rs'), source);
fs.writeFileSync(path.join(root, 'AGENTS.md'), 'Preserve the public API. Keep the existing test unchanged.\n');

let modelCalls = 0;
let patchCalls = 0;
let repairObserved = false;
let diagnosticObserved = false;
const server = http.createServer(async (req, res) => {
  try {
    const request = await readRequest(req);
    modelCalls++;
    const prompt = request.messages
      .filter(message => message.role === 'user')
      .map(message => message.content)
      .join('\n');
    assert(prompt.includes('Preserve the public API'));
    const tools = request.tools.map(tool => tool.function);
    const readTool = tools.find(tool => tool.parameters.properties?.start_line);
    const editTool = tools.find(tool => tool.parameters.properties?.changes);
    assert(readTool && editTool);
    const results = request.messages
      .filter(message => message.role === 'tool')
      .map(message => JSON.parse(message.content));
    const last = results.at(-1);
    let content = '';
    let toolCalls = [];
    const invoke = (tool, args) => {
      toolCalls = [{
        id: `call_${modelCalls}`,
        type: 'function',
        function: { name: tool.name, arguments: JSON.stringify(args) },
      }];
    };
    if (prompt.includes('Summarize notes.txt')) {
      assert(prompt.includes('general profile evidence'));
      content = 'The notes contain general profile evidence.';
    } else if (!last) {
      invoke(readTool, modelCalls === 1 ? { file: 'src/lib.rs' } : { path: 'src/lib.rs' });
    } else if (last.error) {
      assert(last.error.includes('path'));
      repairObserved = true;
      invoke(readTool, { path: 'src/lib.rs' });
    } else if (last.lines && last.content_hash && !prompt.includes('Inspect the result')) {
      const current = last.lines.map(line => line.text).join('\n');
      const old = current.includes('{ 0 }') ? '{ 0 }' : '{ 6 }';
      const value = old === '{ 0 }' ? 6 : 7;
      patchCalls++;
      invoke(editTool, {
        path: 'src/lib.rs',
        expect_hash: last.content_hash,
        changes: JSON.stringify([{ old, new: `{ ${value} }` }]),
      });
    } else if (prompt.includes('failed_checks') && patchCalls === 1) {
      assert(prompt.includes('value_is_seven'));
      diagnosticObserved = true;
      invoke(readTool, { path: 'src/lib.rs' });
    } else {
      content = 'The task is complete.';
    }
    sendCompletion(res, {
      content, toolCalls, usage: { prompt_tokens: 100, completion_tokens: 50 },
    }, request.stream);
  } catch (error) {
    console.error(error);
    res.writeHead(500);
    res.end(JSON.stringify({ error: String(error) }));
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const baseUrl = `http://127.0.0.1:${server.address().port}/v1`;
const env = {
  ...process.env,
  KNUT_CONFIG_DIR: path.join(root, 'config'),
  KNUT_PROFILE: 'coding',
  KNUT_PROVIDER: 'chat-completions',
  KNUT_PROVIDER_API_KEY: 'local-fixture',
  KNUT_PROVIDER_MODEL: 'fixture',
  KNUT_PROVIDER_BASE_URL: baseUrl,
  CARGO_NET_OFFLINE: 'true',
};
delete env.TYPESAFE_API_KEY;
delete env.KNUT_PROVIDER_REASONING_EFFORT;
const lock = spawnSync('cargo', ['generate-lockfile', '--offline'], { cwd: root, env, encoding: 'utf8' });
assert.equal(lock.status, 0, lock.stderr);

if (process.argv.includes('--serve')) {
  fs.writeFileSync(path.join(root, 'connection.json'), JSON.stringify({ root, baseUrl }));
  console.log(JSON.stringify({ root, baseUrl, ready: true }));
} else {
  const session = startJsonlSession(binary, root, env);
  const { child, send, exited } = session;
  const events = [];
  let approvals = 0;
  let completed = 0;
  try {
    for await (const message of session.messages()) {
      if (message.type === 'ready') {
        send({ type: 'submit', prompt: 'Fix value() to return seven; preserve the existing test.' });
      }
      const event = message.event;
      if (!event) continue;
      events.push(event);
      assert.notEqual(event.kind, 'task_failed', JSON.stringify(event));
      assert.notEqual(event.kind, 'runtime_error', JSON.stringify(event));
      if (event.kind === 'waiting_for_user') {
        assert(event.wait.approval);
        if (++approvals === 1) {
          send({ type: 'queue', prompt: 'short' });
          send({ type: 'update_queued', id: 1, prompt: 'Inspect the result and run the checks.' });
          send({ type: 'queue', prompt: 'remove me' });
          send({ type: 'remove_queued', id: 2 });
        }
        const proposal = events.findLast(item => item.kind === 'tool_call_proposed');
        assert.equal(proposal.arguments.path, 'src/lib.rs');
        assert(proposal.arguments.expect_hash);
        send({ type: 'approve', approval_key: event.wait.approval.approval_key });
      }
      if (event.kind === 'task_completed' && ++completed === 2) {
        send({ type: 'close' });
        child.stdin.end();
      }
    }
    const exit = await exited;
    assert.equal(exit.code, 0, `${JSON.stringify(exit)}\n${session.stderr}`);
    assert.equal(approvals, 2);
    assert.equal(completed, 2);
    assert(repairObserved && diagnosticObserved);
    assert.equal(events.filter(event => event.kind === 'task_started').length, 2);
    assert.equal(events.filter(event => event.kind === 'task_steered').length, 0);
    assert.equal(fs.readFileSync(path.join(root, 'src/lib.rs'), 'utf8'), source.replace('{ 0 }', '{ 7 }'));
    const verified = spawnSync('cargo', ['test', '--quiet', '--offline'], { cwd: root, env, encoding: 'utf8' });
    assert.equal(verified.status, 0, verified.stderr);
    const generalRoot = fs.mkdtempSync('/var/tmp/knut-general-smoke-');
    fs.writeFileSync(path.join(generalRoot, 'AGENTS.md'), 'Preserve the public API.\n');
    fs.writeFileSync(path.join(generalRoot, 'notes.txt'), 'general profile evidence\n');
    const general = await new Promise(resolve => {
      const task = spawn(binary, ['run', 'Summarize notes.txt'], {
        cwd: generalRoot,
        env: { ...env, KNUT_PROFILE: 'auto' },
      });
      let stdout = '';
      let stderr = '';
      task.stdout.on('data', chunk => { stdout += chunk; });
      task.stderr.on('data', chunk => { stderr += chunk; });
      task.on('exit', code => resolve({ code, stdout, stderr }));
    });
    assert.equal(general.code, 0, general.stderr);
    assert(general.stdout.includes('The notes contain general profile evidence.'));
    const coding = spawnSync(binary, ['run', 'Summarize notes.txt'], {
      cwd: generalRoot,
      env: { ...env, KNUT_PROFILE: 'coding' },
      encoding: 'utf8',
    });
    assert.notEqual(coding.status, 0);
    assert(coding.stderr.includes('No repository checks configured'));
    console.log(JSON.stringify({
      root, generalRoot, approvals, completed, modelCalls, patchCalls,
      independentChecks: 'passed',
      generalProfile: 'passed',
      requiredCodingChecks: 'passed',
    }));
  } finally {
    session.close();
    fs.writeFileSync(path.join(root, 'events.json'), JSON.stringify(events, null, 2));
    server.close();
    server.closeAllConnections();
  }
}
