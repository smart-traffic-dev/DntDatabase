import { act, fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { DdlEditOpen, ObjectKind, Scope } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useDdlEdits } from '../../store/ddlEdits';
import { useDialogs } from '../../store/dialogs';
import { useTabs, type DdlTabRef } from '../../store/tabs';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { DdlEditorTab } from './DdlEditorTab';

const TAB_ID = 'ddl/one';
const VIEW = 'CREATE VIEW `v` AS select 1';

function openOf(over: Partial<DdlEditOpen> = {}): DdlEditOpen {
  return {
    edit_id: 'e1',
    text: VIEW,
    target: { kind: 'View', database: 'shop', schema: null, name: 'v', table: null },
    object: { database: 'shop', schema: null, name: 'v', kind: 'View' },
    server: serverInfo('MySql8'),
    env: null,
    read_only: false,
    strategies: [],
    strategy: null,
    blocked_reason: null,
    notes: [],
    ...over,
  };
}

/** Opens the tab's edit through the store (as `openDdlEditTab` does) and renders the editor. */
async function setup(
  opens: DdlEditOpen | DdlEditOpen[],
  { kind = 'View', name = 'v', scope = { database: 'shop', schema: null } }: { kind?: ObjectKind; name?: string | null; scope?: Scope } = {},
) {
  const queue = Array.isArray(opens) ? [...opens] : [opens];
  let last = queue[0]!;
  mockBackend({
    ddl_edit_open: () => {
      last = queue.shift() ?? last;
      return last;
    },
    ddl_edit_close: () => null,
  });
  const tab: DdlTabRef = { id: TAB_ID, kind: 'ddl', connectionId: ids.prod, objectKind: kind, title: name ?? 'New view' };
  useTabs.getState().open(tab);
  await act(async () => {
    await useDdlEdits.getState().load(TAB_ID, { connectionId: ids.prod, scope, kind, name, table: null });
  });
  return render(<DdlEditorTab tab={tab} />);
}

const editor = () => screen.getByTestId('monaco-mock') as HTMLTextAreaElement;
const review = () => screen.getByRole('button', { name: 'Review & apply…' });
const revert = () => screen.getByRole('button', { name: 'Revert' });

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
});

describe('DdlEditorTab', () => {
  it('renders_text_and_header', async () => {
    await setup(openOf({ notes: ['MySQL has no CREATE OR REPLACE PROCEDURE.'] }));
    expect(editor().value).toBe(VIEW);
    expect(editor()).toHaveAttribute('data-language', 'mysql');
    expect(screen.getByText('View · v')).toBeInTheDocument();
    expect(screen.getByText('shop-prod › shop · MySQL 8.0.36')).toBeInTheDocument();
    expect(screen.getByText('MySQL has no CREATE OR REPLACE PROCEDURE.')).toBeInTheDocument();
    // No load of its own on mount: the opener already loaded the edit.
    expect(callsOf('ddl_edit_open')).toHaveLength(1);
    // Typing updates the store.
    fireEvent.change(editor(), { target: { value: 'CREATE VIEW `v` AS select 2' } });
    expect(useDdlEdits.getState().byTab[TAB_ID]!.text).toBe('CREATE VIEW `v` AS select 2');
  });

  it('shows_the_schema_and_postgres_language', async () => {
    await setup(
      openOf({
        server: serverInfo('Postgres', [17, 2, 0]),
        target: { kind: 'MaterializedView', database: 'shop', schema: 'sales', name: 'mv', table: null },
      }),
      { kind: 'MaterializedView', name: 'mv', scope: { database: 'shop', schema: 'sales' } },
    );
    expect(screen.getByText('Materialized view · mv')).toBeInTheDocument();
    expect(screen.getByText('shop-prod › shop.sales · PostgreSQL 17.2.0')).toBeInTheDocument();
    expect(editor()).toHaveAttribute('data-language', 'pgsql');
  });

  it('strategy_switch_only_when_choices', async () => {
    const user = userEvent.setup();
    const { unmount } = await setup(openOf({ strategies: ['replace'], strategy: 'replace' }));
    expect(screen.queryByRole('radiogroup', { name: 'Strategy' })).not.toBeInTheDocument();
    unmount();
    resetStores();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    await setup(openOf({ server: serverInfo('Postgres', [17, 2, 0]), strategies: ['replace', 'recreate'], strategy: 'replace' }));
    const replace = screen.getByRole('radio', { name: 'Replace' });
    const recreate = screen.getByRole('radio', { name: 'Drop and re-create' });
    expect(replace).toHaveAttribute('aria-checked', 'true');
    expect(recreate).toHaveAttribute('aria-checked', 'false');
    await user.click(recreate);
    expect(useDdlEdits.getState().byTab[TAB_ID]!.strategy).toBe('recreate');
    expect(recreate).toHaveAttribute('aria-checked', 'true');
  });

  it('review_disabled_until_dirty_on_edit', async () => {
    const user = userEvent.setup();
    await setup(openOf());
    expect(review()).toBeDisabled();
    expect(revert()).toBeDisabled();
    fireEvent.change(editor(), { target: { value: 'CREATE VIEW `v` AS select 2' } });
    expect(review()).toBeEnabled();
    expect(revert()).toBeEnabled();
    await user.click(review());
    expect(useDialogs.getState().request).toEqual({ kind: 'ddlEdit', tabId: TAB_ID });
  });

  it('ctrl_enter_opens_the_review', async () => {
    await setup(openOf());
    fireEvent.keyDown(editor(), { key: 'Enter', ctrlKey: true });
    expect(useDialogs.getState().request).toBeNull();
    fireEvent.change(editor(), { target: { value: 'CREATE VIEW `v` AS select 2' } });
    fireEvent.keyDown(editor(), { key: 'Enter', ctrlKey: true });
    expect(useDialogs.getState().request).toEqual({ kind: 'ddlEdit', tabId: TAB_ID });
  });

  it('new_object_review_enabled', async () => {
    const template = 'CREATE VIEW `shop`.`new_view` AS\nSELECT 1';
    await setup(openOf({ text: template, object: null, target: { kind: 'View', database: 'shop', schema: null, name: null, table: null } }), { name: null });
    expect(screen.getByText('View · new')).toBeInTheDocument();
    expect(editor().value).toBe(template);
    expect(review()).toBeEnabled();
    expect(revert()).toBeDisabled();
  });

  it('blocked_reason_banner_and_read_only_editor', async () => {
    await setup(openOf({ read_only: true, blocked_reason: 'The connection is read-only.' }));
    expect(screen.getByText('The connection is read-only.')).toBeInTheDocument();
    expect(editor()).toHaveAttribute('data-read-only', 'true');
    expect(review()).toBeDisabled();
  });

  it('read_only_connection_disables_review', async () => {
    await setup(openOf({ read_only: true }));
    expect(editor()).toHaveAttribute('data-read-only', 'false');
    fireEvent.change(editor(), { target: { value: 'CREATE VIEW `v` AS select 2' } });
    expect(review()).toBeDisabled();
  });

  it('revert_restores_base', async () => {
    const user = userEvent.setup();
    await setup(openOf());
    fireEvent.change(editor(), { target: { value: 'CREATE VIEW `v` AS select 2' } });
    await user.click(revert());
    expect(editor().value).toBe(VIEW);
    expect(useDdlEdits.getState().isDirty(TAB_ID)).toBe(false);
    expect(revert()).toBeDisabled();
  });

  it('stale_banner_reopen_keeps_text', async () => {
    const user = userEvent.setup();
    const changed = 'CREATE VIEW `v` AS select 42';
    await setup([openOf(), openOf({ edit_id: 'e2', text: changed })]);
    const mine = 'CREATE VIEW `v` AS select 2';
    fireEvent.change(editor(), { target: { value: mine } });
    act(() => useDdlEdits.getState().markStale(TAB_ID));
    expect(
      screen.getByText('The object changed on the server since you opened it. Your text is kept; review it before applying.'),
    ).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Reopen' }));
    await act(async () => undefined);
    expect(callsOf('ddl_edit_open')).toHaveLength(2);
    expect(screen.queryByText(/changed on the server/)).not.toBeInTheDocument();
    expect(editor().value).toBe(mine);
    expect(useDdlEdits.getState().byTab[TAB_ID]!.base).toBe(changed);
  });

  it('lost_banner_reopen_and_review_disabled', async () => {
    const user = userEvent.setup();
    await setup([openOf(), openOf({ edit_id: 'e2' })]);
    fireEvent.change(editor(), { target: { value: 'CREATE VIEW `v` AS select 2' } });
    act(() => useDdlEdits.getState().markLost(TAB_ID));
    expect(screen.getByText('The connection to this object was lost. Your text is kept.')).toBeInTheDocument();
    expect(review()).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Reopen' }));
    await act(async () => undefined);
    expect(screen.queryByText(/was lost/)).not.toBeInTheDocument();
    expect(editor().value).toBe('CREATE VIEW `v` AS select 2');
    expect(review()).toBeEnabled();
  });

  it('a_failed_open_shows_the_error_and_retries', async () => {
    const user = userEvent.setup();
    let calls = 0;
    mockBackend({
      ddl_edit_open: () => {
        calls += 1;
        if (calls === 1) throw new Error('boom');
        return openOf();
      },
      ddl_edit_close: () => null,
    });
    const tab: DdlTabRef = { id: TAB_ID, kind: 'ddl', connectionId: ids.prod, objectKind: 'View', title: 'v' };
    useTabs.getState().open(tab);
    await act(async () => {
      await useDdlEdits.getState().load(TAB_ID, { connectionId: ids.prod, scope: { database: 'shop', schema: null }, kind: 'View', name: 'v', table: null });
    });
    render(<DdlEditorTab tab={tab} />);
    expect(screen.getByRole('alert')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Retry' }));
    await act(async () => undefined);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(editor().value).toBe(VIEW);
  });
});
