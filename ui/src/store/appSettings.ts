import { create } from 'zustand';
import { api } from '../api';

/** Default page size of the table data view (also the backend's default). */
export const DEFAULT_TABLE_PAGE_SIZE = 100;
export const MIN_TABLE_PAGE_SIZE = 10;
export const MAX_TABLE_PAGE_SIZE = 10_000;

export const isValidPageSize = (n: number) =>
  Number.isInteger(n) && n >= MIN_TABLE_PAGE_SIZE && n <= MAX_TABLE_PAGE_SIZE;

interface AppSettingsState {
  /** The stored table data page size; `null` until loaded. */
  tablePageSize: number | null;
  /** Reads the stored page size once (the default when it cannot be read). */
  loadTablePageSize(): Promise<number>;
  /** Stores a new default page size (rejects with the backend's error). */
  setTablePageSize(size: number): Promise<void>;
  reset(): void;
}

let loading: Promise<number> | null = null;

/** App preferences stored in the backend's `app_settings` (app.db). */
export const useAppSettings = create<AppSettingsState>()((set, get) => ({
  tablePageSize: null,
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
    set({ tablePageSize: null });
  },
}));
