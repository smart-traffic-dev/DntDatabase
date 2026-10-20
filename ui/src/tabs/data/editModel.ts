import type { CellInput, TableDataPage, Value } from '../../bindings';
import type { RowId, Staged } from '../../store/dataEdits';

/** How a cell of the grid differs from the server: the grid colours it, the legend counts them. */
export type CellStatus = 'edited' | 'new' | 'deleted' | 'conflict';

const other = (text: string): Value => ({ t: 'Other', v: { type_name: '', text } });

/** The value the grid shows for a staged cell input. */
export function inputToValue(input: CellInput): Value {
  switch (input.kind) {
    case 'null':
      return { t: 'Null' };
    case 'default':
      return other('DEFAULT');
    case 'text':
      return { t: 'Text', v: input.text };
    case 'value':
      return input.value;
    case 'base64':
      return other('(binary value)');
    case 'loaded':
      return other('(loaded value)');
  }
}

/**
 * The page with the staged changes applied for display: edited cells hold their new value and
 * new rows follow the last row (columns they do not set read DEFAULT). Rows marked for deletion
 * stay where they are.
 */
export function effectivePage(page: TableDataPage, staged: Staged, rowIds: (RowId | null)[]): TableDataPage {
  const names = page.columns.map((c) => c.name);
  const rows = page.rows.map((row, i) => {
    const id = rowIds[i];
    const cells = id ? staged.updates.get(id)?.cells : undefined;
    if (!cells) return row;
    return row.map((value, at) => {
      const input = cells.get(names[at] ?? '');
      return input ? inputToValue(input) : value;
    });
  });
  for (const insert of staged.inserts) {
    rows.push(names.map((name) => {
      const input = insert.values.get(name);
      return input ? inputToValue(input) : other('DEFAULT');
    }));
  }
  return { ...page, rows };
}

/** The status of the cell at (`row`, column `name`) of the effective page, or null. */
export function cellStatus(
  page: TableDataPage,
  staged: Staged,
  conflicts: readonly RowId[],
  rowIds: (RowId | null)[],
  row: number,
  name: string,
): CellStatus | null {
  if (row >= page.rows.length) return 'new';
  const id = rowIds[row];
  if (!id) return null;
  if (conflicts.includes(id)) return 'conflict';
  if (staged.deletes.has(id)) return 'deleted';
  return staged.updates.get(id)?.cells.has(name) ? 'edited' : null;
}
