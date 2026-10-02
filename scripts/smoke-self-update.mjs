import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { knutBinary } from './binary.mjs';
import { readRequest, sendCompletion, startJsonlSession } from './smoke-fixtures.mjs';

function createFixture(binary) {
  const root = fs.mkdtempSync('/var/tmp/knut-self-update-');
  const workspace = path.join(root, 'source');
  const target = path.join(root, 'knut');
  const config = path.join(root, 'config');
  fs.mkdirSync(path.join(workspace, 'src'), { recursive: true });
  fs.mkdirSync(config, { mode: 0o700 });
  fs.copyFileSync(binary, target);
  fs.chmodSync(target, 0o755);
  fs.writeFileSync(
    path.join(workspace, 'Cargo.toml'),
    '[package]\nname="knut"\nversion="0.1.0"\nedition="2024"\n',
  );
  fs.writeFileSync(path.join(workspace, '.gitignore'), '.knut-update/\n');
  fs.writeFileSync(
    path.join(workspace, 'src/main.rs'),
    'fn main() {\n    println!("updated fixture");\n}\n',
  );
  const lock = spawnSync('cargo', ['generate-lockfile', '--offline'], {
    cwd: workspace,
    encoding: 'utf8',
  });
  assert.equal(lock.status, 0, lock.stderr);
  return { root, workspace, target, config };
}

const binary = knutBinary();
const { root, workspace, target, config } = createFixture(binary);
let calls = 0;

function updateResponse(request) {
  calls++;
  const results = request.messages
    .filter(item => item.role === 'tool')
    .map(item => JSON.parse(item.content));
  const last = results.at(-1);
  let tool;
  let args;
  if (!last) {
    tool = request.tools.find(item =>
      item.function.description.includes('Inspect the installed Knut hash'));
    args = {};
  } else if (last.source_revision) {
    tool = request.tools.find(item =>
      item.function.parameters.properties?.expect_source_revision);
    args = {
      expect_installed_hash: last.installed_hash,
      expect_source_revision: last.source_revision,
    };
  } else {
    assert(last.installed_hash && !last.error, JSON.stringify(last));
  }
  return {
    content: 'Update installed. The original session is still running.',
    toolCalls: tool ? [{
      id: `call-${calls}`,
      type: 'function',
      function: { name: tool.function.name, arguments: JSON.stringify(args) },
    }] : [],
  };
}

const model = http.createServer(async (request, response) => {
  try {
    sendCompletion(response, updateResponse(await readRequest(request)));
  } catch (error) {
    response.writeHead(500);
    response.end(String(error));
  }
});
await new Promise(resolve => model.listen(0, '127.0.0.1', resolve));

const session = startJsonlSession(target, workspace, {
  ...process.env,
  KNUT_CONFIG_DIR: config,
  KNUT_UPDATE_TARGET: target,
  KNUT_PROVIDER: 'chat-completions',
  KNUT_PROVIDER_API_KEY: 'fixture-key',
  KNUT_PROVIDER_MODEL: 'fixture',
  KNUT_PROVIDER_BASE_URL: `http://127.0.0.1:${model.address().port}/v1`,
  KNUT_PROFILE: 'general',
  KNUT_LOAD_ENV: '0',
});
let approvals = 0;
let completed = false;

try {
  for await (const message of session.messages()) {
    if (message.type === 'ready') {
      session.send({ type: 'submit', prompt: 'Inspect and install the current source as an update.' });
    }
    const event = message.event;
    if (!event) continue;
    assert(!['runtime_error', 'task_failed'].includes(event.kind), JSON.stringify(event));
    if (event.kind === 'waiting_for_user') {
      assert(event.wait.approval);
      approvals++;
      assert.equal(
        spawnSync(target, ['--help'], { encoding: 'utf8' }).status,
        0,
        'Original executable is active before approval',
      );
      session.send({ type: 'approve', approval_key: event.wait.approval.approval_key });
    }
    if (event.kind === 'tool_call_failed') throw new Error(JSON.stringify(event));
    if (event.kind === 'task_completed') {
      completed = true;
      session.send({ type: 'close' });
      session.child.stdin.end();
    }
  }
  const result = await session.exited;
  assert.equal(result.code, 0, JSON.stringify(result) + session.stderr);
  assert(completed);
  assert.equal(approvals, 1);
  assert.equal(calls, 3);
  const updated = spawnSync(target, [], { encoding: 'utf8' });
  assert.equal(updated.status, 0);
  assert.equal(updated.stdout.trim(), 'updated fixture');
  assert.equal(fs.readFileSync(`${target}.previous`).compare(fs.readFileSync(binary)), 0);
  console.log(JSON.stringify({
    root,
    approvals,
    checks: 'sandboxed offline format/test/clippy/release build',
    atomicUpdate: 'passed',
    residentSession: 'passed',
    rollbackBinary: 'passed',
  }));
} finally {
  session.close();
  model.close();
  model.closeAllConnections();
}
