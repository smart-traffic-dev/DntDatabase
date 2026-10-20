// Entry points for menus: act on the result grid that was focused last. Each mounted grid
// registers itself; the menu bar calls the plain functions below.

export type CopyMode = 'tsv' | 'tsv-headers' | 'insert' | 'csv' | 'column';

export interface GridCommands {
  copySelection(mode: CopyMode): void;
  openFindInResults(): void;
  openRecordView(): void;
}

const grids: GridCommands[] = [];

/** Registers a grid; the returned function unregisters it. */
export function registerGrid(commands: GridCommands): () => void {
  grids.push(commands);
  return () => {
    const i = grids.indexOf(commands);
    if (i >= 0) grids.splice(i, 1);
  };
}

/** Makes `commands` the grid menus act on (called when a grid gets the focus). */
export function activateGrid(commands: GridCommands): void {
  const i = grids.indexOf(commands);
  if (i >= 0 && i !== grids.length - 1) {
    grids.splice(i, 1);
    grids.push(commands);
  }
}

const active = () => grids[grids.length - 1];

/** Copies the active grid's selection (`tsv`, `tsv-headers`, `insert`, `csv`, or `column` for the column name). */
export const copySelection = (mode: CopyMode) => active()?.copySelection(mode);
/** Opens the find bar of the active grid. */
export const openFindInResults = () => active()?.openFindInResults();
/** Opens the record view of the active grid's current row. */
export const openRecordView = () => active()?.openRecordView();
