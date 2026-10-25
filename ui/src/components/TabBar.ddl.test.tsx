import { render, screen } from '@testing-library/react';
import { act } from 'react';
import { beforeEach, expect, it } from 'vitest';
import type { DdlEditOpen } from '../bindings';
import { useDdlEdits } from '../store/ddlEdits';
import { useTabs } from '../store/tabs';
import { ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { TabBar } from './TabBar';

const id = 'ddl/one';
const open: DdlEditOpen = {
  edit_id: 'e1',
  text: 'CREATE VIEW v AS select 1',
  target: { kind: 'View', database: 'shop', schema: null, name: 'v', table: null },
  object: { database: 'shop', schema: null, name: 'v', kind: 'View' },
  server: serverInfo('MySql8'),
  env: null,
  read_only: false,
  strategies: [],
  strategy: null,
  blocked_reason: null,
  notes: [],
};

beforeEach(() => {
  resetStores();
  mockBackend({ ddl_edit_open: () => open, ddl_edit_close: () => null });
});

it('a DDL tab shows the "DDL" label and a dirty dot only while its text differs', async () => {
  useTabs.getState().open({ id, kind: 'ddl', connectionId: ids.prod, objectKind: 'View', title: 'sales_v' });
  await act(async () => {
    await useDdlEdits.getState().load(id, { connectionId: ids.prod, scope: { database: 'shop', schema: null }, kind: 'View', name: 'v', table: null });
  });
  render(<TabBar />);
  const tab = () => screen.getByRole('tab', { name: /sales_v/ }).closest('.tab')!;
  expect(tab().querySelector('.tab-kind')?.textContent).toBe('DDL');
  expect(tab()).not.toHaveClass('dirty');

  act(() => useDdlEdits.getState().setText(id, 'CREATE VIEW v AS select 2'));
  expect(tab()).toHaveClass('dirty');
  expect(screen.getByLabelText('Unsaved DDL changes')).toBeInTheDocument();

  act(() => useDdlEdits.getState().revert(id));
  expect(tab()).not.toHaveClass('dirty');
});
