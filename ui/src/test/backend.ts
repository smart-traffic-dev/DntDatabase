import { invoke } from '@tauri-apps/api/core';
import { vi } from 'vitest';
import { appError, Failure } from './fixtures/data';
import { useAppSettings } from '../store/appSettings';
import { useConnections } from '../store/connections';
import { useDataEdits } from '../store/dataEdits';
import { useTableData } from '../store/tableData';
import { useTableDesigns } from '../store/tableDesigns';
import { useDialogs } from '../store/dialogs';
import { useJobs } from '../store/jobs';
import { useToasts } from '../store/toasts';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';

// The backend contract fixtures live in `fixtures/data.ts` (no vitest there, so the
// browser E2E tests in `e2e-web` can load them too); re-exported for the unit tests.
export {
  appError,
  backendError,
  config,
  fail,
  Failure,
  ids,
  openResult,
  sampleTree,
  serverInfo,
} from './fixtures/data';
export type { BackendErrorName } from './fixtures/data';

type Handler = (args: Record<string, unknown>) => unknown;

/** Routes `invoke(cmd, args)` to `handlers[cmd]`; a `Failure` rejects with its `AppError`. */
export function mockBackend(handlers: Record<string, Handler>) {
  const mock = vi.mocked(invoke);
  mock.mockReset();
  mock.mockImplementation(async (cmd, args) => {
    const handler = handlers[cmd];
    if (!handler) throw appError('internal', `unmocked command ${cmd}`);
    try {
      return (await handler((args ?? {}) as Record<string, unknown>)) as never;
    } catch (e) {
      if (e instanceof Failure) throw e.error;
      throw e;
    }
  });
  return mock;
}

/** Names of the commands invoked so far, in order. */
export const calls = () => vi.mocked(invoke).mock.calls.map(([cmd]) => cmd);

export const callsOf = (cmd: string) =>
  vi.mocked(invoke).mock.calls.filter(([c]) => c === cmd).map(([, args]) => args as Record<string, unknown>);

export function resetStores() {
  useTree.getState().reset();
  useConnections.getState().reset();
  useTabs.getState().reset();
  useQueryDocs.getState().reset();
  useAppSettings.getState().reset();
  useTableData.getState().reset();
  useDataEdits.getState().reset();
  useTableDesigns.getState().reset();
  useDialogs.setState({ objectActionRunning: false, tableDesignRunning: false, prompt: null, request: null, notice: null, internalError: null, question: null });
  useToasts.getState().reset();
  useJobs.getState().reset();
}
