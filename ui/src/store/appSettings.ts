import { create } from 'zustand';
import { api } from '../api';

/** Default page size of the table data view (also the backend's default). */
export const DEFAULT_TABLE_PAGE_SIZE = 100;
export const MIN_TABLE_PAGE_SIZE = 10;
export const MAX_TABLE_PAGE_SIZE = 10_000;

export const isValidPageSize = (n: number) =>
  Number.isInteger(n) && n >= MIN_TABLE_PAGE_SIZE && n <= MAX_TABLE_PAGE_SIZE;

/** Minutes an open transaction may idle before the status bar warns (backend default 5, 1..120). */
export const DEFAULT_IDLE_TX_WARN_MINUTES = 5;
export const MIN_IDLE_TX_WARN_MINUTES = 1;
export const MAX_IDLE_TX_WARN_MINUTES = 120;

export const isValidIdleTxMinutes = (n: number) =>
  Number.isInteger(n) && n >= MIN_IDLE_TX_WARN_MINUTES && n <= MAX_IDLE_TX_WARN_MINUTES;

/** Background jobs that may run at once on one connection (P5 R2: default 2, 1..8). */
export const DEFAULT_JOBS_PER_CONNECTION = 2;
export const MIN_JOBS_PER_CONNECTION = 1;
export const MAX_JOBS_PER_CONNECTION = 8;

export const isValidJobsPerConnection = (n: number) =>
  Number.isInteger(n) && n >= MIN_JOBS_PER_CONNECTION && n <= MAX_JOBS_PER_CONNECTION;

interface AppSettingsState {
  /** The stored per-connection job limit; the default until loaded. */
  jobsPerConnection: number;
  /** Reads the limit once (the default when it cannot be read). */
  loadJobsPerConnection(): Promise<number>;
  /** Stores a new limit (rejects with the backend's error). */
  setJobsPerConnection(n: number): Promise<void>;
  /** The folder searched first for the external dump tools (P5 R25); `null`: `PATH` only. */
  externalToolsDir: string | null;
  /** Reads the folder once (`null` when it cannot be read). */
  loadExternalToolsDir(): Promise<string | null>;
  /**
   * Lets the user pick the folder in the backend's folder dialog, which stores it; resolves
   * to the folder, or `null` when cancelled (rejects with the backend's error). The UI never
   * names the folder itself: the app runs programs from it.
   */
  pickExternalToolsDir(): Promise<string | null>;
  /** Clears the folder: `PATH` only (rejects with the backend's error). */
  clearExternalToolsDir(): Promise<void>;
  /** The stored idle-transaction warning threshold; the default until loaded. */
  idleTxWarnMinutes: number;
  /** Reads the threshold once (the default when it cannot be read). */
  loadIdleTxWarnMinutes(): Promise<number>;
  /** Stores a new threshold (rejects with the backend's error). */
  setIdleTxWarnMinutes(minutes: number): Promise<void>;
  /** The stored table data page size; `null` until loaded. */
  tablePageSize: number | null;
  /** Reads the stored page size once (the default when it cannot be read). */
  loadTablePageSize(): Promise<number>;
  /** Stores a new default page size (rejects with the backend's error). */
  setTablePageSize(size: number): Promise<void>;
  reset(): void;
}

let loading: Promise<number> | null = null;
let idleLoading: Promise<number> | null = null;
let idleLoaded = false;
let jobsLoading: Promise<number> | null = null;
let jobsLoaded = false;
let toolsLoading: Promise<string | null> | null = null;
let toolsLoaded = false;

/** App preferences stored in the backend's `app_settings` (app.db). */
export const useAppSettings = create<AppSettingsState>()((set, get) => ({
  jobsPerConnection: DEFAULT_JOBS_PER_CONNECTION,
  loadJobsPerConnection() {
    if (jobsLoaded) return Promise.resolve(get().jobsPerConnection);
    jobsLoading ??= api
      .getJobsPerConnection()
      .then((n) => (isValidJobsPerConnection(n) ? n : DEFAULT_JOBS_PER_CONNECTION))
      .catch(() => DEFAULT_JOBS_PER_CONNECTION)
      .then((n) => {
        // A save that landed meanwhile wins.
        if (!jobsLoaded) set({ jobsPerConnection: n });
        jobsLoaded = true;
        jobsLoading = null;
        return get().jobsPerConnection;
      });
    return jobsLoading;
  },
  async setJobsPerConnection(n) {
    await api.setJobsPerConnection(n);
    jobsLoaded = true;
    set({ jobsPerConnection: n });
  },
  externalToolsDir: null,
  loadExternalToolsDir() {
    if (toolsLoaded) return Promise.resolve(get().externalToolsDir);
    toolsLoading ??= api
      .getExternalToolsDir()
      .catch(() => null)
      .then((dir) => {
        if (!toolsLoaded) set({ externalToolsDir: dir });
        toolsLoaded = true;
        toolsLoading = null;
        return get().externalToolsDir;
      });
    return toolsLoading;
  },
  async pickExternalToolsDir() {
    const dir = await api.toolsPickDir();
    if (dir !== null) {
      toolsLoaded = true;
      set({ externalToolsDir: dir });
    }
    return dir;
  },
  async clearExternalToolsDir() {
    await api.clearExternalToolsDir();
    toolsLoaded = true;
    set({ externalToolsDir: null });
  },
  tablePageSize: null,
  idleTxWarnMinutes: DEFAULT_IDLE_TX_WARN_MINUTES,
  loadIdleTxWarnMinutes() {
    if (idleLoaded) return Promise.resolve(get().idleTxWarnMinutes);
    idleLoading ??= api
      .getIdleTxWarnMinutes()
      .then((n) => (isValidIdleTxMinutes(n) ? n : DEFAULT_IDLE_TX_WARN_MINUTES))
      .catch(() => DEFAULT_IDLE_TX_WARN_MINUTES)
      .then((n) => {
        // A save that landed meanwhile wins.
        if (!idleLoaded) set({ idleTxWarnMinutes: n });
        idleLoaded = true;
        idleLoading = null;
        return get().idleTxWarnMinutes;
      });
    return idleLoading;
  },
  async setIdleTxWarnMinutes(minutes) {
    await api.setIdleTxWarnMinutes(minutes);
    idleLoaded = true;
    set({ idleTxWarnMinutes: minutes });
  },
  loadTablePageSize() {
    const known = get().tablePageSize;
    if (known !== null) return Promise.resolve(known);
    loading ??= api
      .getTablePageSize()
      .then((n) => (isValidPageSize(n) ? n : DEFAULT_TABLE_PAGE_SIZE))
      .catch(() => DEFAULT_TABLE_PAGE_SIZE)
      .then((n) => {
        // A save that landed meanwhile wins.
        if (get().tablePageSize === null) set({ tablePageSize: n });
        loading = null;
        return get().tablePageSize ?? n;
      });
    return loading;
  },
  async setTablePageSize(size) {
    await api.setTablePageSize(size);
    set({ tablePageSize: size });
  },
  reset() {
    loading = null;
    idleLoading = null;
    idleLoaded = false;
    jobsLoading = null;
    jobsLoaded = false;
    toolsLoading = null;
    toolsLoaded = false;
    set({
      tablePageSize: null,
      idleTxWarnMinutes: DEFAULT_IDLE_TX_WARN_MINUTES,
      jobsPerConnection: DEFAULT_JOBS_PER_CONNECTION,
      externalToolsDir: null,
    });
  },
}));
