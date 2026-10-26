import { useDialogs } from '../store/dialogs';
import { installCloseGuard, type CloseGuardWindow } from '../store/queryTabs';
import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import type { ActionPreview, ActionReport, ObjectAction, ObjectRef } from '../bindings';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo, appError, Failure } from '../test/backend';
import { useConnections } from '../store/connections';
import { objectTabId, useTabs } from '../store/tabs';
import { useDataEdits } from '../store/dataEdits';
import { useTree } from '../store/tree';
import { ObjectActionDialog, actionTitle, refreshAfterAction } from './ObjectActionDialog';
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
it('shows the exact SQL on open and requires the exact typed name before dropping once', async () => {
  const user = userEvent.setup();
  const close = vi.fn();
  mockBackend({ object_action_preview: () => preview, object_action_apply: () => report });
  render(<ObjectActionDialog request={request} onClose={close} />);
  expect(screen.getByRole('alertdialog', { name: 'Drop table orders' })).toBeInTheDocument();
  expect(screen.getByTestId('context-bar')).toHaveTextContent('shop-prod›shop›orders');
  await waitFor(() => expect(screen.getByLabelText('SQL preview')).toHaveTextContent(preview.sql));
  expect(screen.queryByRole('button', { name: 'Preview SQL' })).not.toBeInTheDocument();
  const drop = screen.getByRole('button', { name: 'Drop table' });
  expect(drop).toHaveClass('danger');
  expect(drop).toBeDisabled();
  await user.type(screen.getByLabelText('Confirmation name'), 'Orders');
  expect(drop).toBeDisabled();
  await user.clear(screen.getByLabelText('Confirmation name'));
  await user.type(screen.getByLabelText('Confirmation name'), 'orders');
  await user.click(drop);
  await waitFor(() => expect(close).toHaveBeenCalled());
  expect(useDialogs.getState().notice).toBe('Dropped table orders.');
  expect(callsOf('object_action_preview')).toHaveLength(1);
  expect(callsOf('object_action_apply')).toHaveLength(1);
  expect(callsOf('object_action_apply')[0]).toMatchObject({ planId: 'plan', confirmed: true, name: 'orders' });
});
it('a partly finished drop stays open with its outcome and offers only Close', async () => {
  const user = userEvent.setup();
  mockBackend({ object_action_preview: () => ({ ...preview, confirm_name: null }), object_action_apply: () => ({ ...report, warnings: ['Lock wait was slow.'] }) });
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  await user.click(await screen.findByRole('button', { name: 'Drop table' }));
  expect(await screen.findByText('Operation applied.')).toBeInTheDocument();
  expect(screen.getByText('Lock wait was slow.')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Drop table' })).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Close' })).toBeEnabled();
});
it('renames from a form: the name is checked, Enter plans and applies when no typed name is needed', async () => {
  const user = userEvent.setup();
  const close = vi.fn();
  const rename: ObjectAction = { kind: 'rename_table', object, name: 'orders' };
  mockBackend({
    object_action_render: (args) => `RENAME TABLE \`orders\` TO \`${(args.action as { name: string }).name}\``,
    object_action_preview: (args) => ({ ...preview, action: args.action as ObjectAction, confirm_name: null }),
    object_action_apply: () => report,
  });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: rename }} onClose={close} />);
  const input = screen.getByLabelText('Name');
  expect(screen.getByRole('button', { name: 'Rename table' })).toBeDisabled();
  await user.clear(input);
  expect(screen.getByRole('alert')).toHaveTextContent('Enter a name.');
  await user.type(input, 'orders_archive');
  await waitFor(() => expect(screen.getByLabelText('SQL preview')).toHaveTextContent('RENAME TABLE `orders` TO `orders_archive`'));
  await user.keyboard('{Enter}');
  await waitFor(() => expect(close).toHaveBeenCalled());
  expect(callsOf('object_action_preview')[0]?.action).toMatchObject({ name: 'orders_archive' });
  expect(callsOf('object_action_apply')).toHaveLength(1);
  expect(useDialogs.getState().notice).toBe('Renamed orders to orders_archive.');
});
it('a rename that needs a typed name waits for it, and editing the name drops the plan', async () => {
  const user = userEvent.setup();
  const rename: ObjectAction = { kind: 'rename_table', object, name: 'next' };
  mockBackend({ object_action_render: () => 'RENAME', object_action_preview: () => ({ ...preview, action: rename }) });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: rename }} onClose={() => {}} />);
  await user.click(screen.getByRole('button', { name: 'Rename table' }));
  expect(await screen.findByLabelText('Confirmation name')).toBeInTheDocument();
  expect(callsOf('object_action_apply')).toHaveLength(0);
  await user.type(screen.getByLabelText('Name'), '2');
  expect(screen.queryByLabelText('Confirmation name')).not.toBeInTheDocument();
});
it('after a stale plan it offers Preview again and never retries the drop', async () => {
  const user = userEvent.setup();
  mockBackend({ object_action_preview: () => ({ ...preview, confirm_name: null }), object_action_apply: () => { throw new Failure(appError('plan_stale', 'Preview again.')); } });
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  await user.click(await screen.findByRole('button', { name: 'Drop table' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Preview again.');
  expect(screen.getByRole('button', { name: 'Preview again' })).toBeEnabled();
  expect(callsOf('object_action_apply')).toHaveLength(1);
});
const gone = () => new Failure(appError('config', 'Table orders no longer exists on the server; it may have been dropped or renamed elsewhere. Nothing was changed.', { field: 'object_missing' }));
it('a target gone at open says so, closes its tabs, refreshes the tree and offers no new preview', async () => {
  const refresh = vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  mockBackend({ object_action_preview: () => { throw gone(); } });
  useTabs.getState().open({ kind: 'query', id: 'q', connectionId: ids.prod, title: 'SQL' });
  useTabs.getState().open({ kind: 'object', id: objectTabId(ids.prod, object), objectRef: object, connectionId: ids.prod, title: 'orders' });
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  expect(await screen.findByText(/no longer exists on the server.*The tree was refreshed\./)).toBeInTheDocument();
  expect(useTabs.getState().tabs.map((t) => t.id)).toEqual(['q']);
  expect(refresh).toHaveBeenCalled();
  expect(screen.queryByRole('button', { name: 'Preview again' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Drop table' })).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Close' })).toBeEnabled();
});
it('a target gone between preview and apply (rename) closes its tab instead of renaming it', async () => {
  const user = userEvent.setup();
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  const rename: ObjectAction = { kind: 'rename_table', object, name: 'next' };
  mockBackend({ object_action_render: () => 'RENAME', object_action_preview: () => ({ ...preview, action: rename, confirm_name: null }), object_action_apply: () => { throw gone(); } });
  useTabs.getState().open({ kind: 'object', id: objectTabId(ids.prod, object), objectRef: object, connectionId: ids.prod, title: 'orders' });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: rename }} onClose={() => {}} />);
  await user.click(screen.getByRole('button', { name: 'Rename table' }));
  expect(await screen.findByText(/no longer exists on the server.*The tree was refreshed\./)).toBeInTheDocument();
  expect(useTabs.getState().tabs).toEqual([]);
  expect(callsOf('object_action_apply')).toHaveLength(1);
});
it('Stop uses the same operation id and the dialog stays open while the drop is pending', async () => {
  const user = userEvent.setup();
  let resolve!: (r: ActionReport) => void;
  const close = vi.fn();
  mockBackend({ object_action_preview: () => ({ ...preview, confirm_name: null }), object_action_apply: () => new Promise<ActionReport>((r) => { resolve = r; }), object_action_cancel: () => null });
  render(<ObjectActionDialog request={request} onClose={close} />);
  await user.click(await screen.findByRole('button', { name: 'Drop table' }));
  expect(screen.getByRole('button', { name: 'Cancel' })).toBeDisabled();
  expect(screen.getByRole('button', { name: 'Dropping…' })).toBeDisabled();
  await user.keyboard('{Escape}');
  expect(close).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: 'Stop' }));
  expect(callsOf('object_action_cancel')[0]?.operationId).toBe(callsOf('object_action_apply')[0]?.operationId);
  await act(async () => resolve({ ...report, outcome: 'unknown' }));
  expect(await screen.findByText(/Outcome unknown. Do not retry/)).toBeInTheDocument();
});
it('does not plan while this connection has staged edits', async () => {
  mockBackend({});
  useTabs.getState().open({ kind: 'object', id: objectTabId(ids.prod, object), objectRef: object, connectionId: ids.prod, title: 'orders' });
  vi.spyOn(useDataEdits.getState(), 'hasStaged').mockReturnValue(true);
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  expect(await screen.findByRole('alert')).toHaveTextContent('Apply or discard staged data edits');
  expect(callsOf('object_action_preview')).toHaveLength(0);
});
it('a read-only connection plans nothing and explains why', () => {
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { read_only: true })]);
  mockBackend({});
  render(<ObjectActionDialog request={request} onClose={() => {}} />);
  expect(screen.getByRole('button', { name: 'Drop table' })).toBeDisabled();
  expect(screen.getByTestId('context-bar')).toHaveTextContent('Read-only');
  expect(callsOf('object_action_preview')).toHaveLength(0);
});
it('names the object kind the way the tree menu does', () => {
  expect(actionTitle({ kind: 'drop_object', object: { ...object, kind: 'View', name: 'v_orders' } })).toBe('Drop view v_orders');
  expect(actionTitle({ kind: 'drop_namespace', database: 'shop', name: 'shop' })).toBe('Drop database shop');
  expect(actionTitle({ kind: 'drop_namespace', database: 'app', name: 'reporting' }, true)).toBe('Drop schema reporting');
  expect(actionTitle({ kind: 'truncate_table', object })).toBe('Truncate table orders');
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

it('rename preserves a background editor group, its active preview and the focused query', async () => {
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  const store = useTabs.getState();
  store.open({ kind: 'query', id: 'q', connectionId: ids.prod, title: 'SQL' });
  const left = useTabs.getState().focusedGroupId;
  store.setLayout('columns');
  const right = useTabs.getState().groups[1]!.id;
  store.focusGroup(right);
  const old = objectTabId(ids.prod, object);
  store.openPreview({ kind: 'object', id: old, objectRef: object, connectionId: ids.prod, title: 'orders' });
  store.focusGroup(left);
  await refreshAfterAction({ connectionId: ids.prod, action: { kind: 'rename_table', object, name: 'renamed' } }, report);
  const next = useTabs.getState();
  const renamed = objectTabId(ids.prod, { ...object, name: 'renamed' });
  expect(next.groupOf[renamed]).toBe(right);
  expect(next.groups.find((g) => g.id === right)?.activeId).toBe(renamed);
  expect(next.tabs.find((t) => t.id === renamed)).toMatchObject({ preview: true });
  expect(next.activeId).toBe('q');
  expect(next.focusedGroupId).toBe(left);
  expect(next.groupOf[old]).toBeUndefined();
});
it('drop removes an emptied group without disturbing the surviving query group', async () => {
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  const store = useTabs.getState();
  store.open({ kind: 'query', id: 'q', connectionId: ids.prod, title: 'SQL' });
  const left = useTabs.getState().focusedGroupId;
  store.setLayout('columns');
  store.focusGroup(useTabs.getState().groups[1]!.id);
  const old = objectTabId(ids.prod, object);
  store.open({ kind: 'object', id: old, objectRef: object, connectionId: ids.prod, title: 'orders' });
  await refreshAfterAction(request, report);
  expect(useTabs.getState().groups.map((g) => g.id)).toEqual([left]);
  expect(useTabs.getState().activeId).toBe('q');
  expect(useTabs.getState().tabs.map((t) => t.id)).toEqual(['q']);
});
it('truncate preserves pin and group while changing the resource revision', async () => {
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
  const id = objectTabId(ids.prod, object);
  useTabs.getState().open({ kind: 'object', id, objectRef: object, connectionId: ids.prod, title: 'orders', pinned: true });
  const group = useTabs.getState().groupOf[id];
  await refreshAfterAction({ connectionId: ids.prod, action: { kind: 'truncate_table', object } }, report);
  expect(useTabs.getState().tabs[0]).toMatchObject({ id, pinned: true, resourceRevision: 1 });
  expect(useTabs.getState().groupOf[id]).toBe(group);
  expect(useTabs.getState().groups[0]?.activeId).toBe(id);
});
