import { create } from 'zustand';

/** What the grid of a data tab has selected ("3 rows selected"), for the status bar. */
interface GridSelectionState {
  byTab: Record<string, string>;
}

/** Only the status bar reads this; the grid keeps its own selection. */
export const useGridSelection = create<GridSelectionState>()(() => ({ byTab: {} }));

/** Records (or with null, forgets) the selection summary of a tab's grid. */
export function reportGridSelection(tabId: string, summary: string | null): void {
  useGridSelection.setState((s) => {
    if ((s.byTab[tabId] ?? null) === summary) return s;
    const byTab = { ...s.byTab };
    if (summary === null) delete byTab[tabId];
    else byTab[tabId] = summary;
    return { byTab };
  });
}

export function resetGridSelection(): void {
  useGridSelection.setState({ byTab: {} });
}
