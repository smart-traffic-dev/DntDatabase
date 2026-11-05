import { create } from 'zustand';
import { api, toAppError } from '../api';
import type { ApplyReport, KeyEdit, KeyName, KeySummary, KeyType, KeyValue, KeyValueView, ValueRange } from '../bindings';
import { sameKey } from '../tabs/keys/keyEdits';
import { useConnections } from './connections';
import { STALE_NOTICE, UNKNOWN_OUTCOME, type PendingApply } from './dataEdits';
import { keysTabId, keysTabTitle, useTabs, type KeysTabRef } from './tabs';

/** Keys asked for per scan. */
export const SCAN_COUNT = 200;
/** Items of a hash / list / set / zset / stream asked for per page. */
export const VALUE_PAGE = 200;
/** The selected key expired or was deleted (spec §7.6). */
export const KEY_GONE = 'Key no longer exists';

const firstRange = (): ValueRange => ({ offset: 0, count: VALUE_PAGE });

/** The key browser of one tab (spec §7.6). */
export interface KeysTabState {
  connectionId: string;
  database: string;
  /** A Redis glob. */
  pattern: string;
  keyType: KeyType | null;
  keys: KeySummary[];
  /** Where the next scan continues; null with `done: false` before the first page. */
  cursor: string | null;
  done: boolean;
  scanning: boolean;
  /** Compared by `raw`, never by `display` (decision 8). */
  selected: KeyName | null;
  value: KeyValueView | null;
  /** The page of the value asked for last. */
  range: ValueRange;
  loadingValue: boolean;
  /** A scan of this tab has succeeded (a restored tab scans again when its connection opens). */
  scanned: boolean;
  /** A failed scan, value read or preview. */
  error: string | null;
  /** `Key no longer exists`, until another key is selected. */
  notice: string | null;
  /** The plan under review, and the edit it was made of. */
  pending: PendingApply | null;
  edit: KeyEdit | null;
  /** The type of the key `edit` creates (the toolbar's `New key…`), else null. */
  creates: KeyType | null;
}

interface KeysTabsStore {
  byTab: Record<string, KeysTabState>;
  /** Creates the tab's state and scans its first page (nothing when it already exists). */
  ensure(tab: Pick<KeysTabRef, 'id' | 'connectionId' | 'database'>): void;
  setPattern(tabId: string, pattern: string): void;
  setKeyType(tabId: string, keyType: KeyType | null): void;
  /** Scans again from the start (cursor `null`). */
  rescan(tabId: string): Promise<void>;
  /** The next page of keys. */
  loadMore(tabId: string): Promise<void>;
  select(tabId: string, key: KeyName): Promise<void>;
  /** Reads the selected key's value again from its first page. */
  refresh(tabId: string): Promise<void>;
  /** The next page of the selected key's value. */
  loadMoreValue(tabId: string): Promise<void>;
  /** Plans one edit (`key_edit_preview`) for review. `creates`: the type of the key it makes. */
  preview(tabId: string, edit: KeyEdit, creates?: KeyType | null): Promise<void>;
  /** Applies the plan under review (`data_apply`). */
  confirm(tabId: string): Promise<void>;
  cancel(tabId: string): void;
  /** Forgets a closed tab: reopening it starts again with a fresh scan. */
  drop(tabId: string): void;
  reset(): void;
}

/** How many items of a value are loaded, and how many there are. */
export function valueCounts(v: KeyValue): { loaded: number; total: number } | null {
  if ('Hash' in v) return { loaded: v.Hash.fields.length, total: v.Hash.total };
  if ('List' in v) return { loaded: v.List.items.length, total: v.List.total };
  if ('Set' in v) return { loaded: v.Set.members.length, total: v.Set.total };
  if ('ZSet' in v) return { loaded: v.ZSet.members.length, total: v.ZSet.total };
  if ('Stream' in v) return { loaded: v.Stream.entries.length, total: v.Stream.total };
  return null;
}

/** `next` (a later page of the same value) appended to `prev`. */
function appendPage(prev: KeyValue, next: KeyValue): KeyValue {
  if ('Hash' in prev && 'Hash' in next) return { Hash: { ...next.Hash, fields: [...prev.Hash.fields, ...next.Hash.fields] } };
  if ('List' in prev && 'List' in next) return { List: { ...next.List, items: [...prev.List.items, ...next.List.items] } };
  if ('Set' in prev && 'Set' in next) return { Set: { ...next.Set, members: [...prev.Set.members, ...next.Set.members] } };
  if ('ZSet' in prev && 'ZSet' in next) return { ZSet: { ...next.ZSet, members: [...prev.ZSet.members, ...next.ZSet.members] } };
  if ('Stream' in prev && 'Stream' in next) return { Stream: { ...next.Stream, entries: [...prev.Stream.entries, ...next.Stream.entries] } };
  return next;
}

function applyFailure(report: ApplyReport): string {
  const failed = report.statements.find((s) => s.error)?.error ?? report.error;
  switch (report.outcome.outcome) {
    case 'committed':
      return '';
    case 'rolled_back':
      return failed?.message ?? 'The change was rolled back; nothing was applied.';
    case 'partially_applied':
      return failed?.message ?? 'The change stopped before it was applied.';
    default:
      return UNKNOWN_OUTCOME;
  }
}

/**
 * Sequence numbers of the latest scan / value read per tab: an older answer is dropped. One
 * counter for all, so a tab closed and reopened never reuses the number of a read still running.
 */
const scanSeq = new Map<string, number>();
const valueSeq = new Map<string, number>();
let lastSeq = 0;
const bump = (m: Map<string, number>, id: string) => {
  const n = ++lastSeq;
  m.set(id, n);
  return n;
};

export const useKeysTabs = create<KeysTabsStore>()((set, get) => {
  const patch = (tabId: string, change: Partial<KeysTabState>) =>
    set((s) => {
      const current = s.byTab[tabId];
      return current ? { byTab: { ...s.byTab, [tabId]: { ...current, ...change } } } : s;
    });

  async function scan(tabId: string, fresh: boolean) {
    const state = get().byTab[tabId];
    if (!state || (!fresh && state.done)) return;
    const seq = bump(scanSeq, tabId);
    const cursor = fresh ? null : state.cursor;
    patch(tabId, fresh ? { scanning: true, keys: [], cursor: null, done: false, error: null } : { scanning: true, error: null });
    try {
      const page = await api.keysScan(state.connectionId, state.database, {
        cursor,
        pattern: state.pattern.trim() || null,
        key_type: state.keyType,
        count: SCAN_COUNT,
      });
      if (scanSeq.get(tabId) !== seq) return;
      const now = get().byTab[tabId];
      if (!now) return;
      // A key listed on an earlier page (keys can move between pages of a scan) is kept once.
      const seen = new Set(now.keys.map((k) => k.name.raw));
      patch(tabId, {
        keys: [...now.keys, ...page.keys.filter((k) => !seen.has(k.name.raw))],
        cursor: page.next_cursor,
        done: page.next_cursor === null,
        scanning: false,
        scanned: true,
      });
    } catch (e) {
      if (scanSeq.get(tabId) === seq) patch(tabId, { scanning: false, error: toAppError(e).message });
    }
  }

  async function readValue(tabId: string, more: boolean) {
    const state = get().byTab[tabId];
    const key = state?.selected;
    if (!state || !key) return;
    const counts = more && state.value ? valueCounts(state.value.value) : null;
    const range: ValueRange = counts ? { offset: counts.loaded, count: VALUE_PAGE } : firstRange();
    const seq = bump(valueSeq, tabId);
    patch(tabId, { loadingValue: true, range, error: null, ...(more ? {} : { notice: null }) });
    try {
      const view = await api.keyValue(state.connectionId, state.database, key, range);
      if (valueSeq.get(tabId) !== seq) return;
      const now = get().byTab[tabId];
      if (!now || !sameKey(now.selected, key)) return;
      const value = more && now.value ? { ...view, value: appendPage(now.value.value, view.value) } : view;
      // The list shows the TTL the server has now.
      const keys = now.keys.map((k) => (sameKey(k.name, key) && k.ttl_ms !== view.ttl_ms ? { ...k, ttl_ms: view.ttl_ms } : k));
      patch(tabId, { value, keys, loadingValue: false });
    } catch (e) {
      if (valueSeq.get(tabId) !== seq) return;
      const err = toAppError(e);
      if (err.kind === 'key_not_found') {
        const now = get().byTab[tabId];
        patch(tabId, {
          keys: (now?.keys ?? []).filter((k) => !sameKey(k.name, key)),
          selected: null,
          value: null,
          loadingValue: false,
          notice: KEY_GONE,
        });
      } else {
        patch(tabId, { loadingValue: false, error: err.message });
      }
    }
  }

  /** What a committed edit changes in the list and the selection. */
  function applied(tabId: string, edit: KeyEdit, creates: KeyType | null) {
    const state = get().byTab[tabId];
    if (!state) return;
    switch (edit.op) {
      case 'delete':
        patch(tabId, {
          keys: state.keys.filter((k) => !sameKey(k.name, edit.key)),
          ...(sameKey(state.selected, edit.key) ? { selected: null, value: null } : {}),
        });
        return;
      case 'rename':
        patch(tabId, {
          keys: state.keys.filter((k) => !sameKey(k.name, edit.to)).map((k) => (sameKey(k.name, edit.key) ? { ...k, name: edit.to } : k)),
          selected: edit.to,
        });
        void readValue(tabId, false);
        return;
      default:
        if (creates && !state.keys.some((k) => sameKey(k.name, edit.key))) {
          patch(tabId, { keys: [...state.keys, { name: edit.key, key_type: creates, ttl_ms: null }] });
        }
        if (!sameKey(state.selected, edit.key)) patch(tabId, { selected: edit.key, value: null });
        void readValue(tabId, false);
    }
  }

  async function plan(tabId: string, edit: KeyEdit, creates: KeyType | null, notice: string | null) {
    const state = get().byTab[tabId];
    if (!state) return;
    try {
      const preview = await api.keyEditPreview(state.connectionId, state.database, edit);
      patch(tabId, {
        pending: {
          preview,
          overwrite: false,
          keys: edit.op === 'delete' ? [{ kind: 'delete', row: edit.key.raw }] : [],
          only: null,
          notice,
          busy: false,
          error: null,
        },
        edit,
        creates,
        error: null,
      });
    } catch (e) {
      const message = toAppError(e).message;
      if (get().byTab[tabId]?.pending) patch(tabId, { pending: { ...get().byTab[tabId]!.pending!, busy: false, error: message } });
      else patch(tabId, { error: message });
    }
  }

  return {
    byTab: {},
    ensure(tab) {
      if (get().byTab[tab.id]) return;
      set((s) => ({
        byTab: {
          ...s.byTab,
          [tab.id]: {
            connectionId: tab.connectionId,
            database: tab.database,
            pattern: '*',
            keyType: null,
            keys: [],
            cursor: null,
            done: false,
            scanning: false,
            scanned: false,
            selected: null,
            value: null,
            range: firstRange(),
            loadingValue: false,
            error: null,
            notice: null,
            pending: null,
            edit: null,
            creates: null,
          },
        },
      }));
      void scan(tab.id, true);
    },
    setPattern: (tabId, pattern) => patch(tabId, { pattern }),
    setKeyType(tabId, keyType) {
      patch(tabId, { keyType });
      void scan(tabId, true);
    },
    rescan: (tabId) => scan(tabId, true),
    loadMore: (tabId) => scan(tabId, false),
    async select(tabId, key) {
      const state = get().byTab[tabId];
      if (!state) return;
      patch(tabId, { selected: key, value: null, notice: null, range: firstRange() });
      await readValue(tabId, false);
    },
    refresh: (tabId) => readValue(tabId, false),
    loadMoreValue: (tabId) => readValue(tabId, true),
    preview: (tabId, edit, creates = null) => plan(tabId, edit, creates, null),
    async confirm(tabId) {
      const state = get().byTab[tabId];
      const pending = state?.pending;
      const edit = state?.edit;
      if (!state || !pending || !edit || pending.busy) return;
      patch(tabId, { pending: { ...pending, busy: true, error: null } });
      try {
        // The dialog is the confirmation, also of a Production plan.
        const report = await api.dataApply(pending.preview.plan_id, true);
        if (report.outcome.outcome === 'committed') {
          patch(tabId, { pending: null, edit: null, creates: null });
          applied(tabId, edit, state.creates);
        } else {
          patch(tabId, { pending: { ...pending, busy: false, error: applyFailure(report) } });
        }
      } catch (e) {
        const err = toAppError(e);
        // Nothing was sent: show the new commands for another review.
        if (err.kind === 'plan_stale') await plan(tabId, edit, state.creates, STALE_NOTICE);
        else patch(tabId, { pending: { ...pending, busy: false, error: err.message } });
      }
    },
    cancel: (tabId) => patch(tabId, { pending: null, edit: null, creates: null }),
    drop(tabId) {
      scanSeq.delete(tabId);
      valueSeq.delete(tabId);
      set((s) => {
        if (!(tabId in s.byTab)) return s;
        const byTab = { ...s.byTab };
        delete byTab[tabId];
        return { byTab };
      });
    },
    reset() {
      scanSeq.clear();
      valueSeq.clear();
      set({ byTab: {} });
    },
  };
});

/** Where the open keys tabs are remembered across restarts. */
export const KEYS_TABS_KEY = 'dnt.keys.tabs';

interface SavedKeysTab {
  connectionId: string;
  database: string;
  pinned?: boolean;
}

/** Saves the open keys tabs whenever the tabs change; returns the unsubscribe. */
export function persistKeysTabs(): () => void {
  return useTabs.subscribe((s, prev) => {
    if (s.tabs === prev.tabs) return;
    const saved: SavedKeysTab[] = s.tabs.flatMap((t) => (t.kind === 'keys' ? [{ connectionId: t.connectionId, database: t.database, pinned: !!t.pinned }] : []));
    try {
      localStorage.setItem(KEYS_TABS_KEY, JSON.stringify(saved));
    } catch {
      // Storage unavailable: the keys tabs just do not come back.
    }
  });
}

/**
 * Keeps the key browsers in step with the tabs and connections; returns the unsubscribe. A closed
 * keys tab loses its state (switching tabs unmounts it too, so this is not an unmount cleanup), and
 * a tab whose scan has not succeeded yet (restored before its connection was open) scans again
 * once its connection's server info arrives.
 */
export function syncKeysTabs(): () => void {
  const stopTabs = useTabs.subscribe((s, prev) => {
    if (s.tabs === prev.tabs) return;
    const open = new Set(s.tabs.map((t) => t.id));
    for (const t of prev.tabs) if (t.kind === 'keys' && !open.has(t.id)) useKeysTabs.getState().drop(t.id);
  });
  const stopConnections = useConnections.subscribe((s, prev) => {
    if (s.info === prev.info) return;
    const keys = useKeysTabs.getState();
    for (const t of useTabs.getState().tabs) {
      if (t.kind !== 'keys' || !s.info.has(t.connectionId) || prev.info.has(t.connectionId)) continue;
      const state = keys.byTab[t.id];
      if (state && !state.scanned) void keys.rescan(t.id);
    }
  });
  return () => {
    stopTabs();
    stopConnections();
  };
}

/**
 * Reopens the keys tabs saved by `persistKeysTabs` (before `restoreArrangement`, which puts them
 * back in their groups). A tab of a connection that is gone is dropped; `connectionsKnown: false`
 * (the stored connections failed to load) keeps every one.
 */
export function restoreKeysTabs({ connectionsKnown = true }: { connectionsKnown?: boolean } = {}): void {
  let saved: unknown;
  try {
    saved = JSON.parse(localStorage.getItem(KEYS_TABS_KEY) ?? '[]');
  } catch {
    return;
  }
  if (!Array.isArray(saved)) return;
  const known = useConnections.getState().summaries;
  const tabs = useTabs.getState();
  const hadActive = tabs.activeId;
  for (const entry of saved as SavedKeysTab[]) {
    if (typeof entry?.connectionId !== 'string' || typeof entry.database !== 'string') continue;
    if (connectionsKnown && !known.has(entry.connectionId)) continue;
    const id = keysTabId(entry.connectionId, entry.database);
    tabs.open({ kind: 'keys', id, connectionId: entry.connectionId, database: entry.database, title: keysTabTitle(entry.connectionId, entry.database) });
    if (entry.pinned) tabs.setPinned(id, true);
  }
  if (hadActive) useTabs.getState().activate(hadActive);
}
