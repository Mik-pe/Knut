import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';
import {knutBinary} from './binary.mjs';

const require = createRequire(import.meta.url);
const modulePath = require.resolve(process.env.KNUT_TUISTORY_MODULE || 'tuistory');
const {launchTerminal} = await import(modulePath);
const captureImages = process.argv.includes('--screenshots');
const renderer = captureImages
  ? await import(createRequire(modulePath).resolve('ghostty-opentui/image')) : null;
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const binary = knutBinary();
const artifacts = fs.mkdtempSync(path.join(os.tmpdir(), 'knut-tui-smoke-'));
let provider, terminal;
let frames = 0;

async function snapshot(name, image = false) {
  const text = await terminal.text({trimEnd: true});
  fs.writeFileSync(path.join(artifacts, `${name}.txt`), text);
  if (image && renderer) {
    const png = await renderer.renderTerminalToImage(terminal.getTerminalData(), {
      fontSize: 16, lineHeight: 1.5, paddingX: 20, paddingY: 20, devicePixelRatio: 1.5,
      theme: {background: '#12171b', text: '#e5e2dc'},
    });
    fs.writeFileSync(path.join(artifacts, `${name}.png`), png);
  }
  frames++;
  return text;
}

async function press(keys, name) {
  await terminal.press(keys);
  return snapshot(name);
}

try {
  provider = await launchTerminal({command: 'node', args: [path.join(repo, 'scripts/smoke-harness.mjs'), '--serve'], cols: 160, rows: 8});
  const ready = await provider.waitForText('"ready":true', {timeout: 15000});
  const connection = JSON.parse(ready.split('\n').find(line => line.trim().startsWith('{')).trim());
  const workspace = path.join(artifacts, 'Knut');
  fs.cpSync(connection.root, workspace, {recursive: true});
  const original = fs.readFileSync(path.join(workspace, 'src/lib.rs'), 'utf8');
  const env = {...process.env, KNUT_CONFIG_DIR: path.join(artifacts, 'config'), KNUT_PROFILE: 'coding', KNUT_PROVIDER: 'chat-completions', TYPESAFE_API_KEY: undefined, KNUT_PROVIDER_API_KEY: 'local-fixture',
    KNUT_PROVIDER_MODEL: 'fixture', KNUT_PROVIDER_BASE_URL: connection.baseUrl,
    KNUT_PROVIDER_REASONING_EFFORT: undefined, CARGO_NET_OFFLINE: 'true',
    NO_COLOR: '1', KNUT_TUI_COLORS: 'truecolor', KNUT_TUI_MOTION: 'on',
    KNUT_SESSION_STORE: path.join(artifacts, 'sessions.db')};
  terminal = await launchTerminal({command: binary, args: ['tui'], cwd: workspace, cols: 100, rows: 32, env});
  await terminal.waitForText('What are we building?', {timeout: 15000});
  await snapshot('welcome-100x32', true);
  assert(terminal.getRawOutput().includes('38;2;'), 'explicit color override did not reach the terminal');
  for (const [cols, rows] of [[60,18], [80,24], [140,40], [30,12]]) {
    terminal.resize({cols, rows});
    const text = await snapshot(`welcome-${cols}x${rows}`, true);
    assert(text.includes('Enter send'));
    assert(text.includes('F1 help'));
  }
  await press('f1', 'narrow-help');
  await press('pagedown', 'help-scroll');
  assert((await press('pagedown', 'help-word-shortcuts')).includes('Alt+B / F'));
  await press('esc', 'close-narrow-help');
  terminal.resize({cols: 80, rows: 24});
  await snapshot('resized');
  await terminal.type('keep first second');
  await snapshot('draft');
  assert((await press(['ctrl', 'w'], 'delete-word')).includes('keep first'));
  assert(!(await terminal.text({trimEnd: true})).includes('keep first second'));
  assert((await press(['ctrl', 'z'], 'undo-word')).includes('keep first second'));
  await press('f1', 'open-help');
  await snapshot('help-80x24', true);
  await press('esc', 'close-help');
  assert((await press('f2', 'open-settings')).includes('Settings'));
  await snapshot('settings-80x24', true);
  assert((await press('esc', 'close-settings')).includes('keep first second'));
  await press(['ctrl', 'c'], 'clear-draft');
  await terminal.type('/motion');
  await snapshot('motion-palette', true);
  assert((await press('enter', 'motion-off')).includes('Reduced motion on'));
  await terminal.type('Fix value() to return seven; preserve the test.');
  await snapshot('task-draft');
  await press('enter', 'submit');
  await terminal.waitForText('proposed action', {timeout: 30000});
  await snapshot('approval', true);
  await terminal.type('unfinished draft');
  await snapshot('draft-at-approval');
  assert((await press('enter', 'keep-draft')).includes('unfinished draft'));
  await press(['alt', 'a'], 'approve-first');
  await terminal.waitForText('proposed action', {timeout: 60000});
  await snapshot('repair-approval', true);
  await press(['alt', 'a'], 'approve-repair');
  await terminal.waitForText('Verified:', {timeout: 60000});
  assert((await snapshot('completed', true)).includes('unfinished draft'));
  await press(['alt', 'b'], 'position-before-restart');
  await terminal.press(['ctrl', 'q']);
  assert(await terminal.waitForExit(5000));
  terminal.close();
  terminal = await launchTerminal({command: binary, args: ['tui'], cwd: workspace, cols: 80, rows: 24, env});
  await terminal.waitForText('Draft restored', {timeout: 10000});
  assert((await snapshot('restored-draft', true)).includes('unfinished draft'));
  await terminal.type('restored ');
  assert((await snapshot('restored-cursor')).includes('unfinished restored draft'));
  await press(['ctrl', 'c'], 'clear-restored-draft');
  assert((await press('up', 'restored-history')).includes('Fix value() to return seven'));
  await press('down', 'return-to-cleared-draft');
  await terminal.press(['ctrl', 'd']);
  assert(await terminal.waitForExit(5000));
  assert.equal(fs.readFileSync(path.join(workspace, 'src/lib.rs'), 'utf8'), original.replace('{ 0 }', '{ 7 }'));
  terminal.close();
  // Tuistory assigns TERM after merging env; set it in the child for the dumb-terminal case.
  terminal = await launchTerminal({command: 'env', args: ['TERM=dumb', binary, 'tui'], cwd: workspace, cols: 60, rows: 18,
    env: {...env, KNUT_TUI_COLORS: 'none', KNUT_TUI_MOTION: 'off', TERM: 'dumb'}});
  await terminal.waitForText('What are we building?', {timeout: 10000});
  const plain = await snapshot('ascii-reduced-motion', true);
  assert(/^[\x00-\x7F]*$/.test(plain), 'ASCII mode contains non-ASCII text');
  assert(!terminal.getRawOutput().includes('38;2;'), 'monochrome mode emitted color');
  console.log(JSON.stringify({artifacts, frames, result: 'passed'}));
} finally {
  terminal?.close();
  provider?.close();
}
