import { create } from 'zustand';

/** Where the caret of a query tab's editor is, for the status bar. */
export interface CursorPos {
  line: number;
  col: number;
  /** Characters in the selection (0 when none). */
  selected: number;
}

interface CursorState {
  byTab: Record<string, CursorPos>;
}

/** Only the status bar's small cursor widget reads this; the editor and toolbar never subscribe. */
export const useCursor = create<CursorState>()(() => ({ byTab: {} }));

const pending = new Map<string, CursorPos | null>();
let scheduled = false;

const raf: (cb: () => void) => void = (cb) =>
  typeof requestAnimationFrame === 'function' ? void requestAnimationFrame(cb) : void setTimeout(cb, 16);

function flush() {
  scheduled = false;
  if (pending.size === 0) return;
  const batch = [...pending];
  pending.clear();
  useCursor.setState((s) => {
    let byTab = s.byTab;
    for (const [id, pos] of batch) {
      const prev = byTab[id];
      if (pos === null) {
        if (!prev) continue;
        if (byTab === s.byTab) byTab = { ...byTab };
        delete byTab[id];
      } else if (!prev || prev.line !== pos.line || prev.col !== pos.col || prev.selected !== pos.selected) {
        if (byTab === s.byTab) byTab = { ...byTab };
        byTab[id] = pos;
      }
    }
    return byTab === s.byTab ? s : { byTab };
  });
}

/**
 * Records a caret move. Many moves in one frame (typing, holding an arrow key) collapse into a
 * single store update on the next animation frame; only the latest position per tab is kept.
 */
export function reportCursor(tabId: string, pos: CursorPos): void {
  pending.set(tabId, pos);
  if (!scheduled) {
    scheduled = true;
    raf(flush);
  }
}

/** Forgets a tab's caret (its editor was disposed). */
export function clearCursor(tabId: string): void {
  pending.set(tabId, null);
  if (!scheduled) {
    scheduled = true;
    raf(flush);
  }
}

/** Applies what is queued now (tests; the frame callback does the same). */
export function flushCursor(): void {
  flush();
}

export function resetCursor(): void {
  pending.clear();
  scheduled = false;
  useCursor.setState({ byTab: {} });
}
