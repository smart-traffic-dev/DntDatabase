/** A grid cell of the data view, by its place on the page and its column name. */
export interface CellRef {
  row: number;
  col: number;
  column: string;
  /** The row's id (`rowIdOf`); null when the row has no usable key. */
  rowId: string | null;
}

/**
 * The hook for the full-value editor: a cell the page cut short (`truncated`) is never edited
 * in place, it is handed here instead. The editor itself is a separate piece of the data view;
 * until it is mounted a cut cell is view-only.
 */
export function openValueEditor(cell: CellRef): void {
  void cell;
}
