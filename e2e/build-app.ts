import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// Builds the UI and a dedicated E2E binary (embedded UI, own target dir so other cargo builds can't clobber it).
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const run = (cmd: string, args: string[]) => {
  console.log(`> ${cmd} ${args.join(' ')}`);
  const r = spawnSync(cmd, args, { cwd: root, stdio: 'inherit', shell: process.platform === 'win32' });
  if (r.status !== 0) process.exit(r.status ?? 1);
};

run('pnpm', ['--dir', 'ui', 'build']);
run('cargo', [
  'build', '-p', 'dntdatabase',
  '--features', 'tauri/custom-protocol',
  '--target-dir', path.join(root, 'target', 'e2e'),
]);
