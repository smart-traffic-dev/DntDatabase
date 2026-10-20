import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { browser } from '@wdio/globals';

const here = path.dirname(fileURLToPath(import.meta.url));

/** Screenshots land in e2e/evidence/<run stamp>/ (gitignored); E2E_EVIDENCE_DIR overrides. */
export const evidenceDir =
  process.env.E2E_EVIDENCE_DIR ??
  path.join(here, 'evidence', new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19));

export async function evidence(name: string): Promise<void> {
  fs.mkdirSync(evidenceDir, { recursive: true });
  await browser.saveScreenshot(path.join(evidenceDir, `${name}.png`));
}
