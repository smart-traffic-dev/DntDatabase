import { useDialogs } from '../store/dialogs';
import { installCloseGuard, type CloseGuardWindow } from '../store/queryTabs';
import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import type { ActionPreview, ActionReport, ObjectAction, ObjectRef } from '../bindings';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo, appError, Failure } from '../test/backend';
import { useConnections } from '../store/connections';
import { objectTabId, useTabs } from '../store/tabs';
import { useDataEdits } from '../store/dataEdits';
import { useTree } from '../store/tree';
import { ObjectActionDialog, refreshAfterAction } from './ObjectActionDialog';
import { objectActionItems } from '../components/TreeContextMenu';

const object: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const action: ObjectAction = { kind: 'drop_object', object };
const request = { connectionId: ids.prod, action };
const preview: ActionPreview = { plan_id: 'plan', connection_id: ids.prod, action, sql: 'DROP TABLE `shop`.`orders`', confirm_name: 'orders', warnings: ['Removes the object.'], expires_in_seconds: 300 };
const report: ActionReport = { outcome: 'applied', error: null, warnings: [] };
beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});
it('previews exact SQL and requires exact typed confirmation before applying once', async () => {
  const user = userEvent.setup();
  mockBackend({ object_action_preview: () => preview, object_action_apply: () => report });
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  await user.click(screen.getByRole('button', { name: 'Preview SQL' }));
  expect(await screen.findByLabelText('SQL preview')).toHaveTextContent(preview.sql);
  expect(screen.getByRole('button', { name: 'Apply' })).toBeDisabled();
  await user.type(screen.getByLabelText('Confirmation name'), 'Orders');
  expect(screen.getByRole('button', { name: 'Apply' })).toBeDisabled();
  await user.clear(screen.getByLabelText('Confirmation name'));
  await user.type(screen.getByLabelText('Confirmation name'), 'orders');
  await user.click(screen.getByRole('button', { name: 'Apply' }));
  expect(await screen.findByText('Operation applied.')).toBeInTheDocument();
  expect(callsOf('object_action_apply')).toHaveLength(1);
  expect(callsOf('object_action_apply')[0]).toMatchObject({ planId: 'plan', confirmed: true, name: 'orders' });
  expect(screen.queryByRole('button', { name: 'Apply' })).not.toBeInTheDocument();
});
it('invalidates the preview when a rename input changes', async () => {
  const user = userEvent.setup();
  const rename: ObjectAction = { kind: 'rename_table', object, name: 'next' };
  mockBackend({ object_action_preview: () => ({ ...preview, action: rename, confirm_name: null }) });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: rename }} onClose={() => {}} />);
  await user.click(screen.getByRole('button', { name: 'Preview SQL' }));
  await screen.findByLabelText('SQL preview');
  await user.type(screen.getByLabelText('Name'), '2');
  expect(screen.queryByRole('button', { name: 'Apply' })).not.toBeInTheDocument();
});
it('requires a new preview after a stale plan and never retries Apply', async () => {
  const user = userEvent.setup();
  mockBackend({ object_action_preview: () => ({ ...preview, confirm_name: null }), object_action_apply: () => { throw new Failure(appError('plan_stale', 'Preview again.')); } });
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  await user.click(screen.getByRole('button', { name: 'Preview SQL' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Preview again.');
  expect(screen.getByRole('button', { name: 'Preview SQL' })).toBeEnabled();
  expect(callsOf('object_action_apply')).toHaveLength(1);
});
it('Stop uses the same operation id and the dialog stays open while Apply is pending', async () => {
  const user = userEvent.setup();
  let resolve!: (r: ActionReport) => void;
  const close = vi.fn();
  mockBackend({ object_action_preview: () => ({ ...preview, confirm_name: null }), object_action_apply: () => new Promise<ActionReport>((r) => { resolve = r; }), object_action_cancel: () => null });
  render(<ObjectActionDialog request={request} onClose={close} />);
  await user.click(screen.getByRole('button', { name: 'Preview SQL' }));
  await user.click(await screen.findByRole('button', { name: 'Apply' }));
  expect(screen.getByRole('button', { name: 'Close' })).toBeDisabled();
  await user.keyboard('{Escape}');
  expect(close).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: 'Stop' }));
  expect(callsOf('object_action_cancel')[0]?.operationId).toBe(callsOf('object_action_apply')[0]?.operationId);
  await act(async () => resolve({ ...report, outcome: 'unknown' }));
  expect(await screen.findByText(/Outcome unknown. Do not retry/)).toBeInTheDocument();
});
it('blocks preview while this connection has staged edits', async () => {
  const user = userEvent.setup();
  mockBackend({});
  useTabs.getState().open({ kind: 'object', id: objectTabId(ids.prod, object), objectRef: object, connectionId: ids.prod, title: 'orders' });
  vi.spyOn(useDataEdits.getState(), 'hasStaged').mockReturnValue(true);
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  await user.click(screen.getByRole('button', { name: 'Preview SQL' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Apply or discard staged data edits');
  expect(callsOf('object_action_preview')).toHaveLength(0);
});
it('renames canonical object tabs, drops old resources and preserves query SQL tabs', async () => {
  const refresh = vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  const old = objectTabId(ids.prod, object);
  useTabs.getState().open({ kind: 'query', id: 'q', connectionId: ids.prod, title: 'SQL' });
  useTabs.getState().open({ kind: 'object', id: old, objectRef: object, connectionId: ids.prod, title: 'orders', pinned: true });
  await refreshAfterAction({ connectionId: ids.prod, action: { kind: 'rename_table', object, name: 'next' } }, report);
  const state = useTabs.getState();
  expect(state.tabs.map((t) => t.id)).toEqual(['q', objectTabId(ids.prod, { ...object, name: 'next' })]);
  expect(state.tabs[1]).toMatchObject({ title: 'next', pinned: true });
  expect(refresh).toHaveBeenCalled();
});
it('keeps unknown objects behind explicit verification and does not rerun DDL', async () => {
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  const id = objectTabId(ids.prod, object);
  useTabs.getState().open({ kind: 'object', id, objectRef: object, connectionId: ids.prod, title: 'orders' });
  await refreshAfterAction(request, { ...report, outcome: 'unknown' });
  expect(useTabs.getState().tabs[0]).toMatchObject({ id, needsVerification: true });
});
it('exposes supported actions only and disables writes for read-only connections', () => {
  useConnections.getState().setStored([], [config(ids.prod, 'readonly', { read_only: true })]);
  const entries = objectActionItems({ id: `conn:${ids.prod}/obj`, kind: 'object', label: 'orders', objectRef: object });
  expect(entries).toHaveLength(3);
  expect(entries.every((e) => 'disabled' in e && e.disabled)).toBe(true);
  expect(objectActionItems({ id: `conn:${ids.prod}/routine`, kind: 'object', label: 'p', objectRef: { ...object, kind: 'Procedure' } })).toEqual([]);
});

it('native close cannot exit while an object action is in flight', async () => {
  let handler!: Parameters<CloseGuardWindow['onCloseRequested']>[0];
  const destroy = vi.fn().mockResolvedValue(undefined);
  await installCloseGuard({ onCloseRequested: async (h) => { handler = h; return () => {}; }, destroy });
  useDialogs.setState({ objectActionRunning: true });
  const preventDefault = vi.fn();
  await handler({ preventDefault });
  expect(preventDefault).toHaveBeenCalled();
  expect(destroy).not.toHaveBeenCalled();
  expect(useDialogs.getState().notice).toContain('object action is running');
});
