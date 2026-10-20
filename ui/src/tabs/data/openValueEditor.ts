import { create } from 'zustand';

/** A grid cell of the data view, by its place on the page and its column name. */
export interface CellRef {
  /** The data tab the cell is in. */
  tabId: string;
  row: number;
  col: number;
  column: string;
  /** The row's id (`rowIdOf`); null when the row has no usable key. */
  rowId: string | null;
}

interface OpenEditors {
  /** The cell each data tab shows in its value editor, if any. */
  byTab: Record<string, CellRef | undefined>;
}

export const useValueEditors = create<OpenEditors>()(() => ({ byTab: {} }));

/**
 * Opens the value editor of a data tab on `cell`: a cell the page cut short (`truncated`) is never
 * edited in place, it is handed here instead; Shift+Enter and the cell menu open any cell.
 */
export function openValueEditor(cell: CellRef): void {
  useValueEditors.setState((s) => ({ byTab: { ...s.byTab, [cell.tabId]: cell } }));
}

export function closeValueEditor(tabId: string): void {
  useValueEditors.setState((s) => ({ byTab: { ...s.byTab, [tabId]: undefined } }));
}
