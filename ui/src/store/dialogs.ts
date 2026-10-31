import { create } from 'zustand';
import type { AppError, DumpRequest, EnvTag, ImportRequest, NodeId, ObjectRef, RestoreRequest, SeedRequest, ToolDumpRequest, TxAction, UpdateInfo } from '../bindings';
import type { ObjectActionRequest } from '../dialogs/ObjectActionDialog';
import type { ExportRequest } from '../dialogs/resultExport';
import type { SettingsTab } from '../dialogs/SettingsDialog';
import { useToasts } from './toasts';

/** A prompt the connection tree needs answered before it can open a connection. */
export interface PendingPrompt {
  kind: 'password' | 'hostKey';
  connectionId: string;
  error: AppError;
}

/** Something the UI asks a dialog (built in a later task) to do. */
export type UiRequest =
  | ({ kind: 'objectAction' } & ObjectActionRequest)
  /** Review and apply the draft of a table design tab. */
  | { kind: 'tableDesign'; tabId: string }
  | { kind: 'newConnection' }
  | { kind: 'editConnection'; id: string }
  | { kind: 'newGroup'; parentId: string | null }
  | { kind: 'renameGroup'; id: string }
  | { kind: 'deleteNode'; node: NodeId; label: string }
  | { kind: 'export'; preselected?: NodeId[] }
  | { kind: 'import' }
  /** The settings dialog, opened on `tab` (Appearance by default). */
  | { kind: 'settings'; tab?: SettingsTab }
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
  | { kind: 'about' }
  /** Help > Check for updates; `found` when the startup check already found a newer release. */
  | { kind: 'update'; found?: UpdateInfo }
  /** Review and apply the text of a DDL editor tab. */
  | { kind: 'ddlEdit'; tabId: string }
  /** The data import wizard; `table` from the tree, `params` from "Run again" (R5). */
  | { kind: 'importData'; connectionId: string; table?: ObjectRef; params?: ImportRequest }
  /** The data seeder on `table`; `params` from "Run again" (checked, reviewed and confirmed again). */
  | { kind: 'seedData'; connectionId: string; table: ObjectRef; params?: SeedRequest; rerun?: number }
  /**
   * The dump dialog on `database` (PostgreSQL: `schema`, or every schema when absent); `table` from
   * "Dump table…" (pre-selected), `params` from "Run again" (R5).
   */
  | {
      kind: 'dump';
      connectionId: string;
      database: string;
      schema?: string | null;
      table?: string;
      params?: { kind: 'dump'; request: DumpRequest } | { kind: 'tool_dump'; request: ToolDumpRequest };
      /** Set by every "Run again": a fresh dialog even when the same job is re-run twice. */
      rerun?: number;
    }
  /** The restore dialog; `params` from "Run again" (R5: analyzed and confirmed again). */
  | { kind: 'restore'; connectionId: string; database?: string; params?: RestoreRequest; rerun?: number };

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
    }
  /** Quitting while `count` background jobs are queued or running (R8): `quit` stops them all first. */
  | { kind: 'jobsRunning'; count: number; quit(): Promise<void>; done(): void };

interface DialogsState {
  objectActionRunning: boolean;
  /** A table design is being applied (quitting waits for its result). */
  tableDesignRunning: boolean;
  /** A DDL edit is being applied (quitting waits for its result). */
  ddlEditRunning: boolean;
  /** The question shown on top of everything else (`ask` sets it). */
  question: Question | null;
  ask(question: Question): void;
  answer(): void;
  /** Set when opening a connection needs a password (`PasswordPrompt`) or host-key trust (`HostKeyDialog`). */
  prompt: PendingPrompt | null;
  request: UiRequest | null;
  /** The latest short status message shown as a toast (e.g. the import summary); null once it is gone. */
  notice: string | null;
  /** The last `internal` error of any command: shown as a toast with "Open log folder". */
  internalError: AppError | null;
  /**
   * Shows `notice` on the toast stack (`info` by default: hides after 4 s; `error` stays until
   * dismissed). The same text again restarts its timer; `null` hides the current notice.
   */
  setNotice(notice: string | null, kind?: 'info' | 'success' | 'error'): void;
  showPrompt(prompt: PendingPrompt): void;
  closePrompt(): void;
  requestDialog(request: UiRequest): void;
  closeRequest(): void;
  reportInternal(error: AppError): void;
  dismissInternal(): void;
}

const noticeKey = (text: string) => `notice:${text}`;

export const useDialogs = create<DialogsState>()((set, get) => ({
  objectActionRunning: false,
  tableDesignRunning: false,
  ddlEditRunning: false,
  question: null,
  ask: (question) => set({ question }),
  answer: () => set({ question: null }),
  prompt: null,
  request: null,
  notice: null,
  internalError: null,
  setNotice: (notice, kind = 'info') => {
    const current = get().notice;
    if (notice === null) {
      set({ notice: null });
      if (current !== null) useToasts.getState().dismissKey(noticeKey(current));
      return;
    }
    set({ notice });
    useToasts.getState().show({
      kind,
      title: notice,
      key: noticeKey(notice),
      // Timed out or dismissed: the notice is gone (unless a newer one took its place).
      onDismiss: () => {
        if (get().notice === notice) set({ notice: null });
      },
    });
  },
  showPrompt: (prompt) => set({ prompt }),
  closePrompt: () => set({ prompt: null }),
  requestDialog: (request) => set({ request }),
  closeRequest: () => set({ request: null }),
  // The internal-error toast shares the stack's room with the store's toasts.
  reportInternal: (internalError) => {
    set({ internalError });
    useToasts.getState().reserve(internalError ? 1 : 0);
  },
  dismissInternal: () => {
    set({ internalError: null });
    useToasts.getState().reserve(0);
  },
}));
