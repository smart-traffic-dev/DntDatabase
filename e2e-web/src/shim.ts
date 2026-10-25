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
 * no Tauri runtime, no HTTP server. `listen` of `@tauri-apps/api/event` works too (the event
 * plugin's listen / unlisten are answered in the page, see `emitEvent`). There is no window metadata, so the window API
 * (`getCurrentWindow`) throws, which the app treats as "not inside Tauri".
 */
const INIT_SCRIPT = `(() => {
  // transformCallback registry (channels and event handlers), as Tauri keeps it: id -> function.
  const callbacks = new Map();
  let nextCallback = 1;
  // Event listeners: what \`plugin:event|listen\` registered, as {event, eventId, handler}.
  let listeners = [];
  let nextEventId = 1;
  const unregisterListener = (event, eventId) => {
    listeners = listeners.filter((l) => !(l.event === event && l.eventId === eventId));
  };

  // The event plugin's commands stay in the page: the backend's emit() is \`window.__e2eEmit\`.
  const local = {
    'plugin:event|listen': ({ event, handler }) => {
      const eventId = nextEventId++;
      listeners.push({ event, eventId, handler });
      return eventId;
    },
    'plugin:event|unlisten': ({ event, eventId }) => {
      unregisterListener(event, eventId);
      return null;
    },
  };

  window.__TAURI_INTERNALS__ = {
    invoke: async (cmd, args) => {
      if (local[cmd]) return local[cmd](args ?? {});
      const reply = await window.__e2eInvoke(cmd, args ?? {});
      if (reply.ok) return reply.value;
      // Tauri rejects with the deserialized error payload (a plain object), not an Error.
      // tauri-specta turns that into {status:"error"} and src/api turns it into AppErrorEx.
      throw reply.error;
    },
    transformCallback: (callback, once = false) => {
      const id = nextCallback++;
      callbacks.set(id, (data) => {
        if (once) callbacks.delete(id);
        return callback && callback(data);
      });
      return id;
    },
    unregisterCallback: (id) => {
      callbacks.delete(id);
    },
    runCallback: (id, data) => {
      const callback = callbacks.get(id);
      if (callback) callback(data);
    },
    convertFileSrc: (path) => path,
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener };

  /** Number of listeners of \`event\` (emitEvent waits for one). */
  window.__e2eListenerCount = (event) => listeners.filter((l) => l.event === event).length;
  /** Delivers an app event as Tauri does: each handler gets {event, id, payload}. */
  window.__e2eEmit = (event, payload) => {
    const targets = listeners.filter((l) => l.event === event);
    for (const l of targets) {
      const callback = callbacks.get(l.handler);
      if (callback) callback({ event, id: l.eventId, payload });
    }
    return targets.length;
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

/**
 * Emits a Tauri app event to the page, as the backend's `Emitter::emit` would: every handler
 * registered through `listen(event, …)` of `@tauri-apps/api/event` receives `{event, id, payload}`.
 * Waits until the UI listens to `event` (its `listen` call is asynchronous), so an event is never
 * sent into the void.
 */
export async function emitEvent(page: Page, event: string, payload: unknown): Promise<void> {
  await page.waitForFunction(
    (name) => (window as unknown as { __e2eListenerCount(e: string): number }).__e2eListenerCount(name) > 0,
    event,
  );
  await page.evaluate(
    ([name, data]) => (window as unknown as { __e2eEmit(e: string, p: unknown): number }).__e2eEmit(name as string, data),
    [event, payload] as const,
  );
}
