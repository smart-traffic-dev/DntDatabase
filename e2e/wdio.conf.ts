import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const application = path.join(root, 'target', 'debug', 'dntdatabase.exe');
const README_HINT = 'See README.md, section "End-to-end smoke test".';

/** First existing candidate, or the first candidate when none exists (used in the error message). */
function firstExisting(candidates: string[]): string {
  return candidates.find((c) => fs.existsSync(c)) ?? candidates[0];
}

// msedgedriver must match the installed WebView2 runtime (see README).
const nativeDriver = process.env.MSEDGEDRIVER
  ? process.env.MSEDGEDRIVER
  : path.join(root, 'tools', 'msedgedriver', 'msedgedriver.exe');
const tauriDriver = process.env.TAURI_DRIVER
  ? process.env.TAURI_DRIVER
  : firstExisting(
      [process.env.CARGO_HOME, path.join(os.homedir(), '.cargo')]
        .filter((d): d is string => Boolean(d))
        .map((d) => path.join(d, 'bin', 'tauri-driver.exe')),
    );

let driver: ChildProcess | undefined;
let dataDir = '';
let credentialsBefore = new Set<string>();

/** Credential Manager targets written by the app: `DntDatabase/<uuid>/<kind>`. */
function appCredentials(): Set<string> {
  const out = execFileSync('cmdkey', ['/list'], { encoding: 'utf8' });
  const targets = out.match(/target=(\S*DntDatabase\/[0-9a-f-]{36}\/\w+\S*)/gi) ?? [];
  return new Set(targets.map((t) => t.slice('target='.length)));
}

async function waitForDriver(child: ChildProcess): Promise<void> {
  const failed = new Promise<never>((_, reject) => {
    child.once('error', (e) => reject(new Error(`tauri-driver failed to start (${tauriDriver}): ${e.message}`)));
    child.once('exit', (code) => reject(new Error(`tauri-driver exited early (code ${code})`)));
  });
  const ready = (async () => {
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline) {
      try {
        const res = await fetch('http://127.0.0.1:4444/status');
        if (res.ok) return;
      } catch {
        // not listening yet
      }
      await new Promise((r) => setTimeout(r, 200));
    }
    throw new Error('tauri-driver did not answer on http://127.0.0.1:4444/status within 20 s');
  })();
  await Promise.race([ready, failed]);
}

/** Each step is independent: one failing must not skip the others. */
function cleanup() {
  const step = (name: string, fn: () => void) => {
    try {
      fn();
    } catch (e) {
      console.warn(`e2e cleanup: ${name} failed: ${(e as Error).message}`);
    }
  };
  step('stop tauri-driver tree', () => {
    if (driver?.pid) execFileSync('taskkill', ['/PID', String(driver.pid), '/T', '/F'], { stdio: 'ignore' });
  });
  step('remove test credentials', () => {
    for (const target of appCredentials()) {
      if (!credentialsBefore.has(target)) {
        execFileSync('cmdkey', [`/delete:${target}`], { stdio: 'ignore' });
        console.log(`removed test credential ${target}`);
      }
    }
  });
  step('remove data dir', () => {
    if (dataDir) fs.rmSync(dataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  });
  driver = undefined;
}

export const config: WebdriverIO.Config = {
  runner: 'local',
  specs: ['./smoke.e2e.ts'],
  maxInstances: 1,
  hostname: '127.0.0.1',
  port: 4444,
  path: '/',
  capabilities: [
    {
      browserName: 'wry',
      'tauri:options': { application },
    } as WebdriverIO.Capabilities,
  ],
  framework: 'mocha',
  reporters: ['spec'],
  mochaOpts: { ui: 'bdd', timeout: 90_000 },
  logLevel: 'warn',

  async onPrepare() {
    const missing: [string, string][] = [
      ['the app binary', application],
      ['msedgedriver (set MSEDGEDRIVER or extract it to tools/msedgedriver/)', nativeDriver],
      ['tauri-driver (set TAURI_DRIVER or run `cargo install tauri-driver --locked`)', tauriDriver],
    ];
    for (const [what, file] of missing) {
      if (!fs.existsSync(file)) throw new Error(`Missing ${what}: expected ${file}. ${README_HINT}`);
    }
    // A fresh data dir per run; the app (spawned by the driver) inherits this environment.
    dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'dnt-e2e-'));
    process.env.DNT_DATA_DIR = dataDir;
    credentialsBefore = appCredentials();
    driver = spawn(tauriDriver, ['--native-driver', nativeDriver], {
      stdio: ['ignore', process.stdout, process.stderr],
    });
    try {
      await waitForDriver(driver);
    } catch (e) {
      cleanup();
      throw e;
    }
  },

  onComplete() {
    cleanup();
  },
};
