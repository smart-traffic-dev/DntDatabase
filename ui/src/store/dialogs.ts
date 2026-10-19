import { create } from 'zustand';
import type { AppError, NodeId } from '../bindings';

/** A prompt the connection tree needs answered before it can open a connection. */
export interface PendingPrompt {
  kind: 'password' | 'hostKey';
  connectionId: string;
  error: AppError;
}

/** Something the UI asks a dialog (built in a later task) to do. */
export type UiRequest =
  | { kind: 'newConnection' }
  | { kind: 'editConnection'; id: string }
  | { kind: 'newGroup'; parentId: string | null }
  | { kind: 'renameGroup'; id: string }
  | { kind: 'deleteNode'; node: NodeId; label: string }
  | { kind: 'export'; preselected?: NodeId[] }
  | { kind: 'import' };

interface DialogsState {
  /** Set when opening a connection needs a password (`PasswordPrompt`) or host-key trust (`HostKeyDialog`). */
  prompt: PendingPrompt | null;
  request: UiRequest | null;
  /** A short status message shown as a toast (e.g. the import summary). */
  notice: string | null;
  setNotice(notice: string | null): void;
  showPrompt(prompt: PendingPrompt): void;
  closePrompt(): void;
  requestDialog(request: UiRequest): void;
  closeRequest(): void;
}

export const useDialogs = create<DialogsState>()((set) => ({
  prompt: null,
  request: null,
  notice: null,
  setNotice: (notice) => set({ notice }),
  showPrompt: (prompt) => set({ prompt }),
  closePrompt: () => set({ prompt: null }),
  requestDialog: (request) => set({ request }),
  closeRequest: () => set({ request: null }),
}));
