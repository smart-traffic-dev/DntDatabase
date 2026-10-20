import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ApplyReport, ColumnMeta, EditPreview, EditableInfo, ObjectRef, TableDataPage, Value } from '../bindings';
import { DialogHost } from '../dialogs/DialogHost';
import { objectTabId, useTabs, type ObjectTabRef as Tab } from '../store/tabs';
import { callsOf, ids, mockBackend, resetStores } from '../test/backend';
import { DataView } from './DataView';

// P3 acceptance (Task 17, ruling R1): the browser-level scenario "filter -> edit a cell -> preview ->
// apply -> page reloads" walked end to end through the real DataView, stores and dialogs. The canvas
// grid is replaced by one button per cell, as in DataView.test.tsx.
vi.mock('./query/ResultGrid', () => ({
  ResultGrid: ({
    result,
    editing,
  }: {
    result: { columns: { name: string }[]; rowCount: number };
    editing?: { isEditable(r: number, c: number): boolean; status(r: number, c: number): string | null; onEdit(r: number, c: number, t: string): void };
  }) => (
    <div data-testid="grid">
      {editing
        ? Array.from({ length: result.rowCount }).flatMap((_, r) =>
            result.columns.map((_c, ci) => (
              <button
                key={`${r},${ci}`}
                type="button"
                aria-label={`cell ${r},${ci}`}
                data-status={editing.status(r, ci) ?? ''}
                onClick={() => editing.onEdit(r, ci, 'edited')}
              />
            )),
          )
        : null}
    </div>
  ),
}));

const col = (name: string): ColumnMeta => ({ name, db_type: 'int', logical: 'Integer', nullable: false, origin_table: null, origin_column: null });
const int = (v: number): Value => ({ t: 'Int', v });
const page = (ids_: number[]): TableDataPage => ({
  columns: [col('id'), col('qty')],
  rows: ids_.map((i) => [int(i), int(0)]),
  has_more: false,
  truncated_cells: 0,
  truncated: [],
  size_capped: false,
  key_order: [],
  row_versions: ids_.map((i) => String(i + 100)),
  hidden_key: [],
  hidden_key_values: [],
  elapsed_ms: 1,
});
const objectRef: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const info: EditableInfo = { editable: true, reason: null, key: ['id'], hidden_key: [], editable_columns: ['qty'], atomic: 'yes', protection: 'row_version' };
const preview: EditPreview = {
  plan_id: 'p1',
  statements: ["UPDATE `orders` SET `qty` = 'edited' WHERE `id` = 2"],
  verdict: 'allow',
  atomic: 'yes',
  protections: [{ level: 'full' }],
  reason: null,
};
const committed: ApplyReport = {
  outcome: { outcome: 'committed' },
  statements: [{ index: 0, affected: 1, error: null, conflict: false }],
  inserted: [],
  error: null,
};

beforeEach(() => resetStores());

describe('P3 flow: filter, edit, preview, apply, reload', () => {
  it('walks the whole path and keeps the filter on the reload', async () => {
    const tab: Tab = { id: objectTabId(ids.prod, objectRef), kind: 'object', connectionId: ids.prod, objectRef, title: 'orders' };
    // The server answers the filtered page only when the filter is sent.
    mockBackend({
      get_table_page_size: () => 100,
      table_data_cancel: () => true,
      data_editable: () => info,
      table_data: (a) => (a.filter ? page([2]) : page([1, 2, 3])),
      data_preview: () => preview,
      data_apply: () => committed,
    });
    useTabs.getState().open(tab);
    const user = userEvent.setup();
    render(
      <>
        <DataView tab={tab} estimate={null} />
        <DialogHost />
      </>,
    );
    await screen.findByText('Rows 1–3');

    // 1. Filter id = 2 (server side).
    await user.click(screen.getByRole('button', { name: 'Filter' }));
    const bar = screen.getByRole('search', { name: 'Filter' });
    await user.type(within(bar).getByRole('textbox', { name: 'Value 1' }), '2');
    await user.click(within(bar).getByRole('button', { name: 'Apply' }));
    const filter = { mode: 'and', conditions: [{ column: 'id', op: 'eq', values: ['2'] }] };
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ offset: 0, filter }));
    await screen.findByText('Rows 1–1');

    // 2. Edit a cell, then preview: the dialog shows the real statements, nothing ran yet.
    await user.click(screen.getByRole('button', { name: 'cell 0,1' }));
    expect(screen.getByRole('button', { name: 'cell 0,1' })).toHaveAttribute('data-status', 'edited');
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review changes' });
    expect(within(dialog).getByText(/UPDATE `orders` SET/)).toBeInTheDocument();
    expect(callsOf('data_apply')).toEqual([]);

    // 3. Confirm: apply runs the previewed plan and the page reloads, still filtered.
    const before = callsOf('table_data').length;
    await user.click(within(dialog).getByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(callsOf('table_data').length).toBe(before + 1));
    expect(callsOf('data_apply')).toEqual([{ planId: 'p1', confirmed: true }]);
    expect(callsOf('table_data').at(-1)).toMatchObject({ filter });
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'cell 0,1' })).toHaveAttribute('data-status', '');
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
  });
});
