import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export function knutBinary() {
  if (process.env.KNUT_BINARY) return path.resolve(process.env.KNUT_BINARY);
  const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const result = spawnSync(
    'cargo',
    ['metadata', '--no-deps', '--format-version', '1', '--locked'],
    { cwd: repo, encoding: 'utf8' },
  );
  if (result.status !== 0) {
    throw new Error(`Could not locate the Cargo build directory: ${result.stderr}`);
  }
  return path.join(JSON.parse(result.stdout).target_directory, 'debug', 'knut');
}
