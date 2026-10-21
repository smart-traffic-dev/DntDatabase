import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import type { ObjectRef, TableDataPage } from '../bindings';
import { reportGridSelection, resetGridSelection } from '../store/gridSelection';
import { useObjectData } from '../store/objectData';
import { useTableData } from '../store/tableData';
import { objectTabId, useTabs } from '../store/tabs';
import { config, ids, mockBackend, resetStores } from '../test/backend';
import { useConnections } from '../store/connections';
import { StatusBar } from './StatusBar';

const orders: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const tabId = objectTabId(ids.prod, orders);

const page = (n: number): TableDataPage => ({
  columns: [{ name: 'id', db_type: 'int', logical: 'Integer', nullable: false, origin_table: null, origin_column: null }],
  rows: Array.from({ length: n }, (_, i) => [{ t: 'Int', v: i + 1 }]),
  has_more: true,
  truncated_cells: 0,
  truncated: [],
  size_capped: false,
  key_order: [],
  row_versions: null,
  hidden_key: [],
  hidden_key_values: [],
  elapsed_ms: 1,
});

/** An open object tab on its Data section, a loaded page at `offset` and the table's estimate. */
function openData(rows: number, offset = 0, estimate: number | null = 12_345) {
  act(() => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    useTabs.getState().open({ id: tabId, kind: 'object', connectionId: ids.prod, objectRef: orders, title: 'orders' });
    useTableData.setState({
      byTab: {
        [tabId]: {
          connectionId: ids.prod,
          pageSize: 100,
          offset,
          back: [],
          sort: [],
          filter: null,
          draft: { mode: 'and', rows: [] },
          filterBarOpen: false,
          columns: ['id'],
          page: { status: 'ok', data: page(rows) },
        },
      },
    });
    useObjectData.setState({
      byTab: { [tabId]: { sub: 'data', detail: { status: 'ok', data: { row_estimate: estimate } as never } } },
    });
  });
}

describe('status bar of a data tab', () => {
  beforeEach(() => {
    resetStores();
    resetGridSelection();
    useObjectData.setState({ byTab: {} });
    mockBackend({ tabs_save: () => null });
  });

  it('shows the rows on screen and the total on the right', () => {
    render(<StatusBar />);
    openData(100, 100);
    expect(screen.getByTestId('status-data-rows')).toHaveTextContent('Rows 101–200 of ~12,345');
    expect(screen.queryByTestId('status-cursor')).toBeNull();
  });

  it('shows the grid selection summary while there is one', () => {
    render(<StatusBar />);
    openData(3, 0, null);
    expect(screen.getByTestId('status-data-rows')).toHaveTextContent(/^Rows 1–3$/);
    expect(screen.queryByTestId('status-data-selection')).toBeNull();
    act(() => reportGridSelection(tabId, '2 rows selected'));
    expect(screen.getByTestId('status-data-selection')).toHaveTextContent('2 rows selected');
    act(() => reportGridSelection(tabId, null));
    expect(screen.queryByTestId('status-data-selection')).toBeNull();
  });

  it('says nothing on the other sections of the object tab', () => {
    render(<StatusBar />);
    openData(3);
    act(() => useObjectData.getState().setSub(tabId, 'ddl'));
    expect(screen.queryByTestId('status-data-rows')).toBeNull();
  });
});
