import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import type { DdlEditOpen, DdlEditPreview, DdlEditReport, ObjectKind, ServerInfo } from '../bindings';
import { appError, callsOf, config, Failure, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useConnections } from '../store/connections';
import { useDdlEdits } from '../store/ddlEdits';
import { useDialogs } from '../store/dialogs';
import { objectTabId, useTabs, type DdlTabRef } from '../store/tabs';
import { useTree } from '../store/tree';
import { DdlEditDialog } from './DdlEditDialog';

const TAB_ID = 'ddl/one';
const SERVER_TEXT = 'CREATE VIEW `v` AS select 1';
const EDITED = 'CREATE VIEW `v` AS select 2';
const GAP = 'Between the DROP and the CREATE the view does not exist; calls to it fail.';
const scope = { database: 'shop', schema: null };
const object = { database: 'shop', schema: null, name: 'v', kind: 'View' as const };

const opened = (over: Partial<DdlEditOpen> = {}): DdlEditOpen => ({
  edit_id: 'e1',
  text: SERVER_TEXT,
  target: { kind: 'View', database: 'shop', schema: null, name: 'v', table: null },
  object,
  server: serverInfo('MySql8'),
  env: null,
  read_only: false,
  strategies: [],
  strategy: null,
  blocked_reason: null,
  notes: [],
  ...over,
});

const preview: DdlEditPreview = {
  plan_id: 'plan',
  statements: ['DROP VIEW `shop`.`v`', EDITED],
  warnings: [GAP, 'Grants on the view are lost.'],
  checks: ['The view is compared with the text before COMMIT.'],
  transactional: false,
  gap: true,
  confirm_name: null,
  expires_in_seconds: 300,
};

const report = (over: Partial<DdlEditReport> = {}): DdlEditReport => ({
  outcome: 'applied',
  error: null,
  warnings: [],
  statements_sent: 2,
  executed: [],
  failed: null,
  object_state: 'changed',
  restore_text: null,
  object,
  text: 'CREATE VIEW `v` AS select 2 -- reread',
  ...over,
});

const objectTab = objectTabId(ids.prod, object);

async function setup(
  env: 'production' | null,
  handlers: Record<string, (a: Record<string, unknown>) => unknown>,
  plan: DdlEditPreview = preview,
  { kind = 'View', server = serverInfo('MySql8') }: { kind?: ObjectKind; server?: ServerInfo } = {},
) {
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: env })]);
  useConnections.getState().setInfo(ids.prod, server);
  mockBackend({ ddl_edit_open: () => opened({ env, server }), ddl_edit_preview: () => plan, ddl_edit_close: () => null, ...handlers });
  const tab: DdlTabRef = { id: TAB_ID, kind: 'ddl', connectionId: ids.prod, objectKind: kind, title: 'v' };
  useTabs.getState().open({ kind: 'object', id: objectTab, objectRef: object, connectionId: ids.prod, title: 'v' });
  useTabs.getState().open(tab);
  await act(async () => {
    await useDdlEdits.getState().load(TAB_ID, { connectionId: ids.prod, scope, kind, name: 'v', table: null });
  });
  useDdlEdits.getState().setText(TAB_ID, EDITED);
  const onClose = vi.fn();
  const user = userEvent.setup();
  render(<DdlEditDialog tabId={TAB_ID} onClose={onClose} />);
  return { user, onClose };
}

const edit = () => useDdlEdits.getState().byTab[TAB_ID]!;
const objectTabRef = () => useTabs.getState().tabs.find((t) => t.id === objectTab) as { resourceRevision?: number; needsVerification?: boolean };
const applyButton = () => screen.findByRole('button', { name: 'Apply' });

beforeEach(() => {
  resetStores();
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
});

it('shows_exact_statements_and_gap_warning', async () => {
  await setup(null, {});
  const sql = await screen.findByLabelText('SQL statements');
  expect(sql).toHaveTextContent('DROP VIEW `shop`.`v`');
  expect(sql).toHaveTextContent(EDITED);
  expect(callsOf('ddl_edit_preview')[0]).toMatchObject({ editId: 'e1', text: EDITED, strategy: null });
  expect(screen.getByText('Not transactional: statements run one by one')).toBeInTheDocument();
  expect(screen.getByText(GAP)).toHaveClass('danger');
  expect(screen.getByText('Grants on the view are lost.')).not.toHaveClass('danger');
  expect(screen.getByText('The view is compared with the text before COMMIT.')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Apply' })).toBeEnabled();
});

it('a transactional plan says it runs in one transaction', async () => {
  await setup(null, {}, { ...preview, transactional: true, gap: false, warnings: [] });
  expect(await screen.findByText('Runs in one transaction')).toBeInTheDocument();
});

it('production_requires_exact_name', async () => {
  const { user } = await setup('production', { ddl_edit_apply: () => report() }, { ...preview, confirm_name: 'v' });
  const apply = await applyButton();
  expect(screen.getByTestId('env-chip')).toBeInTheDocument();
  expect(screen.getByText('Type v to apply')).toBeInTheDocument();
  expect(apply).toBeDisabled();
  await user.type(screen.getByLabelText('Confirmation name'), 'V');
  expect(apply).toBeDisabled();
  await user.clear(screen.getByLabelText('Confirmation name'));
  await user.type(screen.getByLabelText('Confirmation name'), 'v');
  await user.click(apply);
  await vi.waitFor(() => expect(callsOf('ddl_edit_apply')).toHaveLength(1));
  expect(callsOf('ddl_edit_apply')[0]).toMatchObject({ planId: 'plan', confirmed: true, name: 'v' });
});

it('local_applies_on_click_and_cleans_tab', async () => {
  const { user, onClose } = await setup(null, { ddl_edit_apply: () => report() });
  await user.click(await applyButton());
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(callsOf('ddl_edit_apply')[0]).toMatchObject({ planId: 'plan', confirmed: true, name: null });
  expect(typeof callsOf('ddl_edit_apply')[0]!.operationId).toBe('string');
  expect(useDialogs.getState().notice).toBe('Changes applied.');
  expect(edit().text).toBe('CREATE VIEW `v` AS select 2 -- reread');
  expect(useDdlEdits.getState().isDirty(TAB_ID)).toBe(false);
  expect(useTree.getState().refresh).toHaveBeenCalled();
  expect(objectTabRef()).toMatchObject({ resourceRevision: 1, needsVerification: false });
  expect(useDialogs.getState().ddlEditRunning).toBe(false);
});

it('rolled_back_keeps_text', async () => {
  const rolled = report({ outcome: 'rolled_back', error: appError('query', 'syntax error at or near "selec"'), statements_sent: 2, object_state: 'unchanged', object: null, text: null });
  const { user, onClose } = await setup(null, { ddl_edit_apply: () => rolled });
  await user.click(await applyButton());
  expect(await screen.findByRole('alert')).toHaveTextContent('syntax error at or near "selec"');
  expect(screen.getByText('The changes were rolled back. Your text is kept.')).toBeInTheDocument();
  expect(onClose).not.toHaveBeenCalled();
  expect(edit().text).toBe(EDITED);
  expect(useDdlEdits.getState().isDirty(TAB_ID)).toBe(true);
  expect(useTree.getState().refresh).not.toHaveBeenCalled();
});

it('an apply error that sent nothing keeps the text like not applied', async () => {
  const { user } = await setup(null, { ddl_edit_apply: () => { throw new Failure(appError('query', 'lock timeout')); } });
  await user.click(await applyButton());
  expect(await screen.findByRole('alert')).toHaveTextContent('lock timeout');
  expect(screen.getByText('The changes were not applied. Your text is kept.')).toBeInTheDocument();
  expect(edit().text).toBe(EDITED);
});

it('partial_offers_restore', async () => {
  const partial = report({
    outcome: 'partial',
    error: appError('query', 'You have an error in your SQL syntax'),
    executed: ['DROP VIEW `shop`.`v`'],
    failed: EDITED,
    object_state: 'missing',
    restore_text: SERVER_TEXT,
    object: null,
    text: null,
  });
  const { user, onClose } = await setup(null, { ddl_edit_apply: () => partial });
  await user.click(await applyButton());
  expect(await screen.findByText('The changes were applied only in part.')).toBeInTheDocument();
  expect(screen.getByLabelText('Executed statements')).toHaveTextContent('DROP VIEW `shop`.`v`');
  expect(screen.getByLabelText('Failed statement')).toHaveTextContent(EDITED);
  expect(screen.getByText('The object no longer exists on the server.')).toBeInTheDocument();
  expect(screen.getByRole('alert')).toHaveTextContent('You have an error in your SQL syntax');
  expect(useTree.getState().refresh).toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: 'Put the original definition in the editor' }));
  expect(edit().text).toBe(SERVER_TEXT);
  expect(edit().target.name).toBeNull();
  expect(onClose).toHaveBeenCalled();
});

it('unknown_marks_object_tab', async () => {
  const unknown = report({ outcome: 'unknown', error: appError('connection_lost', 'The connection was lost.'), object_state: 'unknown', object: null, text: null });
  const { user, onClose } = await setup(null, { ddl_edit_apply: () => unknown });
  await user.click(await applyButton());
  expect(await screen.findByText('The result is unknown; check the object before continuing.')).toBeInTheDocument();
  await vi.waitFor(() => expect(objectTabRef()).toMatchObject({ needsVerification: true }));
  expect(useTree.getState().refresh).toHaveBeenCalled();
  expect(edit().lost).toBe(false);
  expect(edit().text).toBe(EDITED);
  expect(onClose).not.toHaveBeenCalled();
});

it('stale_offers_reopen', async () => {
  const { user, onClose } = await setup(null, { ddl_edit_preview: () => { throw new Failure(appError('plan_stale', 'Preview again.')); } });
  expect(await screen.findByRole('alert')).toHaveTextContent('The object changed since you opened it.');
  expect(edit().stale).toBe(true);
  await user.click(screen.getByRole('button', { name: 'Reopen' }));
  expect(onClose).toHaveBeenCalled();
  await vi.waitFor(() => expect(callsOf('ddl_edit_open')).toHaveLength(2));
  expect(edit().text).toBe(EDITED);
});

it('an expired plan offers Preview again', async () => {
  let previews = 0;
  const { user } = await setup(null, {
    ddl_edit_preview: () => ({ ...preview, plan_id: `plan-${++previews}` }),
    ddl_edit_apply: (a) => {
      if (a.planId === 'plan-1') throw new Failure(appError('plan_expired', 'The plan expired.'));
      return report();
    },
  });
  await user.click(await applyButton());
  expect(await screen.findByRole('alert')).toHaveTextContent('The plan expired.');
  await user.click(screen.getByRole('button', { name: 'Preview again' }));
  await user.click(await applyButton());
  await vi.waitFor(() => expect(callsOf('ddl_edit_apply')).toHaveLength(2));
  expect(callsOf('ddl_edit_apply')[1]).toMatchObject({ planId: 'plan-2' });
});

it('a lost edit marks the text lost and closes with a notice', async () => {
  const { onClose } = await setup(null, { ddl_edit_preview: () => { throw new Failure(appError('config', 'gone', { field: 'edit_id' })); } });
  await vi.waitFor(() => expect(onClose).toHaveBeenCalled());
  expect(edit().lost).toBe(true);
  expect(edit().text).toBe(EDITED);
  expect(useDialogs.getState().notice).toContain('connection to this object was lost');
});

it('stop_calls_cancel', async () => {
  let resolve!: (r: DdlEditReport) => void;
  const { user } = await setup(null, { ddl_edit_apply: () => new Promise<DdlEditReport>((r) => { resolve = r; }), ddl_edit_cancel: () => null });
  await user.click(await applyButton());
  expect(useDialogs.getState().ddlEditRunning).toBe(true);
  await user.click(await screen.findByRole('button', { name: 'Stop' }));
  expect(callsOf('ddl_edit_cancel')[0]?.operationId).toBe(callsOf('ddl_edit_apply')[0]?.operationId);
  await act(async () => resolve(report({ outcome: 'not_applied', statements_sent: 0, object_state: 'unchanged', object: null, text: null })));
  expect(useDialogs.getState().ddlEditRunning).toBe(false);
  expect(screen.queryByRole('button', { name: 'Stop' })).not.toBeInTheDocument();
});

it('validation_error_is_shown', async () => {
  await setup(null, { ddl_edit_preview: () => { throw new Failure(appError('config', 'The text defines view `shop`.`v2`, not `shop`.`v`.', { field: 'text' })); } });
  expect(await screen.findByRole('alert')).toHaveTextContent('The text defines view `shop`.`v2`, not `shop`.`v`.');
  expect(screen.queryByRole('button', { name: 'Apply' })).not.toBeInTheDocument();
  expect(edit().lost).toBe(false);
});

it('an unsupported feature shows the feature and version', async () => {
  const unsupported = appError('unsupported', 'This feature is not supported by the server.', { detail: 'BEGIN ATOMIC function bodies (server version PostgreSQL 14)' });
  await setup(null, { ddl_edit_preview: () => { throw new Failure(unsupported); } });
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('This feature is not supported by the server.');
  expect(alert).toHaveTextContent('BEGIN ATOMIC function bodies (server version PostgreSQL 14)');
});

it('the review shows no impacts aside', async () => {
  await setup(null, {});
  await screen.findByLabelText('SQL statements');
  expect(screen.queryByLabelText('Impact')).not.toBeInTheDocument();
  expect(screen.queryByText('None')).not.toBeInTheDocument();
});

it('Copy of a MySQL routine plan is a DELIMITER script', async () => {
  const create = "CREATE PROCEDURE `p`() BEGIN SELECT '$$'; END";
  const { user } = await setup(null, {}, { ...preview, statements: ['DROP PROCEDURE `shop`.`p`', create] }, { kind: 'Procedure' });
  await user.click(await screen.findByRole('button', { name: 'Copy' }));
  expect(await navigator.clipboard.readText()).toBe(`DELIMITER //\nDROP PROCEDURE \`shop\`.\`p\`//\n${create}//\nDELIMITER ;`);
});

it('Copy of a MySQL view plan ends statements with ;', async () => {
  const { user } = await setup(null, {});
  await user.click(await screen.findByRole('button', { name: 'Copy' }));
  expect(await navigator.clipboard.readText()).toBe(`DROP VIEW \`shop\`.\`v\`;\n${EDITED};`);
});

it('Copy of a PostgreSQL plan has a header that is not the table designer one', async () => {
  const { user } = await setup(null, {}, { ...preview, statements: ['CREATE OR REPLACE VIEW public.v AS SELECT 2'] }, { server: serverInfo('Postgres', [17, 2, 0]) });
  await user.click(await screen.findByRole('button', { name: 'Copy' }));
  const text = await navigator.clipboard.readText();
  expect(text).not.toContain('table designer');
  expect(text).toContain('-- These statements are applied in one transaction (BEGIN … COMMIT),');
  expect(text.endsWith('\nCREATE OR REPLACE VIEW public.v AS SELECT 2;')).toBe(true);
});

it('a connection staleness shows its reason and offers Reopen', async () => {
  const reason = 'The connection, policy, dialect or object changed; preview again.';
  await setup(null, { ddl_edit_preview: () => { throw new Failure(appError('plan_stale', 'The table or connection changed since the preview; review the changes again.', { detail: reason })); } });
  expect(await screen.findByRole('alert')).toHaveTextContent(reason);
  expect(screen.getByRole('alert')).not.toHaveTextContent('The object changed since you opened it.');
  expect(screen.getByRole('button', { name: 'Reopen' })).toBeInTheDocument();
});

it('an object staleness keeps the object-changed message', async () => {
  await setup(null, { ddl_edit_preview: () => { throw new Failure(appError('plan_stale', 'x', { detail: 'The object changed since the editor opened it.' })); } });
  expect(await screen.findByRole('alert')).toHaveTextContent('The object changed since you opened it.');
});