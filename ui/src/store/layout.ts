import { create } from 'zustand';

export const SIDEBAR_DEFAULT = 280;
export const SIDEBAR_MIN = 180;
export const SIDEBAR_MAX = 600;
export const EDITOR_RATIO_DEFAULT = 0.6;
/** Smallest sensible editor share; the pixel minimums are applied against the real height (`editorPixels`). */
export const EDITOR_RATIO_MIN = 0.05;
export const EDITOR_RATIO_MAX = 0.95;
/** The editor and the result panel never shrink below these (px). */
export const EDITOR_MIN_PX = 80;
export const RESULT_MIN_PX = 120;

const STORAGE_KEY = 'dnt.layout';

const clamp = (n: number, min: number, max: number) => Math.min(max, Math.max(min, n));

const clampSidebar = (n: number) => (Number.isFinite(n) ? Math.round(clamp(n, SIDEBAR_MIN, SIDEBAR_MAX)) : SIDEBAR_DEFAULT);
const clampRatio = (n: number) => (Number.isFinite(n) ? clamp(n, EDITOR_RATIO_MIN, EDITOR_RATIO_MAX) : EDITOR_RATIO_DEFAULT);

/**
 * The editor height (px) for `ratio` in a body of `total` px: the ratio's share, but at least
 * `EDITOR_MIN_PX` and leaving at least `RESULT_MIN_PX` to the result panel (the editor minimum
 * wins when the body is too small for both).
 */
export function editorPixels(ratio: number, total: number): number {
  const max = Math.max(EDITOR_MIN_PX, total - RESULT_MIN_PX);
  return Math.round(clamp(ratio * total, EDITOR_MIN_PX, max));
}

interface Saved {
  sidebarWidth: number;
  sidebarVisible: boolean;
  editorRatio: number;
}

const defaults = (): Saved => ({ sidebarWidth: SIDEBAR_DEFAULT, sidebarVisible: true, editorRatio: EDITOR_RATIO_DEFAULT });

function load(): Saved {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return defaults();
    const v = JSON.parse(raw) as Partial<Record<keyof Saved, unknown>> | null;
    return {
      sidebarWidth: typeof v?.sidebarWidth === 'number' ? clampSidebar(v.sidebarWidth) : SIDEBAR_DEFAULT,
      sidebarVisible: typeof v?.sidebarVisible === 'boolean' ? v.sidebarVisible : true,
      editorRatio: typeof v?.editorRatio === 'number' ? clampRatio(v.editorRatio) : EDITOR_RATIO_DEFAULT,
    };
  } catch {
    return defaults();
  }
}

function save(s: Saved) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ sidebarWidth: s.sidebarWidth, sidebarVisible: s.sidebarVisible, editorRatio: s.editorRatio }));
  } catch {
    // Storage can be unavailable; the in-memory layout still applies.
  }
}

interface LayoutState extends Saved {
  setSidebarWidth(width: number): void;
  toggleSidebar(): void;
  setEditorRatio(ratio: number): void;
  /** Default sidebar width and editor / result split (the sidebar is shown again). */
  reset(): void;
}

export const useLayout = create<LayoutState>()((set, get) => {
  /** Applies a change and persists the result. */
  const apply = (patch: Partial<Saved>) => {
    set(patch);
    const { sidebarWidth, sidebarVisible, editorRatio } = get();
    save({ sidebarWidth, sidebarVisible, editorRatio });
  };
  return {
    ...load(),
    setSidebarWidth: (width) => apply({ sidebarWidth: clampSidebar(width) }),
    toggleSidebar: () => apply({ sidebarVisible: !get().sidebarVisible }),
    setEditorRatio: (ratio) => apply({ editorRatio: clampRatio(ratio) }),
    reset: () => apply(defaults()),
  };
});

/** Re-reads the stored layout (tests: after changing localStorage). */
export function reloadLayout() {
  useLayout.setState(load());
}
