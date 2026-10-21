import type { Page } from '@playwright/test';
import type { AppError } from '../../ui/src/bindings';
import { Failure, appError } from '../../ui/src/test/fixtures/data';

/** A fake backend command: receives the IPC arguments and returns the command's value. */
export type Handler = (args: Record<string, unknown>) => unknown;

/** What the exposed bridge hands back to the page (the page cannot receive thrown objects). */
type Envelope = { ok: true; value: unknown } | { ok: false; error: AppError };

/**
 * Installed before the app's own scripts. The generated bindings call
 * `window.__TAURI_INTERNALS__.invoke`, so defining it here is all the UI needs:
 * no Tauri runtime, no HTTP server. There is no window metadata, so the window API
 * (`getCurrentWindow`) throws, which the app treats as "not inside Tauri".
 */
const INIT_SCRIPT = `(() => {
  window.__TAURI_INTERNALS__ = {
    invoke: async (cmd, args) => {
      const reply = await window.__e2eInvoke(cmd, args ?? {});
      if (reply.ok) return reply.value;
      // Tauri rejects with the deserialized error payload (a plain object), not an Error.
      // tauri-specta turns that into {status:"error"} and src/api turns it into AppErrorEx.
      throw reply.error;
    },
    // Used by channels and events (streamed query results); the flows tested here do not stream.
    transformCallback: () => 0,
    convertFileSrc: (path) => path,
  };
})();`;

/** Commands a page called without a handler (or whose handler threw a non-`Failure`). */
const problems = new WeakMap<Page, string[]>();

/** What went wrong in the fake backend of `page`; the `test` fixture fails the test on any. */
export const backendProblems = (page: Page): string[] => problems.get(page) ?? [];

/**
 * Points the app at an in-memory backend. Call before `page.goto`.
 *
 * The handlers run in Node, so failures are reported with the stack trace of the test.
 * An unknown command answers with an `internal` AppError (as a real missing command
 * would fail) and is recorded, so the test fails at its end: add a handler in `backend.ts`.
 */
export async function installBackend(page: Page, handlers: Record<string, Handler>): Promise<void> {
  const seen: string[] = [];
  problems.set(page, seen);
  await page.exposeFunction('__e2eInvoke', async (cmd: string, args: Record<string, unknown> | null): Promise<Envelope> => {
    const handler = handlers[cmd];
    if (!handler) {
      const error = appError('internal', `e2e: unmocked command "${cmd}"`);
      seen.push(error.message);
      return { ok: false, error };
    }
    try {
      return { ok: true, value: await handler(args ?? {}) };
    } catch (e) {
      if (e instanceof Failure) return { ok: false, error: e.error };
      const error = appError('internal', `e2e: handler for "${cmd}" threw: ${(e as Error)?.message ?? String(e)}`);
      seen.push(error.message);
      return { ok: false, error };
    }
  });
  await page.addInitScript(INIT_SCRIPT);
}
