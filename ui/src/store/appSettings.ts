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

interface AppSettingsState {
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

/** App preferences stored in the backend's `app_settings` (app.db). */
export const useAppSettings = create<AppSettingsState>()((set, get) => ({
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
    set({ tablePageSize: null, idleTxWarnMinutes: DEFAULT_IDLE_TX_WARN_MINUTES });
  },
}));
