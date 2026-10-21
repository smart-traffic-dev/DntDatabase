import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import type { DesignPreview, DesignReport } from '../bindings';
import { appError, callsOf, config, ids, mockBackend, resetStores, serverInfo, Failure } from '../test/backend';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTableDesigns } from '../store/tableDesigns';
import { designTabId, objectTabId, useTabs, type DesignTabRef } from '../store/tabs';
import { openDesignTab } from '../components/openObject';
import { useTree } from '../store/tree';
import { column, opened, scope, tab } from '../tabs/designer/designerTestKit';
import { TableDesignDialog } from './TableDesignDialog';

const preview: DesignPreview = {
  plan_id: 'plan',
  statements: ['ALTER TABLE `shop`.`orders` CHANGE COLUMN `total` `amount` int NULL'],
  impacts: [{ impact: { kind: 'data_loss', element: { kind: 'column', name: 'total' }, message: 'Drops values.', count_sql: 'SELECT 1' }, rows: 412, counted: true }],
  confirm_name: null,
  expires_in_seconds: 300,
  row_estimate: null,
};
const applied: DesignReport = { outcome: 'applied', error: null, warnings: [], statements_sent: 1 };

const newTab: DesignTabRef = { id: designTabId(ids.prod, scope, null), kind: 'design', connectionId: ids.prod, scope, table: null, title: 'New table' };

async function setup(env: 'production' | null, handlers: Record<string, (a: Record<string, unknown>) => unknown>, plan: DesignPreview = preview, at: DesignTabRef = tab) {
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: env })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  mockBackend({ table_design_open: () => opened(), table_design_preview: () => plan, table_design_close: () => null, ...handlers });
  useTabs.getState().open(at);
  await useTableDesigns.getState().load(at.id, ids.prod, scope, at.table);
  useTableDesigns.getState().edit(at.id, (d) => ({ ...d, columns: [d.columns[0]!, column('amount', { original: 'total' })] }));
  const onClose = vi.fn();
  const user = userEvent.setup();
  render(<TableDesignDialog tabId={at.id} onClose={onClose} />);
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

it('rebuild warnings show the row estimate', async () => {
  const rebuild = { impact: { kind: 'rebuild' as const, element: { kind: 'table' as const }, message: 'May rebuild the table.', count_sql: null }, rows: null, counted: false };
  await setup(null, {}, { ...preview, impacts: [...preview.impacts, rebuild], row_estimate: 1_234_567 });
  expect(await screen.findByText('May rebuild the table. (≈ 1.2M rows)')).toBeInTheDocument();
  expect(screen.getByText('Drops values. (412 rows have a value)')).toBeInTheDocument();
});

it('rebuild warnings without an estimate stay as they are', async () => {
  const rebuild = { impact: { kind: 'rebuild' as const, element: { kind: 'table' as const }, message: 'May rebuild the table.', count_sql: null }, rows: null, counted: false };
  await setup(null, {}, { ...preview, impacts: [rebuild], row_estimate: null });
  expect(await screen.findByText('May rebuild the table.')).toBeInTheDocument();
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
  expect(screen.getByText('The changes were not applied. Your draft is kept.')).toBeInTheDocument();
  expect(opens()).toBe(1);
  expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);
});

it('unknown_reloads', async () => {
  const report: DesignReport = { outcome: 'unknown', error: appError('outcome_unknown', 'The outcome is unknown.'), warnings: [], statements_sent: 1 };
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
  // The definition the draft was edited from is kept with it (its changes are measured against it).
  expect(useTableDesigns.getState().byTab[tab.id]!.staleBase).toEqual(opened().def);
  await user.click(screen.getByRole('button', { name: 'Reopen' }));
  expect(onClose).toHaveBeenCalled();
  await vi.waitFor(() => expect(opens()).toBe(2));
  expect(useTableDesigns.getState().byTab[tab.id]!.staleDraft).toEqual(stale);
});

it('an expired plan offers Preview again, which previews the draft anew', async () => {
  let previews = 0;
  const { user } = await setup(null, {
    table_design_preview: () => ({ ...preview, plan_id: `plan-${++previews}` }),
    table_design_apply: (a) => {
      if (a.planId === 'plan-1') throw new Failure(appError('plan_expired', 'The run plan expired; run it again.'));
      return applied;
    },
  });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('The run plan expired');
  await user.click(screen.getByRole('button', { name: 'Preview again' }));
  expect(await screen.findByLabelText('SQL preview')).toBeInTheDocument();
  expect(callsOf('table_design_preview')).toHaveLength(2);
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Preview again' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(callsOf('table_design_apply')).toHaveLength(2));
  expect(callsOf('table_design_apply')[1]).toMatchObject({ planId: 'plan-2' });
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

const unknownWith = (error: ReturnType<typeof appError>): DesignReport => ({ outcome: 'unknown', error, warnings: [], statements_sent: 1 });
const UNKNOWN_TEXT = 'The result is unknown. The table structure was reloaded; check it before continuing.';

it.each([['connection_lost'], ['plan_stale']])('unknown with %s is still unknown: refresh, verify object tabs, reload', async (kind) => {
  const object = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
  const objectId = objectTabId(ids.prod, object);
  useTabs.getState().open({ kind: 'object', id: objectId, objectRef: object, connectionId: ids.prod, title: 'orders' });
  const { user, onClose } = await setup(null, { table_design_apply: () => unknownWith(appError(kind, 'dropped')) });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByText(UNKNOWN_TEXT)).toBeInTheDocument();
  await vi.waitFor(() => expect(opens()).toBe(2));
  expect(useTabs.getState().tabs.find((t) => t.id === objectId)).toMatchObject({ needsVerification: true });
  expect(useTree.getState().refresh).toHaveBeenCalled();
  expect(useTableDesigns.getState().byTab[tab.id]!.lostDraft).toBeNull();
  expect(screen.queryByRole('button', { name: 'Reopen' })).not.toBeInTheDocument();
  expect(onClose).not.toHaveBeenCalled();
});

it('a sent rollback changed nothing: no reload, no refresh, the draft stays', async () => {
  const report: DesignReport = { outcome: 'rolled_back', error: appError('query', 'Deadlock'), warnings: [], statements_sent: 3 };
  const { user } = await setup(null, { table_design_apply: () => report });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByText('The changes were rolled back. Your draft is kept.')).toBeInTheDocument();
  expect(opens()).toBe(1);
  expect(useTree.getState().refresh).not.toHaveBeenCalled();
  expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);
});

it('not applied with a lost connection and nothing sent marks the draft lost', async () => {
  const report: DesignReport = { outcome: 'not_applied', error: appError('connection_lost', 'gone'), warnings: [], statements_sent: 0 };
  const { user, onClose } = await setup(null, { table_design_apply: () => report });
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(useTableDesigns.getState().byTab[tab.id]!.lostDraft).not.toBeNull();
});

it('unknown on a never-created table does not claim a reload and keeps the draft', async () => {
  const { user } = await setup(null, { table_design_apply: () => unknownWith(appError('outcome_unknown', 'x')) }, preview, newTab);
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByText(/may or may not have been created/)).toBeInTheDocument();
  expect(screen.queryByText(/was reloaded/)).not.toBeInTheDocument();
  expect(opens()).toBe(1);
  expect(useTableDesigns.getState().isDirty(newTab.id)).toBe(true);
});

it('unknown after a rename reloads under the new name when it exists', async () => {
  const { user } = await setup(null, { table_design_apply: () => unknownWith(appError('outcome_unknown', 'x')) });
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, name: 'orders2' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(opens()).toBe(2));
  expect(callsOf('table_design_open')[1]).toMatchObject({ table: 'orders2' });
  expect(useTabs.getState().tabs.some((t) => t.id === designTabId(ids.prod, scope, 'orders2'))).toBe(true);
});

it('unknown after a rename falls back to the old name when the new one does not open', async () => {
  const { user } = await setup(null, {
    table_design_apply: () => unknownWith(appError('outcome_unknown', 'x')),
    table_design_open: (a) => {
      if (a.table === 'orders2') throw new Failure(appError('not_found', 'no such table'));
      return opened();
    },
  });
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, name: 'orders2' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(opens()).toBe(3));
  expect(callsOf('table_design_open').map((c) => c.table)).toEqual(['orders', 'orders2', 'orders']);
  expect(useTabs.getState().tabs.some((t) => t.id === tab.id)).toBe(true);
  expect(useTableDesigns.getState().byTab[tab.id]!.error).toBeNull();
});

it('after creating a table the tab is re-keyed: New table opens a blank designer, Design table focuses this one', async () => {
  const { user, onClose } = await setup(null, { table_design_apply: () => applied }, preview, newTab);
  useTableDesigns.getState().edit(newTab.id, (d) => ({ ...d, name: 'customers' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  const moved = designTabId(ids.prod, scope, 'customers');
  expect(useTabs.getState().tabs.filter((t) => t.kind === 'design').map((t) => t.id)).toEqual([moved]);
  expect(useTabs.getState().activeId).toBe(moved);
  expect(useTableDesigns.getState().byTab[newTab.id]).toBeUndefined();
  expect(useTableDesigns.getState().byTab[moved]).toBeDefined();
  expect(callsOf('table_design_close')).toHaveLength(0);
  const blank = openDesignTab(ids.prod, scope, null);
  expect(blank).toBe(newTab.id);
  expect(useTabs.getState().tabs.filter((t) => t.kind === 'design')).toHaveLength(2);
  expect(openDesignTab(ids.prod, scope, 'customers')).toBe(moved);
  expect(useTabs.getState().tabs.filter((t) => t.kind === 'design')).toHaveLength(2);
});

it('after a rename Design table on the new name focuses this tab without a second session', async () => {
  const { user, onClose } = await setup(null, { table_design_apply: () => applied });
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, name: 'orders2' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  const moved = designTabId(ids.prod, scope, 'orders2');
  expect(openDesignTab(ids.prod, scope, 'orders2')).toBe(moved);
  expect(useTabs.getState().tabs.filter((t) => t.kind === 'design')).toHaveLength(1);
  expect(opens()).toBe(2);
  expect(callsOf('table_design_close')).toHaveLength(0);
});

it('an applied rename onto a name that already has a designer keeps that tab and its draft', async () => {
  const { user, onClose } = await setup(null, { table_design_apply: () => applied });
  const other = designTabId(ids.prod, scope, 'orders2');
  useTabs.getState().open({ id: other, kind: 'design', connectionId: ids.prod, scope, table: 'orders2', title: 'orders2' });
  await useTableDesigns.getState().load(other, ids.prod, scope, 'orders2');
  useTableDesigns.getState().edit(other, (d) => ({ ...d, comment: 'mine' }));
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, name: 'orders2' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(useTabs.getState().tabs.filter((t) => t.kind === 'design').map((t) => t.id)).toEqual([other]);
  expect(useTabs.getState().activeId).toBe(other);
  expect(useTableDesigns.getState().byTab[other]!.draft!.comment).toBe('mine');
});

it('Close works while the preview is loading', async () => {
  let resolve!: (p: DesignPreview) => void;
  const { user, onClose } = await setup(null, { table_design_preview: () => new Promise<DesignPreview>((r) => { resolve = r; }) });
  const close = screen.getByRole('button', { name: 'Close' });
  expect(close).toBeEnabled();
  await user.click(close);
  expect(onClose).toHaveBeenCalled();
  await act(async () => resolve(preview));
});

it('a double click on Apply sends once', async () => {
  let resolve!: (r: DesignReport) => void;
  const { user } = await setup(null, { table_design_apply: () => new Promise<DesignReport>((r) => { resolve = r; }) });
  const apply = await screen.findByRole('button', { name: 'Apply' });
  await user.dblClick(apply);
  expect(callsOf('table_design_apply')).toHaveLength(1);
  await act(async () => resolve({ ...applied, outcome: 'not_applied', statements_sent: 0 }));
});

it('staleDraft is cleared by an applied change and by discard', async () => {
  const { user, onClose } = await setup(null, { table_design_apply: () => applied });
  useTableDesigns.getState().setStaleDraft(tab.id, useTableDesigns.getState().byTab[tab.id]!.draft);
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(useTableDesigns.getState().byTab[tab.id]!.staleDraft).toBeNull();
  useTableDesigns.getState().setStaleDraft(tab.id, useTableDesigns.getState().byTab[tab.id]!.draft);
  useTableDesigns.getState().discard(tab.id);
  expect(useTableDesigns.getState().byTab[tab.id]!.staleDraft).toBeNull();
});
