import { api, isHostKeyMismatch, isHostKeyUnknown, isPasswordRequired, toAppError } from '../api';
import type { AppError } from '../bindings';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { useTree } from './tree';

/**
 * Opens a connection again after `not_open` / `connection_lost` ("Reconnect"). Records its
 * server info on success. A missing password or an untrusted host key opens the matching
 * prompt (which finishes the open). Returns the error, or null when the connection is open.
 */
export async function reopenConnection(connectionId: string): Promise<AppError | null> {
  try {
    const opened = await api.openConnection(connectionId, null);
    useConnections.getState().setInfo(connectionId, opened.info);
    // The tree shows the connection as open again (engine chip, version).
    void useTree.getState().reloadStored();
    return null;
  } catch (e) {
    const error = toAppError(e);
    if (isPasswordRequired(error)) {
      useDialogs.getState().showPrompt({ kind: 'password', connectionId, error });
    } else if (isHostKeyUnknown(error) || isHostKeyMismatch(error)) {
      useDialogs.getState().showPrompt({ kind: 'hostKey', connectionId, error });
    }
    return error;
  }
}

/** How long a transient quiet-open failure (an unreachable server) waits before a retry. */
const QUIET_RETRY_MS = 10_000;
/**
 * Per connection: the quiet open in flight, or until when a failed one is not retried
 * (`Infinity` until the connection is opened some other way).
 */
const quiet = new Map<string, Promise<boolean> | number>();

// Opened by the user (a prompt, the tree, a run): a blocked quiet open may try again later.
useConnections.subscribe((s, prev) => {
  for (const id of s.info.keys()) if (!prev.info.has(id) && typeof quiet.get(id) === 'number') quiet.delete(id);
});

/**
 * Opens a connection without any prompt (autocomplete of a restored tab whose connection is
 * not open yet). Concurrent calls share one open, which resolves `false` on failure. A failure
 * that retrying cannot fix (a password or host key to ask for, rejected credentials) is never
 * retried quietly: a login per keystroke window would only pile up failed logins on the
 * server. The next run reopens with its prompts as usual. A transient one waits `QUIET_RETRY_MS`.
 * A Production connection, or one already being opened, is left alone.
 */
export function openQuietly(connectionId: string): Promise<boolean> {
  if (useConnections.getState().info.has(connectionId)) return Promise.resolve(true);
  const pending = quiet.get(connectionId);
  if (pending instanceof Promise) return pending;
  if (pending !== undefined && pending > Date.now()) return Promise.resolve(false);
  // Production is never logged into behind the user's back: only an explicit open (the tree,
  // a run, Reconnect) connects it.
  if (useConnections.getState().summaries.get(connectionId)?.config.env_tag === 'production') {
    quiet.set(connectionId, Infinity);
    return Promise.resolve(false);
  }
  const open = api
    .connectionState(connectionId)
    // The state is only a hint: when it cannot be read, open as before.
    .catch(() => ({ state: 'disconnected' as const }))
    // An open already in flight (the tree, a run, a prompt): a second `open_connection` would
    // supersede it and cancel the user's own open. Its result reaches `useConnections` anyway.
    .then((s) => (s.state === 'connecting' || s.state === 'reconnecting' ? null : api.openConnection(connectionId, null)))
    .then((opened) => {
      if (!opened) {
        quiet.set(connectionId, Date.now() + QUIET_RETRY_MS);
        return false;
      }
      quiet.delete(connectionId);
      useConnections.getState().setInfo(connectionId, opened.info);
      void useTree.getState().reloadStored();
      return true;
    })
    .catch((e: unknown) => {
      const error = toAppError(e);
      quiet.set(connectionId, error.retryable && !promptHandles(error) ? Date.now() + QUIET_RETRY_MS : Infinity);
      return false;
    });
  quiet.set(connectionId, open);
  return open;
}

/** Whether a quiet open of the connection failed recently (it resolves `false` at once). */
export function quietOpenFailed(connectionId: string): boolean {
  const pending = quiet.get(connectionId);
  return typeof pending === 'number' && pending > Date.now();
}

/** Forgets the quiet opens that failed (tests). */
export function resetQuietOpens(): void {
  quiet.clear();
}

/** Whether the prompt dialogs took over the error (nothing else to show). */
export const promptHandles = (error: AppError) =>
  isPasswordRequired(error) || isHostKeyUnknown(error) || isHostKeyMismatch(error);
