import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const application = path.join(root, 'target', 'debug', 'dntdatabase.exe');

// The msedgedriver that matches the installed WebView2 runtime (see README).
const nativeDriver = process.env.MSEDGEDRIVER ?? 'D:\\dev\\msedgedriver\\msedgedriver.exe';
const tauriDriver = process.env.TAURI_DRIVER ?? path.join(process.env.CARGO_HOME ?? path.join(os.homedir(), '.cargo'), 'bin', 'tauri-driver.exe');

let driver: ChildProcess | undefined;
let dataDir = '';
let credentialsBefore = new Set<string>();

/** Credential Manager targets written by the app: `DntDatabase/<uuid>/<kind>`. */
function appCredentials(): Set<string> {
  const out = execFileSync('cmdkey', ['/list'], { encoding: 'utf8' });
  const targets = out.match(/target=(\S*DntDatabase\/[0-9a-f-]{36}\/\w+\S*)/gi) ?? [];
  return new Set(targets.map((t) => t.slice('target='.length)));
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

  onPrepare() {
    if (!fs.existsSync(application)) {
      throw new Error(`Missing ${application}; build it first (see README, "End-to-end smoke test").`);
    }
    // A fresh data dir per run; the app (spawned by the driver) inherits this environment.
    dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'dnt-e2e-'));
    process.env.DNT_DATA_DIR = dataDir;
    credentialsBefore = appCredentials();
    driver = spawn(tauriDriver, ['--native-driver', nativeDriver], {
      stdio: ['ignore', process.stdout, process.stderr],
    });
    driver.on('error', (e) => {
      throw new Error(`tauri-driver failed to start (${tauriDriver}): ${e.message}`);
    });
    // Give the driver time to open its port before the first session request.
    return new Promise<void>((resolve) => setTimeout(resolve, 2000));
  },

  onComplete() {
    driver?.kill();
    // Remove the password the test stored in Windows Credential Manager.
    for (const target of appCredentials()) {
      if (!credentialsBefore.has(target)) {
        execFileSync('cmdkey', [`/delete:${target}`]);
        console.log(`removed test credential ${target}`);
      }
    }
    fs.rmSync(dataDir, { recursive: true, force: true });
  },
};
