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

/** Whether the prompt dialogs took over the error (nothing else to show). */
export const promptHandles = (error: AppError) =>
  isPasswordRequired(error) || isHostKeyUnknown(error) || isHostKeyMismatch(error);
