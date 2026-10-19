import { commands, type Result } from '../bindings';
import { AppErrorEx, toAppError } from './errors';

export { AppErrorEx, isHostKeyUnknown, isPasswordRequired, toAppError } from './errors';

type Commands = typeof commands;
type Unwrap<R> = R extends Promise<Result<infer T, unknown>> ? T : never;

/** `commands` with every call resolving to its value and rejecting with `AppErrorEx`. */
export type Api = {
  [K in keyof Commands]: (...args: Parameters<Commands[K]>) => Promise<Unwrap<ReturnType<Commands[K]>>>;
};

async function unwrap<T>(call: () => Promise<Result<T, unknown>>): Promise<T> {
  let result: Result<T, unknown>;
  try {
    result = await call();
  } catch (e) {
    // The generated bindings rethrow real `Error`s (e.g. no Tauri runtime): keep one shape.
    throw new AppErrorEx(toAppError(e));
  }
  if (result.status === 'error') throw new AppErrorEx(toAppError(result.error));
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
