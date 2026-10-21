import { create } from 'zustand';
import { useTabs } from './tabs';

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

/** The right sidebar (History panel) width range, px. */
export const RIGHT_DEFAULT = 340;
export const RIGHT_MIN = 260;
export const RIGHT_MAX = 560;

const STORAGE_KEY = 'dnt.layout';

const clamp = (n: number, min: number, max: number) => Math.min(max, Math.max(min, n));

const clampSidebar = (n: number) => (Number.isFinite(n) ? Math.round(clamp(n, SIDEBAR_MIN, SIDEBAR_MAX)) : SIDEBAR_DEFAULT);
const clampRatio = (n: number) => (Number.isFinite(n) ? clamp(n, EDITOR_RATIO_MIN, EDITOR_RATIO_MAX) : EDITOR_RATIO_DEFAULT);
const clampRight = (n: number) => (Number.isFinite(n) ? Math.round(clamp(n, RIGHT_MIN, RIGHT_MAX)) : RIGHT_DEFAULT);

/** What the results area of one editor group is doing. */
export interface GroupResults {
  visible: boolean;
  /** Editor share of the query tab body. */
  ratio: number;
  /** The results fill the group and the editor folds away. */
  maximized: boolean;
}

/** The results settings of an editor group: its own, else the global values (a new group's start). */
export function resultsOf(
  s: Pick<LayoutState, 'groupResults' | 'resultsVisible' | 'editorRatio'>,
  groupId: string,
): GroupResults {
  const own = s.groupResults[groupId];
  return { visible: own?.visible ?? s.resultsVisible, ratio: own?.ratio ?? s.editorRatio, maximized: own?.maximized ?? false };
}

/**
 * The editor height (px) for `ratio` in a body of `total` px: the ratio's share, but at least
 * `EDITOR_MIN_PX` and leaving at least `RESULT_MIN_PX` to the result panel (the editor minimum
 * wins when the body is too small for both).
 */
export function editorPixels(ratio: number, total: number): number {
  const max = Math.max(EDITOR_MIN_PX, total - RESULT_MIN_PX);
  return Math.round(clamp(ratio * total, EDITOR_MIN_PX, max));
}

/** Scale of the code font (editor, result grid, messages): Ctrl+= / Ctrl+- / Ctrl+0. */
export const FONT_SCALE_MIN = 0.8;
export const FONT_SCALE_MAX = 1.6;
export const FONT_SCALE_STEP = 0.1;
export const FONT_SCALE_DEFAULT = 1;

/** Clamped to the range and rounded to one decimal (0.1 steps add up without float noise). */
export const clampFontScale = (n: number) =>
  Number.isFinite(n) ? Math.round(clamp(n, FONT_SCALE_MIN, FONT_SCALE_MAX) * 10) / 10 : FONT_SCALE_DEFAULT;

interface Saved {
  sidebarWidth: number;
  sidebarVisible: boolean;
  editorRatio: number;
  /** The result panel of query tabs is shown (Ctrl+J). */
  resultsVisible: boolean;
  fontScale: number;
  /** The right sidebar (activity bar + History panel) is open. */
  rightVisible: boolean;
  rightWidth: number;
}

const defaults = (): Saved => ({
  rightVisible: false,
  rightWidth: RIGHT_DEFAULT,
  sidebarWidth: SIDEBAR_DEFAULT,
  sidebarVisible: true,
  editorRatio: EDITOR_RATIO_DEFAULT,
  resultsVisible: true,
  fontScale: FONT_SCALE_DEFAULT,
});

function load(): Saved {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return defaults();
    const v = JSON.parse(raw) as Partial<Record<keyof Saved, unknown>> | null;
    return {
      sidebarWidth: typeof v?.sidebarWidth === 'number' ? clampSidebar(v.sidebarWidth) : SIDEBAR_DEFAULT,
      sidebarVisible: typeof v?.sidebarVisible === 'boolean' ? v.sidebarVisible : true,
      editorRatio: typeof v?.editorRatio === 'number' ? clampRatio(v.editorRatio) : EDITOR_RATIO_DEFAULT,
      resultsVisible: typeof v?.resultsVisible === 'boolean' ? v.resultsVisible : true,
      fontScale: typeof v?.fontScale === 'number' ? clampFontScale(v.fontScale) : FONT_SCALE_DEFAULT,
      rightVisible: typeof v?.rightVisible === 'boolean' ? v.rightVisible : false,
      rightWidth: typeof v?.rightWidth === 'number' ? clampRight(v.rightWidth) : RIGHT_DEFAULT,
    };
  } catch {
    return defaults();
  }
}

function save(s: Saved) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(s));
  } catch {
    // Storage can be unavailable; the in-memory layout still applies.
  }
}

interface LayoutState extends Saved {
  /** Per editor group results settings (memory only; a group's id is not stable across restarts). */
  groupResults: Record<string, Partial<GroupResults>>;
  setSidebarWidth(width: number): void;
  toggleSidebar(): void;
  setRightWidth(width: number): void;
  toggleRight(): void;
  setRightVisible(visible: boolean): void;
  setEditorRatio(ratio: number): void;
  toggleResults(): void;
  /** Shows / hides the results of one group (the global value follows, so a new group starts the same). */
  toggleGroupResults(groupId: string): void;
  /** The group's editor / results split (the global value follows). */
  setGroupRatio(groupId: string, ratio: number): void;
  /** Maximizes the group's results (showing them) or restores the split. */
  toggleGroupMaximize(groupId: string): void;
  /** Sets the code font scale (clamped to 0.8 to 1.6). */
  setFontScale(scale: number): void;
  /** One step bigger / smaller (`step` of `FONT_SCALE_STEP`), clamped. */
  zoomFont(steps: number): void;
  /** Default sidebar width and editor / result split (the sidebar and results are shown again; the font scale stays). */
  reset(): void;
}

export const useLayout = create<LayoutState>()((set, get) => {
  /** Applies a change and persists the result. */
  const apply = (patch: Partial<Saved> & { groupResults?: Record<string, Partial<GroupResults>> }) => {
    set(patch);
    const { sidebarWidth, sidebarVisible, editorRatio, resultsVisible, fontScale, rightVisible, rightWidth } = get();
    save({ sidebarWidth, sidebarVisible, editorRatio, resultsVisible, fontScale, rightVisible, rightWidth });
  };
  /**
   * A change to one group. Every open group (and this one) first keeps the values it shows now,
   * so the global value that follows the change, and starts new groups off, leaves the others alone.
   */
  const patchGroup = (groupId: string, patch: Partial<GroupResults>) => {
    const groupResults = { ...get().groupResults };
    for (const id of [...useTabs.getState().groups.map((g) => g.id), groupId]) groupResults[id] ??= resultsOf(get(), id);
    groupResults[groupId] = { ...groupResults[groupId], ...patch };
    return { groupResults };
  };
  return {
    ...load(),
    groupResults: {},
    setSidebarWidth: (width) => apply({ sidebarWidth: clampSidebar(width) }),
    toggleSidebar: () => apply({ sidebarVisible: !get().sidebarVisible }),
    setRightWidth: (width) => apply({ rightWidth: clampRight(width) }),
    toggleRight: () => apply({ rightVisible: !get().rightVisible }),
    setRightVisible: (visible) => apply({ rightVisible: visible }),
    setEditorRatio: (ratio) => apply({ editorRatio: clampRatio(ratio) }),
    toggleResults: () => apply({ resultsVisible: !get().resultsVisible }),
    toggleGroupResults: (groupId) => {
      const visible = !resultsOf(get(), groupId).visible;
      // Hiding the results also ends a maximize (nothing would be left on screen).
      apply({ resultsVisible: visible, ...patchGroup(groupId, { visible, ...(visible ? {} : { maximized: false }) }) });
    },
    setGroupRatio: (groupId, ratio) => {
      const r = clampRatio(ratio);
      apply({ editorRatio: r, ...patchGroup(groupId, { ratio: r }) });
    },
    toggleGroupMaximize: (groupId) => {
      const maximized = !resultsOf(get(), groupId).maximized;
      apply({ ...patchGroup(groupId, maximized ? { maximized, visible: true } : { maximized }) });
    },
    setFontScale: (scale) => apply({ fontScale: clampFontScale(scale) }),
    zoomFont: (steps) => apply({ fontScale: clampFontScale(get().fontScale + steps * FONT_SCALE_STEP) }),
    reset: () => apply({ ...defaults(), fontScale: get().fontScale, groupResults: {} }),
  };
});

/** Re-reads the stored layout (tests: after changing localStorage). */
export function reloadLayout() {
  useLayout.setState(load());
}

/**
 * Mirrors the font scale into `--font-scale` on the root element: CSS mono surfaces (messages)
 * multiply their size by it, and the result grid (which watches the root's `style`) re-themes.
 */
function mirrorFontScale(scale: number) {
  try {
    document.documentElement.style.setProperty('--font-scale', String(scale));
  } catch {
    // No document (not a browser): nothing to mirror.
  }
}
mirrorFontScale(useLayout.getState().fontScale);
useLayout.subscribe((s, prev) => {
  if (s.fontScale !== prev.fontScale) mirrorFontScale(s.fontScale);
});
