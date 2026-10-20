import { readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

for (const directory of ['../src/', './']) {
  const root = new URL(directory, import.meta.url);
  for (const entry of readdirSync(root)) {
    if (!entry.endsWith('.js')) continue;
    const result = spawnSync(process.execPath, ['--check', fileURLToPath(new URL(entry, root))], {
      stdio: 'inherit',
    });
    if (result.error || result.status !== 0) process.exit(1);
  }
}
console.log('JavaScript syntax checks passed.');
