import { create } from 'zustand';
import type { AppError, EnvTag, NodeId, TxAction } from '../bindings';
import type { ExportRequest } from '../dialogs/resultExport';

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
  | { kind: 'import' }
  | { kind: 'settings' }
  /** Export a query result or the data view to a file. */
  | { kind: 'exportResult'; request: ExportRequest }
  /** Save `sql` (a query tab's selection or whole text) as a favorite. */
  | {
      kind: 'addFavorite';
      sql: string;
      /** Suggested name (the tab title). */
      name: string;
      connectionId: string | null;
      database: string | null;
      fromSelection: boolean;
    }
  /** The user snippets manager. */
  | { kind: 'snippets' }
  | { kind: 'shortcuts' }
  | { kind: 'about' };

/** A question a store awaits (closing a tab, quitting): `DialogHost` shows it and answers through `done`. */
export type Question =
  | {
      kind: 'txClose';
      /** Title of the tab being closed. */
      title: string;
      /** Connection the transaction runs on, and its environment (named in the dialog). */
      connectionName?: string;
      envTag?: EnvTag | null;
      /** Commits or rolls back; rejects with the backend error (the dialog stays open). */
      settle(action: TxAction): Promise<void>;
      done(closed: boolean): void;
    }
  | { kind: 'discard'; file: string; done(discard: boolean): void }
  /** Leaving a data view (closing its tab, changing page or filter) that has staged changes. */
  | { kind: 'discardEdits'; count: number; done(discard: boolean): void }
  /** An import overwrites connections whose tabs have staged edits: discard them and import, or cancel the import. */
  | { kind: 'discardForImport'; count: number; done(discard: boolean): void }
  /** Closing a tab whose query is still running: closing stops it first. */
  | { kind: 'stopRun'; title: string; done(close: boolean): void }
  /** Quitting while `count` tabs could not be saved. */
  | { kind: 'saveFailed'; count: number; done(quit: boolean): void }
  | {
      kind: 'appClose';
      /** Titles of the tabs with an open transaction. */
      tabs: string[];
      /** Rolls every transaction back, then quits. */
      quit(): Promise<void>;
      done(): void;
    };

interface DialogsState {
  /** The question shown on top of everything else (`ask` sets it). */
  question: Question | null;
  ask(question: Question): void;
  answer(): void;
  /** Set when opening a connection needs a password (`PasswordPrompt`) or host-key trust (`HostKeyDialog`). */
  prompt: PendingPrompt | null;
  request: UiRequest | null;
  /** A short status message shown as a toast (e.g. the import summary). */
  notice: string | null;
  /** Bumped by every `setNotice`, so the same text shown again restarts the toast's timer. */
  noticeSeq: number;
  /** The last `internal` error of any command: shown as a toast with "Open log folder". */
  internalError: AppError | null;
  setNotice(notice: string | null): void;
  showPrompt(prompt: PendingPrompt): void;
  closePrompt(): void;
  requestDialog(request: UiRequest): void;
  closeRequest(): void;
  reportInternal(error: AppError): void;
  dismissInternal(): void;
}

export const useDialogs = create<DialogsState>()((set) => ({
  question: null,
  ask: (question) => set({ question }),
  answer: () => set({ question: null }),
  prompt: null,
  request: null,
  notice: null,
  noticeSeq: 0,
  internalError: null,
  setNotice: (notice) => set((s) => ({ notice, noticeSeq: s.noticeSeq + 1 })),
  showPrompt: (prompt) => set({ prompt }),
  closePrompt: () => set({ prompt: null }),
  requestDialog: (request) => set({ request }),
  closeRequest: () => set({ request: null }),
  reportInternal: (internalError) => set({ internalError }),
  dismissInternal: () => set({ internalError: null }),
}));
