import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const application = path.join(root, 'target', 'e2e', 'debug', 'dntdatabase.exe');
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
/** Taken in `onPrepare`; null until then, and cleanup deletes nothing without it. */
let credentialsBefore: Set<string> | null = null;

/**
 * Keychain service of the app under test (`DNT_KEYRING_SERVICE`), so the run never shares
 * entries with the user's own `DntDatabase` ones.
 */
const KEYRING_SERVICE = 'DntDatabase-e2e';

/** Credential Manager targets the app under test wrote: `DntDatabase/<uuid>/<kind>.<service>`. */
function appCredentials(): Set<string> {
  const out = execFileSync('cmdkey', ['/list'], { encoding: 'utf8' });
  const targets = out.match(/target=(\S*DntDatabase\/[0-9a-f-]{36}\/\w+\S*)/gi) ?? [];
  return new Set(
    targets.map((t) => t.slice('target='.length)).filter((t) => t.endsWith(`.${KEYRING_SERVICE}`)),
  );
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
    // `onPrepare` failed before the snapshot (WDIO still runs `onComplete`): without it
    // every entry looks new, so delete nothing.
    const before = credentialsBefore;
    if (!before) return;
    for (const target of appCredentials()) {
      if (!before.has(target)) {
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
  specs: ['./smoke.e2e.ts', './postgres.e2e.ts', './query.e2e.ts', './data-edit.e2e.ts', './object-actions.e2e.ts'],
  maxInstances: 1,
  maxInstancesPerCapability: 1, // both specs share one DNT_DATA_DIR and one tauri-driver port
  hostname: '127.0.0.1',
  port: 4444,
  path: '/',
  capabilities: [
    {
      browserName: 'wry',
      maxInstances: 1, // specs share one DNT_DATA_DIR and one tauri-driver; run them serially
      'tauri:options': { application },
    } as WebdriverIO.Capabilities,
  ],
  framework: 'mocha',
  reporters: ['spec'],
  mochaOpts: { ui: 'bdd', timeout: 90_000 },
  logLevel: 'warn',

  async onPrepare() {
    const missing: [string, string][] = [
      ['the app binary (run `pnpm --dir e2e build:app`)', application],
      ['msedgedriver (set MSEDGEDRIVER or extract it to tools/msedgedriver/)', nativeDriver],
      ['tauri-driver (set TAURI_DRIVER or run `cargo install tauri-driver --locked`)', tauriDriver],
    ];
    for (const [what, file] of missing) {
      if (!fs.existsSync(file)) throw new Error(`Missing ${what}: expected ${file}. ${README_HINT}`);
    }
    // A fresh data dir per run; the app (spawned by the driver) inherits this environment.
    dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'dnt-e2e-'));
    process.env.DNT_DATA_DIR = dataDir;
    process.env.DNT_KEYRING_SERVICE = KEYRING_SERVICE;
    // One evidence folder per run, shared with the worker process.
    process.env.E2E_EVIDENCE_DIR ??= path.join(
      here,
      'evidence',
      new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19),
    );
    console.log(`e2e evidence: ${process.env.E2E_EVIDENCE_DIR}`);
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

  async afterTest(test, _context, { passed }) {
    if (!passed) {
      // The worker's global `browser`; importing ./evidence from the launcher cannot resolve @wdio/globals.
      const dir = process.env.E2E_EVIDENCE_DIR ?? here;
      fs.mkdirSync(dir, { recursive: true });
      await browser.saveScreenshot(path.join(dir, `FAILED-${test.title.replace(/\W+/g, '_').slice(0, 60)}.png`));
    }
  },

  onComplete() {
    cleanup();
  },
};

