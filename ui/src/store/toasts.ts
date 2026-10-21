import { create } from 'zustand';

// The toast stack (spec §6, board 06): bottom-right, at most three. Success and info hide on
// their own; errors stay until dismissed; a progress toast stays until its owner finishes it.
// A toast is never the only place a field or statement error is reported.

export type ToastKind = 'success' | 'info' | 'warning' | 'error' | 'progress';

export interface ToastAction {
  label: string;
  /** Runs, then the toast closes. */
  run(): void;
}

export interface Toast {
  id: number;
  kind: ToastKind;
  title: string;
  /** Second line (counts, the file, the reason). */
  message?: string;
  /** Extra text for "Copy details" (errors), after the title and message. */
  details?: string;
  /** Progress toasts: done fraction 0..1; `null` = indeterminate. */
  progress?: number | null;
  actions?: ToastAction[];
  /** A toast shown again with the same key replaces the old one and restarts its timer. */
  key?: string;
  /** Bumped on every replace or update: the auto-hide timer restarts. */
  seq: number;
  /** Called once when the toast goes (dismissed, timed out, pushed out). */
  onDismiss?(): void;
}

export type ToastInput = Omit<Toast, 'id' | 'seq'>;

export const MAX_TOASTS = 3;
/** Success / info hide after this long (paused while hovered or focused). */
export const AUTO_HIDE_MS = 4000;

export const autoHides = (kind: ToastKind) => kind === 'success' || kind === 'info';

interface ToastsState {
  toasts: Toast[];
  /** Shows a toast (or replaces the one with the same `key`); returns its id. */
  show(toast: ToastInput): number;
  update(id: number, patch: Partial<ToastInput>): void;
  dismiss(id: number): void;
  /** Dismisses the toast with this key, if one is shown. */
  dismissKey(key: string): void;
  /** Slots taken by toasts shown outside this store (the internal-error toast); trims the stack to fit. */
  reserve(slots: number): void;
  reset(): void;
}

let nextId = 1;
/** Slots held by the internal-error toast (see `reserve`). */
let reserved = 0;

export const useToasts = create<ToastsState>()((set, get) => {
  const drop = (gone: Toast[]) => {
    for (const t of gone) t.onDismiss?.();
  };
  /** Pushes toasts out until `next` fits the room left; `keep` (the newest) goes last. */
  const fit = (next: Toast[], keep?: Toast) => {
    const gone: Toast[] = [];
    while (next.length > Math.max(1, MAX_TOASTS - reserved)) {
      // Push out the oldest one that would hide anyway; an error or a running progress only if all are.
      const victim = next.find((t) => t !== keep && autoHides(t.kind)) ?? next.find((t) => t !== keep) ?? next[0]!;
      gone.push(victim);
      next = next.filter((t) => t !== victim);
    }
    return { next, gone };
  };
  return {
    toasts: [],
    show: (input) => {
      const { toasts } = get();
      const same = input.key ? toasts.find((t) => t.key === input.key) : undefined;
      if (same) {
        set({ toasts: toasts.map((t) => (t.id === same.id ? { ...t, ...input, id: t.id, seq: t.seq + 1 } : t)) });
        return same.id;
      }
      const toast: Toast = { ...input, id: nextId++, seq: 0 };
      const { next, gone } = fit([...toasts, toast], toast);
      set({ toasts: next });
      drop(gone);
      return toast.id;
    },
    update: (id, patch) =>
      set((s) => ({ toasts: s.toasts.map((t) => (t.id === id ? { ...t, ...patch, seq: t.seq + 1 } : t)) })),
    dismiss: (id) => {
      const gone = get().toasts.filter((t) => t.id === id);
      if (!gone.length) return;
      set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
      drop(gone);
    },
    dismissKey: (key) => {
      const hit = get().toasts.find((t) => t.key === key);
      if (hit) get().dismiss(hit.id);
    },
    reserve: (slots) => {
      reserved = slots;
      const { next, gone } = fit(get().toasts, get().toasts.at(-1));
      if (!gone.length) return;
      set({ toasts: next });
      drop(gone);
    },
    reset: () => set({ toasts: [] }),
  };
});

export const showToast = (toast: ToastInput) => useToasts.getState().show(toast);
