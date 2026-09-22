import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const [operation, directory, arm, effort = 'high'] = process.argv.slice(2);
if (!directory || !['prepare', 'run', 'verify'].includes(operation)) {
  throw new Error('Usage: node scripts/compare-harnesses.mjs prepare|run|verify <directory> [knut|ante] [low|high|max]');
}
const root = fs.realpathSync(directory);
const prompt = 'Fix the composer character-limit bug. len_chars must count every Unicode scalar in text(), including line separators. All insertion paths (typing, insert_newline, and multiline paste) must respect MAX_COMPOSER_CHARS. At capacity a newline must be a no-op, preserving text and cursor; at one character below capacity it must fit. Preserve Unicode editing and undo behavior. Add focused regression tests and run relevant checks. Make the actual edits; do not just describe a patch. Keep changes limited to this bug and do not weaken or delete existing tests.';

function command(program, args, cwd = source, env = process.env) {
  const result = spawnSync(program, args, { cwd, env, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  if (result.status !== 0) throw new Error(`${program} failed: ${result.stderr || result.error || result.status}`);
  return result.stdout;
}

function save(file, value) {
  fs.writeFileSync(file, typeof value === 'string' ? value : JSON.stringify(value, null, 2), { flag: 'wx', mode: 0o600 });
}

function credentials() {
  const env = { ...process.env };
  const file = path.join(source, '.env');
  if (fs.existsSync(file)) {
    for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
      const match = line.match(/^\s*(?:export\s+)?([A-Z_][A-Z0-9_]*)\s*=\s*(.*?)\s*$/);
      if (!match || !['KNUT_PROVIDER_API_KEY', 'ZAI_API_KEY', 'TYPESAFE_API_KEY'].includes(match[1])) continue;
      let value = match[2];
      if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) value = value.slice(1, -1);
      env[match[1]] ??= value;
    }
  }
  const key = env.KNUT_PROVIDER_API_KEY || env.ZAI_API_KEY;
  if (!key) throw new Error('Missing configured Z.ai credential');
  env.ZAI_API_KEY = key;
  env.KNUT_PROVIDER_API_KEY = key;
  env.KNUT_PROVIDER_BASE_URL = 'https://api.z.ai/api/coding/paas/v4';
  env.KNUT_PROVIDER_MODEL = 'glm-5.3-flash';
  env.KNUT_PROVIDER_REASONING_EFFORT = effort;
  env.KNUT_PROVIDER_TIMEOUT_SECONDS = '300';
  env.CARGO_NET_OFFLINE = 'true';
  delete env.ANTE_PROFILE;
  delete env.KNUT_PROVIDER_TIER;
  return env;
}

function sandbox(work, state) {
  const args = ['--die-with-parent', '--unshare-pid', '--ro-bind', '/', '/', '--tmpfs', '/tmp', '--tmpfs', source, '--tmpfs', root];
  for (const location of [work, state, path.join(root, 'bin')]) {
    args.push('--bind', location, location);
  }
  for (const location of [path.join(os.homedir(), '.cache/mbx'), path.join(os.homedir(), '.cargo')]) {
    if (fs.existsSync(location)) args.push('--bind', location, location);
  }
  args.push('--proc', '/proc', '--dev', '/dev', '--chdir', work, '--');
  return args;
}

async function execute(program, args, cwd, env, log, seconds) {
  const fd = fs.openSync(log, 'wx', 0o600);
  const started = performance.now();
  const child = spawn(program, args, { cwd, env, detached: true, stdio: ['ignore', fd, fd] });
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    try { process.kill(-child.pid, 'SIGKILL'); } catch {}
  }, seconds * 1000);
  return await new Promise((resolve, reject) => {
    child.once('error', error => { clearTimeout(timer); fs.closeSync(fd); reject(error); });
    child.once('close', (code, signal) => {
      clearTimeout(timer);
      fs.closeSync(fd);
      resolve({ exit_code: code, signal, timed_out: timedOut, elapsed_ms: Math.round(performance.now() - started) });
    });
  });
}

if (operation === 'prepare') {
  if (fs.statfsSync(root).type === 0x01021994) {
    throw new Error('Use disk-backed storage, not tmpfs: Rust build outputs can exceed /tmp quotas. Try mktemp -d /var/tmp/knut-comparison.XXXXXX');
  }
  fs.mkdirSync(path.join(root, 'baseline'));
  fs.mkdirSync(path.join(root, 'bin'));
  const driver = fs.readFileSync(fileURLToPath(import.meta.url));
  save(path.join(root, 'comparison-driver.mjs'), driver.toString('utf8'));
  const files = command('git', ['ls-files', '--cached', '--others', '--exclude-standard', '-z']).split('\0').filter(Boolean)
    .filter(file => !/^(scripts|\.agents|reports)\//.test(file) && !/(^|\/)\.env(?:\.|$)/.test(file));
  const hash = crypto.createHash('sha256');
  for (const file of [...new Set(files)].sort()) {
    const origin = path.join(source, file);
    if (!fs.existsSync(origin) || !fs.lstatSync(origin).isFile()) continue;
    const bytes = fs.readFileSync(origin);
    hash.update(file).update('\0').update(bytes).update('\0');
    const destination = path.join(root, 'baseline', file);
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.copyFileSync(origin, destination);
  }
  fs.copyFileSync(path.join(source, 'target/debug/knut'), path.join(root, 'bin/knut'));
  fs.chmodSync(path.join(root, 'bin/knut'), 0o700);
  save(path.join(root, 'manifest.json'), {
    schema: 1, task: 'composer-newline-budget', prompt, snapshot_sha256: hash.digest('hex'),
    driver_sha256: crypto.createHash('sha256').update(driver).digest('hex'),
    source_commit: command('git', ['rev-parse', 'HEAD']).trim(), source_dirty: true,
    model: 'glm-5.3-flash', endpoint: 'https://api.z.ai/api/coding/paas/v4',
    ante_version: command('ante', ['--version']).trim(),
    knut_binary_sha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(root, 'bin/knut'))).digest('hex'),
    limitations: ['One development task, not a held-out evaluation', 'Native harness tools and prompts differ', 'Knut leaves sampling/output limits to provider defaults; Ante sends temperature=1, top_p=0.95, max_tokens=131072', 'Knut request timeout=300s; both outer run limits=900s', 'Wall time includes native checks; build caches may warm', 'Jev cost and cached-token accounting incomplete'],
  });
  console.log(`Prepared isolated snapshot: ${root}`);
} else {
  if (!['knut', 'ante'].includes(arm) || !['low', 'high', 'max'].includes(effort)) throw new Error('Invalid arm or native effort');
  const run = path.join(root, `${arm}-${effort}`);
  const work = path.join(run, 'workspace');
  const state = path.join(run, 'state');
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'manifest.json'), 'utf8'));
  if (operation === 'run') {
    fs.mkdirSync(run);
    fs.cpSync(path.join(root, 'baseline'), work, { recursive: true });
    fs.mkdirSync(state);
    command('git', ['init', '-q'], work);
    command('git', ['add', '.'], work);
    command('git', ['-c', 'user.name=Knut evaluation', '-c', 'user.email=eval@localhost', 'commit', '-qm', 'Matched evaluation baseline'], work);
    const env = credentials();
    env.ANTE_HOME = state;
    env.CARGO_TARGET_DIR = path.join(work, 'target');
    if (arm === 'ante') {
      save(path.join(state, 'catalog.json'), { providers: { 'zai-coding-plan': {
        display_name: 'Z.ai GLM Coding Plan', base_url: manifest.endpoint,
        wire_style: 'OpenAiCompatible', thinking_display: 'detailed',
        auth: { bearer: { env_key: 'ZAI_API_KEY' } },
        preferred_models: [{ id: manifest.model, supported_efforts: ['low', 'high', 'max'],
          effort, temperature: 1, top_p: 0.95, max_tokens: 131072, context_limit: 1000000, support_vision: true }],
      } } });
      const catalog = JSON.parse(command('ante', ['catalog'], work, env));
      const provider = catalog.providers.find(provider => provider.id === 'zai-coding-plan');
      if (provider?.base_url !== manifest.endpoint) throw new Error('Isolated Ante endpoint did not match; refusing fallback');
    }
    const args = arm === 'knut'
      ? [path.join(root, 'bin/knut'), 'run', manifest.prompt, '--yes', '--verbose', '--census', path.join(state, 'census.json')]
      : [command('which', ['ante']).trim(), '--profile', 'bare', '--provider', 'zai-coding-plan', '--model', manifest.model, '--effort', effort, '--permission-mode', 'yolo', '--no-skills', '--disable-auto-memory', '--no-session-save', '--tools', 'Read,Write,Edit,Glob,Grep,Bash', '--output-format', 'json', '--prompt', manifest.prompt];
    console.log(`Starting ${arm}, ${manifest.model}, effort=${effort}, 15-minute limit`);
    const result = await execute('bwrap', [...sandbox(work, state), ...args], work, env, path.join(run, 'output.log'), 900);
    save(path.join(run, 'result.json'), { ...result, arm, effort, model: manifest.model, jev_configured: arm === 'knut' && Boolean(env.TYPESAFE_API_KEY), args });
    command('git', ['add', '-N', '--', '.'], work);
    save(path.join(run, 'patch.diff'), command('git', ['diff', '--no-ext-diff', 'HEAD'], work));
    console.log(JSON.stringify({ arm, effort, ...result }));
  } else {
    command('git', ['add', '-N', '--', '.'], work);
    const changedPaths = command('git', ['diff', '--name-only', 'HEAD'], work).trim().split('\n').filter(Boolean);
    const scopeOk = changedPaths.length > 0 && changedPaths.every(file => file === 'src/composer.rs' || file.startsWith('tests/') || file.startsWith('src/composer/'));
    const hidden = path.join(work, 'tests', 'evaluation_composer_budget.rs');
    fs.mkdirSync(path.dirname(hidden), { recursive: true });
    save(hidden, `use knut::{Composer, MAX_COMPOSER_CHARS};
#[test] fn multiline_count_matches_serialized_text() {
 let mut c=Composer::new(); c.paste("å\\n界\\nx"); assert_eq!(c.len_chars(), c.text().chars().count());
}
#[test] fn newline_at_capacity_preserves_text_and_cursor() {
 let mut c=Composer::new(); c.insert(&"x".repeat(MAX_COMPOSER_CHARS)); let before=c.text(); let cursor=c.cursor();
 c.insert_newline(); assert_eq!(c.text(),before); assert_eq!(c.cursor(),cursor);
}
#[test] fn one_newline_fits_but_two_do_not() {
 let mut c=Composer::new(); c.insert(&"x".repeat(MAX_COMPOSER_CHARS-1)); c.insert_newline();
 assert_eq!(c.text().chars().count(),MAX_COMPOSER_CHARS); c.insert_newline(); assert_eq!(c.text().chars().count(),MAX_COMPOSER_CHARS);
}
#[test] fn multiline_paste_respects_remaining_budget() {
 let mut c=Composer::new(); c.insert(&"x".repeat(MAX_COMPOSER_CHARS-3)); c.paste("å\\n界\\nz");
 assert!(c.text().chars().count()<=MAX_COMPOSER_CHARS); assert_eq!(c.len_chars(),c.text().chars().count());
}
`);
    const env = { ...process.env, CARGO_TARGET_DIR: path.join(work, 'target'), CARGO_NET_OFFLINE: 'true' };
    delete env.KNUT_LIVE_SMOKE;
    const result = await execute('bwrap', [...sandbox(work, state), 'cargo', 'test', '--locked', '--quiet'], work, env, path.join(run, 'verification.log'), 900);
    const verification = { ...result, changed_paths: changedPaths, scope_ok: scopeOk };
    save(path.join(run, 'verification.json'), verification);
    console.log(JSON.stringify({ arm, effort, verification }));
  }
}
