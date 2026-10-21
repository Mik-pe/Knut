import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { knutBinary } from './binary.mjs';

const require = createRequire(import.meta.url);
const modulePath = require.resolve(process.env.KNUT_TUISTORY_MODULE || 'tuistory');
const { launchTerminal } = await import(modulePath);
const artifacts = fs.mkdtempSync(path.join(os.tmpdir(), 'knut-settings-smoke-'));
const binary = knutBinary();
const config = path.join(artifacts, 'config');
const catalog = Array.from({ length: 45 }, (_, index) => ({
  id: `model-${String(index).padStart(2, '0')}`,
  name: `Fixture model ${index}`,
}));
let requests = 0;
let failCatalog = false;
let terminal;
let frames = 0;

const provider = http.createServer((request, response) => {
  assert.equal(request.method, 'GET', 'Browsing settings must not call inference');
  assert.equal(request.url, '/v1/models');
  requests++;
  response.setHeader('Content-Type', 'application/json');
  response.writeHead(failCatalog ? 503 : 200);
  response.end(JSON.stringify(failCatalog ? { error: { message: 'Temporary catalog failure' } } : { data: catalog }));
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));

const env = {
  ...process.env,
  KNUT_LOAD_ENV: '0',
  KNUT_CONFIG_DIR: config,
  KNUT_SESSION_STORE: path.join(artifacts, 'sessions.db'),
  KNUT_PROFILE: 'general',
  KNUT_PROVIDER: 'chat-completions',
  KNUT_PROVIDER_API_KEY: 'local-fixture',
  KNUT_PROVIDER_MODEL: 'model-00',
  KNUT_PROVIDER_BASE_URL: `http://127.0.0.1:${provider.address().port}/v1`,
  KNUT_PROVIDER_REASONING_EFFORT: undefined,
  TYPESAFE_API_KEY: undefined,
  KNUT_TUI_MOTION: undefined,
  KNUT_TUI_COLORS: 'truecolor',
  NO_COLOR: undefined,
};

async function start() {
  terminal = await launchTerminal({ command: binary, args: ['tui'], cwd: artifacts, cols: 100, rows: 32, env });
  await terminal.waitForText('What are we building?', { timeout: 10000 });
}

async function snapshot(name) {
  const text = await terminal.text({ trimEnd: true });
  fs.writeFileSync(path.join(artifacts, `${name}.txt`), text);
  frames++;
  return text;
}

async function press(keys, name) {
  await terminal.press(keys);
  return snapshot(name);
}

async function exit() {
  await terminal.press(['ctrl', 'q']);
  assert(await terminal.waitForExit(5000));
  terminal.close();
}

try {
  await start();
  await terminal.type('unfinished å draft');
  await snapshot('draft');
  await press('f4', 'loading-models');
  await terminal.waitForText('45 / 45 models', { timeout: 10000 });
  assert((await snapshot('catalog')).includes('model-00'));
  assert((await press('end', 'last-model')).includes('model-44'));
  await terminal.type('no-such-model');
  assert((await snapshot('no-matches')).includes('No matching models'));
  assert((await press('enter', 'no-accidental-selection')).includes('No matching models'));
  await press(['ctrl', 'u'], 'clear-query');
  await terminal.type('model-31');
  const filtered = await snapshot('filtered');
  assert(filtered.includes('1 / 45 models'));
  assert(filtered.includes('model-31'));
  await press('enter', 'select-model');
  await terminal.waitForText('Using API connection', { timeout: 10000 });
  assert((await snapshot('model-selected')).includes('unfinished å draft'));
  const saved = JSON.parse(fs.readFileSync(path.join(config, 'openai-accounts.json'), 'utf8'));
  assert.deepEqual(Object.values(saved.api_models), ['model-31']);
  assert(!JSON.stringify(saved).includes('local-fixture'));
  await exit();

  await start();
  await terminal.waitForText('Draft restored', { timeout: 10000 });
  const restored = await snapshot('restored');
  assert(restored.includes('model-31'));
  assert(restored.includes('unfinished å draft'));
  await press('f4', 'reopen-picker');
  await terminal.waitForText('45 / 45 models', { timeout: 10000 });
  assert((await snapshot('current-model')).includes('current'));
  failCatalog = true;
  await press(['ctrl', 'r'], 'refresh-models');
  await terminal.waitForText('Could not load models', { timeout: 10000 });
  const failedRefresh = await snapshot('refresh-error');
  assert(failedRefresh.includes('model-31'));
  assert(failedRefresh.includes('45 / 45 models'));
  await press('esc', 'close-picker');
  failCatalog = false;

  await press('f2', 'settings');
  assert((await press('end', 'background-setting')).includes('Background: Knut'));
  assert((await press('enter', 'terminal-background')).includes('Background: terminal'));
  assert((await press('up', 'motion-setting')).includes('Motion: animated'));
  assert((await press('enter', 'reduce-motion')).includes('Motion: reduced'));
  const preferences = JSON.parse(fs.readFileSync(path.join(config, 'terminal-preferences.json'), 'utf8'));
  assert.equal(preferences.reduced_motion, true);
  assert.equal(preferences.terminal_background, true);
  await press('esc', 'close-settings');
  await exit();

  await start();
  await press('f2', 'restored-settings');
  const restoredSettings = await snapshot('appearance-restored');
  assert(restoredSettings.includes('Motion: reduced'));
  assert(restoredSettings.includes('Background: terminal'));
  assert(restoredSettings.includes('Current  model-31'));
  await press('home', 'first-setting');
  for (let index = 0; index < 3; index++) await press('down', `reset-navigation-${index}`);
  await press('enter', 'reset-to-environment');
  await terminal.waitForText('Using API connection', { timeout: 10000 });
  assert((await snapshot('reset-model')).includes('model-00'));
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(config, 'openai-accounts.json'), 'utf8')).api_models, {});
  assert.equal(requests, 3);
  await exit();
  console.log(JSON.stringify({ artifacts, frames, result: 'passed', modelPersistence: 'passed', appearancePersistence: 'passed' }));
} finally {
  terminal?.close();
  provider.close();
  provider.closeAllConnections();
}
