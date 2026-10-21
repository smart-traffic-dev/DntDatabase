import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import type { DesignPreview, DesignReport } from '../bindings';
import { appError, callsOf, config, ids, mockBackend, resetStores, serverInfo, Failure } from '../test/backend';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTableDesigns } from '../store/tableDesigns';
import { objectTabId, useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { column, opened, scope, tab } from '../tabs/designer/designerTestKit';
import { TableDesignDialog } from './TableDesignDialog';

const preview: DesignPreview = {
  plan_id: 'plan',
  statements: ['ALTER TABLE `shop`.`orders` CHANGE COLUMN `total` `amount` int NULL'],
  impacts: [{ impact: { kind: 'data_loss', element: { kind: 'column', name: 'total' }, message: 'Drops values.', count_sql: 'SELECT 1' }, rows: 412, counted: true }],
  confirm_name: null,
  expires_in_seconds: 300,
};
const applied: DesignReport = { outcome: 'applied', error: null, warnings: [], statements_sent: 1 };

async function setup(env: 'production' | null, handlers: Record<string, (a: Record<string, unknown>) => unknown>, plan: DesignPreview = preview) {
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: env })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  mockBackend({ table_design_open: () => opened(), table_design_preview: () => plan, table_design_close: () => null, ...handlers });
  useTabs.getState().open(tab);
  await useTableDesigns.getState().load(tab.id, ids.prod, scope, 'orders');
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, columns: [d.columns[0]!, column('amount', { original: 'total' })] }));
  const onClose = vi.fn();
  const user = userEvent.setup();
  render(<TableDesignDialog tabId={tab.id} onClose={onClose} />);
  return { user, onClose };
}
const opens = () => callsOf('table_design_open').length;

beforeEach(() => {
  resetStores();
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
});

it('production_requires_exact_name', async () => {
  const { user } = await setup('production', { table_design_apply: () => applied }, { ...preview, confirm_name: 'orders' });
  expect(await screen.findByLabelText('SQL preview')).toHaveTextContent('CHANGE COLUMN `total` `amount`');
  expect(screen.getByTestId('env-chip')).toBeInTheDocument();
  expect(screen.getByText('Drops values. (412 rows have a value)')).toBeInTheDocument();
  const apply = screen.getByRole('button', { name: 'Apply' });
  expect(apply).toBeDisabled();
  await user.type(screen.getByLabelText('Confirmation name'), 'Orders');
  expect(apply).toBeDisabled();
  await user.clear(screen.getByLabelText('Confirmation name'));
  await user.type(screen.getByLabelText('Confirmation name'), 'orders');
  await user.click(apply);
  await vi.waitFor(() => expect(callsOf('table_design_apply')).toHaveLength(1));
  expect(callsOf('table_design_apply')[0]).toMatchObject({ planId: 'plan', confirmed: true, name: 'orders' });
});

it('local_applies_on_click', async () => {
  const { user, onClose } = await setup(null, { table_design_apply: () => applied });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(callsOf('table_design_apply')[0]).toMatchObject({ planId: 'plan', confirmed: true, name: null });
  expect(useDialogs.getState().notice).toBe('Changes applied.');
  expect(opens()).toBe(2);
  expect(useTree.getState().refresh).toHaveBeenCalled();
});

it('rolled_back_keeps_draft', async () => {
  const report: DesignReport = { outcome: 'rolled_back', error: appError('query', 'Duplicate entry'), warnings: ['History save failed'], statements_sent: 0 };
  const { user, onClose } = await setup(null, { table_design_apply: () => report });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Duplicate entry');
  expect(screen.getByText('History save failed')).toBeInTheDocument();
  expect(onClose).not.toHaveBeenCalled();
  expect(opens()).toBe(1);
  expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);
});

it('a lock wait error keeps the draft like not applied', async () => {
  const { user } = await setup(null, { table_design_apply: () => { throw new Failure(appError('query', 'lock timeout', { sqlstate: '55P03' })); } });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('lock timeout');
  expect(opens()).toBe(1);
  expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);
});

it('unknown_reloads', async () => {
  const report: DesignReport = { outcome: 'unknown', error: appError('connection', 'lost'), warnings: [], statements_sent: 1 };
  const { user, onClose } = await setup(null, { table_design_apply: () => report });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByText('The result is unknown. The table structure was reloaded; check it before continuing.')).toBeInTheDocument();
  await vi.waitFor(() => expect(opens()).toBe(2));
  expect(onClose).not.toHaveBeenCalled();
});

it('stale_offers_reopen', async () => {
  const { user, onClose } = await setup(null, { table_design_apply: () => { throw new Failure(appError('plan_stale', 'Preview again.')); } });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('The table changed since you opened it.');
  const stale = useTableDesigns.getState().byTab[tab.id]!.draft;
  expect(useTableDesigns.getState().byTab[tab.id]!.staleDraft).toEqual(stale);
  await user.click(screen.getByRole('button', { name: 'Reopen' }));
  expect(onClose).toHaveBeenCalled();
  await vi.waitFor(() => expect(opens()).toBe(2));
  expect(useTableDesigns.getState().byTab[tab.id]!.staleDraft).toEqual(stale);
});

it('stop_calls_cancel', async () => {
  let resolve!: (r: DesignReport) => void;
  const { user } = await setup(null, { table_design_apply: () => new Promise<DesignReport>((r) => { resolve = r; }), table_design_cancel: () => null });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(useDialogs.getState().tableDesignRunning).toBe(true);
  await user.click(await screen.findByRole('button', { name: 'Stop' }));
  expect(callsOf('table_design_cancel')[0]?.operationId).toBe(callsOf('table_design_apply')[0]?.operationId);
  await act(async () => resolve({ ...applied, outcome: 'not_applied', statements_sent: 0 }));
  expect(useDialogs.getState().tableDesignRunning).toBe(false);
  expect(screen.queryByRole('button', { name: 'Stop' })).not.toBeInTheDocument();
});

it('a lost design on preview marks the draft lost and closes', async () => {
  const { onClose } = await setup(null, { table_design_preview: () => { throw new Failure(appError('config', 'gone', { field: 'design_id' })); } });
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(useTableDesigns.getState().byTab[tab.id]!.lostDraft).not.toBeNull();
  expect(useDialogs.getState().notice).toContain('connection to this table was lost');
});

it('a rename updates the object tabs and the design tab', async () => {
  const object = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  const handlers = { table_design_apply: () => applied };
  useTabs.getState().open({ kind: 'object', id: objectTabId(ids.prod, object), objectRef: object, connectionId: ids.prod, title: 'orders' });
  const { user, onClose } = await setup(null, handlers);
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, name: 'orders2' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(useTabs.getState().tabs.find((t) => t.kind === 'object')).toMatchObject({ title: 'orders2' });
  expect(useTabs.getState().tabs.find((t) => t.kind === 'design')).toMatchObject({ title: 'orders2', table: 'orders2' });
  expect(callsOf('table_design_open')[1]).toMatchObject({ table: 'orders2' });
});
