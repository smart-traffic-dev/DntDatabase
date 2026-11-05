import { commands, type Result } from '../bindings';
import { useDialogs } from '../store/dialogs';
import { AppErrorEx, INTERNAL_MESSAGE, isAppErrorPayload, toAppError } from './errors';

export {
  AppErrorEx,
  isCancelled,
  isHostKeyMismatch,
  isHostKeyUnknown,
  isPasswordRequired,
  needsReconnect,
  notOpenError,
  stageLabel,
  toAppError,
} from './errors';

type Commands = typeof commands;
type Unwrap<R> = R extends Promise<Result<infer T, unknown>> ? T : never;

/** `commands` with every call resolving to its value and rejecting with `AppErrorEx`. */
export type Api = {
  [K in keyof Commands]: (...args: Parameters<Commands[K]>) => Promise<Unwrap<ReturnType<Commands[K]>>>;
};

/**
 * Every `internal` failure also reaches the global toast (spec §7: "Open log folder").
 * A command that failed without an AppError (Tauri's bare string for an unknown command, a JS
 * error from a broken bridge: "Cannot read properties of undefined (reading 'invoke')") gets the
 * backend's own plain message; its raw text goes to the details (Copy details), not the screen.
 */
function failure(e: unknown): AppErrorEx {
  const raw = toAppError(e);
  const error = isAppErrorPayload(e) ? raw : { ...raw, message: INTERNAL_MESSAGE, detail: raw.message };
  if (error.kind === 'internal') useDialogs.getState().reportInternal(error);
  return new AppErrorEx(error);
}

async function unwrap<T>(call: () => Promise<Result<T, unknown>>): Promise<T> {
  let result: Result<T, unknown>;
  try {
    result = await call();
  } catch (e) {
    // The generated bindings rethrow real `Error`s (e.g. no Tauri runtime): keep one shape.
    throw failure(e);
  }
  if (result.status === 'error') throw failure(result.error);
  return result.data;
}

export const api = new Proxy({} as Api, {
  get(_target, name: string) {
    const fn = (commands as Record<string, unknown>)[name];
    if (typeof fn !== 'function') return undefined;
    return (...args: unknown[]) =>
      unwrap(() => (fn as (...a: unknown[]) => Promise<Result<unknown, unknown>>)(...args));
  },
});
