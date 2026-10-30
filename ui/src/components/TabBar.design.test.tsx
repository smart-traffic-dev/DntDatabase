import { render, screen } from '@testing-library/react';
import { act } from 'react';
import { beforeEach, expect, it } from 'vitest';
import type { DesignOpen, TableDef } from '../bindings';
import { useTableDesigns } from '../store/tableDesigns';
import { designTabId, useTabs } from '../store/tabs';
import { ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { TabBar } from './TabBar';

const scope = { database: 'shop', schema: null };
const id = designTabId(ids.prod, scope, 'orders');
const def = {
  original_name: 'orders',
  name: 'orders',
  columns: [],
  primary_key: null,
  indexes: [],
  foreign_keys: [],
  checks: [],
  options: { engine: null, charset: null, collation: null, row_format: null, locked: false },
  comment: null,
  read_only_reason: null,
} satisfies TableDef;
const open: DesignOpen = { design_id: 'd1', def, locked: [], catalog: [], server: serverInfo('MySql8'), env: null, read_only: false };

beforeEach(() => {
  resetStores();
  mockBackend({ table_design_open: () => open, table_design_render: () => ({ statements: [], impacts: [], errors: [], error: null }), table_design_close: () => null });
});

it('a design tab shows the "Design" label and a dirty dot only while its draft differs', async () => {
  useTabs.getState().open({ id, kind: 'design', connectionId: ids.prod, scope, table: 'orders', title: 'orders' });
  await act(async () => {
    await useTableDesigns.getState().load(id, ids.prod, scope, 'orders');
  });
  render(<TabBar />);
  const tab = () => screen.getByRole('tab', { name: /orders/ }).closest('.tab')!;
  expect(tab().querySelector('.tab-kind')?.getAttribute('data-kind')).toBe('Table design');
  expect(tab()).not.toHaveClass('dirty');
  expect(screen.queryByLabelText('Unsaved design changes')).toBeNull();

  act(() => useTableDesigns.getState().edit(id, (d) => ({ ...d, comment: 'x' })));
  expect(tab()).toHaveClass('dirty');
  expect(screen.getByLabelText('Unsaved design changes')).toBeInTheDocument();

  act(() => useTableDesigns.getState().discard(id));
  expect(tab()).not.toHaveClass('dirty');
});
