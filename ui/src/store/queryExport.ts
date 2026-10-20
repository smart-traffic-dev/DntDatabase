// Query tabs exporting "All rows" of a result: the export runs the statement again on the
// tab's own session (backend `query_export`), so the tab is busy meanwhile. Run is disabled,
// Stop / Esc / closing the tab cancel it through `query_cancel(runId)`.
import { create } from 'zustand';

interface ExportingState {
  /** Tab id → the run id of its export. */
  runs: Map<string, string>;
}

export const useExporting = create<ExportingState>()(() => ({ runs: new Map() }));

/** The run id of the tab's export, or null when it exports nothing. */
export const exportRunOf = (tabId: string): string | null => useExporting.getState().runs.get(tabId) ?? null;

/** Marks the tab as exporting under `runId` (null: the export ended). */
export function setExporting(tabId: string, runId: string | null): void {
  useExporting.setState((s) => {
    if ((s.runs.get(tabId) ?? null) === runId) return s;
    const runs = new Map(s.runs);
    if (runId === null) runs.delete(tabId);
    else runs.set(tabId, runId);
    return { runs };
  });
}
