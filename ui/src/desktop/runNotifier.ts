import { useQueryRun, type TabRun } from '../store/queryRun';

/** A query that ran longer than this, finishing while the window is in the background, is announced. */
export const LONG_RUN_MS = 10_000;

/** The part of the Tauri window the notifier uses. */
export interface NotifyWindow {
  requestUserAttention(type: number | null): Promise<void>;
  setTitle(title: string): Promise<void>;
}

export interface NotifierOptions {
  win: NotifyWindow;
  /** `UserAttentionType.Informational`: the taskbar button flashes until the window is focused. */
  attention: number;
  /** The window title to go back to. */
  baseTitle: string;
  /** Title of a query tab (for "✓ Done · orders - DntDatabase"). */
  titleOf(tabId: string): string | undefined;
  /** Whether the window is in the foreground now. */
  hasFocus(): boolean;
  now(): number;
  /** Subscribes to the window regaining focus; returns the unsubscriber. */
  onFocus(listener: () => void): () => void;
  thresholdMs?: number;
}

const startedAt = (run: TabRun): number | undefined =>
  run.startedAt ?? (run.phase.kind === 'running' ? run.phase.startedAt : undefined);

/**
 * Long query notification: when a run that took more than 10 s finishes while the window is
 * not focused, flashes the taskbar and puts the outcome in the window title ("✓ Done · orders -
 * DntDatabase") until the window gets the focus back. No OS notification. Returns the uninstaller.
 */
export function installRunNotifier(opts: NotifierOptions): () => void {
  const threshold = opts.thresholdMs ?? LONG_RUN_MS;
  let announced = false;
  const swallow = (p: Promise<void>) => void p.catch(() => {});

  const restore = () => {
    if (!announced) return;
    announced = false;
    swallow(opts.win.setTitle(opts.baseTitle));
  };
  const offFocus = opts.onFocus(restore);

  const unsubscribe = useQueryRun.subscribe((state, prev) => {
    for (const [tabId, run] of state.runs) {
      const before = prev.runs.get(tabId);
      if (!before || before === run) continue;
      // A run just ended: it was active and now is idle.
      if (before.phase.kind === 'idle' || run.phase.kind !== 'idle') continue;
      const began = startedAt(before);
      if (began === undefined || opts.now() - began <= threshold) continue;
      if (opts.hasFocus()) continue;
      const failed = run.stop?.kind === 'error' || (run.lastRun?.errors ?? 0) > 0;
      const name = opts.titleOf(tabId);
      const title = `${failed ? '✗ Failed' : '✓ Done'}${name ? ` · ${name}` : ''} - ${opts.baseTitle}`;
      announced = true;
      swallow(opts.win.setTitle(title));
      swallow(opts.win.requestUserAttention(opts.attention));
    }
  });

  return () => {
    unsubscribe();
    offFocus();
  };
}
