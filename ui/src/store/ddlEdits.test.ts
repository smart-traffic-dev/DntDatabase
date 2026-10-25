import { beforeEach, describe, expect, it } from 'vitest';
import type { DdlEditOpen, DdlEditReport } from '../bindings';
import { openDdlEditTab } from '../components/openDdlEdit';
import { callsOf, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useConnections } from './connections';
import { confirmDiscardDdl, useDdlEdits, type DdlTarget } from './ddlEdits';
import { useDialogs } from './dialogs';
import { useTabs } from './tabs';

const VIEW = 'CREATE VIEW `shop`.`v` AS select 1 AS `a`';
const scope = { database: 'shop', schema: null };
const target: DdlTarget = { connectionId: ids.prod, scope, kind: 'View', name: 'v', table: null };

let editSeq = 0;
let serverText = VIEW;
const opened = (): DdlEditOpen => ({
  edit_id: `e${++editSeq}`,
  text: serverText,
  target: { kind: 'View', database: 'shop', schema: null, name: 'v', table: null } as DdlEditOpen['target'],
  object: { database: 'shop', schema: null, name: 'v', kind: 'View' },
  server: serverInfo('MySql8'),
  env: null,
  read_only: false,
  strategies: [],
  strategy: null,
  blocked_reason: null,
  notes: [],
});

const store = () => useDdlEdits.getState();
const TAB = 'ddl/t1';
const state = () => store().byTab[TAB]!;

beforeEach(() => {
  resetStores();
  editSeq = 0;
  serverText = VIEW;
  mockBackend({ ddl_edit_open: () => opened(), ddl_edit_close: () => null });
});

describe('ddl edits store', () => {
  it('load_sets_base_and_text', async () => {
    await store().load(TAB, target);
    expect(state()).toMatchObject({ base: VIEW, text: VIEW, loading: false, error: null, lost: false, stale: false, target });
    expect(state().open?.edit_id).toBe('e1');
    expect(callsOf('ddl_edit_open')[0]).toEqual({ connectionId: ids.prod, scope, kind: 'View', name: 'v', table: null });
    expect(store().isDirty(TAB)).toBe(false);
  });

  it('dirty_and_revert', async () => {
    await store().load(TAB, target);
    store().setText(TAB, `${VIEW}, 2 AS b`);
    expect(store().isDirty(TAB)).toBe(true);
    store().setStrategy(TAB, 'recreate');
    expect(state().strategy).toBe('recreate');
    store().revert(TAB);
    expect(state().text).toBe(VIEW);
    expect(store().isDirty(TAB)).toBe(false);
  });

  it('reopen_keeps_text', async () => {
    await store().load(TAB, target);
    const mine = `${VIEW}, 2 AS b`;
    store().setText(TAB, mine);
    store().markStale(TAB);
    expect(state().stale).toBe(true);
    serverText = `${VIEW}, 3 AS c`;
    await store().reopen(TAB);
    expect(state()).toMatchObject({ text: mine, base: serverText, stale: false, lost: false });
    expect(state().open?.edit_id).toBe('e2');
    // The old backend edit is closed once the new one is open.
    expect(callsOf('ddl_edit_close')).toEqual([{ editId: 'e1' }]);
  });

  it('applied_cleans_and_renames', async () => {
    const id = openDdlEditTab(ids.prod, scope, 'View', null, null);
    expect(useTabs.getState().tabs[0]).toMatchObject({ id, kind: 'ddl', title: 'New view', objectKind: 'View' });
    await Promise.resolve();
    await Promise.resolve();
    store().setText(id, 'CREATE VIEW `shop`.`fresh` AS select 1');
    const report = {
      outcome: 'applied',
      error: null,
      warnings: [],
      statements_sent: 1,
      executed: [],
      failed: null,
      object_state: 'changed',
      restore_text: null,
      object: { database: 'shop', schema: null, name: 'fresh', kind: 'View' },
      text: 'CREATE VIEW `shop`.`fresh` AS select 1 AS `1`',
    } as DdlEditReport;
    store().applied(id, report);
    const s = store().byTab[id]!;
    expect(s).toMatchObject({ base: report.text, text: report.text });
    expect(s.target.name).toBe('fresh');
    expect(store().isDirty(id)).toBe(false);
    expect(useTabs.getState().tabs[0]!.title).toBe('fresh');
    // The created object's edit is now found by its target.
    expect(store().findTab({ ...target, name: 'fresh' })).toBe(id);
  });

  it('applied after a create reopens the backend edit of the created object', async () => {
    const CREATED = 'CREATE VIEW `shop`.`fresh` AS select 1 AS `1`';
    mockBackend({
      ddl_edit_open: (a) =>
        a.name === null
          ? { ...opened(), text: 'CREATE VIEW `shop`.`new_view` AS SELECT 1', object: null, strategies: [], strategy: null, notes: [] }
          : { ...opened(), text: CREATED, object: { database: 'shop', schema: null, name: 'fresh', kind: 'View' }, strategies: ['replace', 'recreate'], strategy: 'replace', notes: ['A view note.'] },
      ddl_edit_close: () => null,
    });
    await store().load(TAB, { ...target, name: null });
    expect(state().open?.object).toBeNull();
    store().setText(TAB, 'CREATE VIEW `shop`.`fresh` AS select 1');
    const report = {
      outcome: 'applied',
      error: null,
      warnings: [],
      statements_sent: 1,
      executed: [],
      failed: null,
      object_state: 'changed',
      restore_text: null,
      object: { database: 'shop', schema: null, name: 'fresh', kind: 'View' },
      text: CREATED,
    } as DdlEditReport;
    await store().applied(TAB, report);
    expect(callsOf('ddl_edit_open')[1]).toMatchObject({ name: 'fresh' });
    expect(state().open).toMatchObject({ edit_id: 'e2', strategies: ['replace', 'recreate'], strategy: 'replace', notes: ['A view note.'] });
    expect(state().open?.object?.name).toBe('fresh');
    expect(state()).toMatchObject({ text: CREATED, base: CREATED, strategy: 'replace' });
    expect(store().isDirty(TAB)).toBe(false);
    expect(callsOf('ddl_edit_close')).toEqual([{ editId: 'e1' }]);
  });

  it('applied without a re-read reopens and takes the server text', async () => {
    await store().load(TAB, target);
    store().setText(TAB, `${VIEW}, 2 AS b`);
    serverText = `${VIEW}, 2 AS \`b\``;
    const report = {
      outcome: 'applied',
      error: null,
      warnings: ['The definition could not be reloaded; reopen the editor.'],
      statements_sent: 1,
      executed: [],
      failed: null,
      object_state: 'changed',
      restore_text: null,
      object: { database: 'shop', schema: null, name: 'v', kind: 'View' },
      text: null,
    } as DdlEditReport;
    await store().applied(TAB, report);
    expect(state().open?.edit_id).toBe('e2');
    expect(state()).toMatchObject({ text: serverText, base: serverText });
    expect(store().isDirty(TAB)).toBe(false);
  });

  it('restore_makes_new_and_dirty', async () => {
    await store().load(TAB, target);
    store().restore(TAB, VIEW);
    expect(state().target.name).toBeNull();
    expect(state()).toMatchObject({ base: '', text: VIEW });
    expect(store().isDirty(TAB)).toBe(true);
    expect(store().findTab(target)).toBeNull();
  });

  it('missing_turns_the_edit_into_a_create_and_keeps_the_restore_text', async () => {
    mockBackend({ ddl_edit_open: () => ({ ...opened(), strategies: ['replace'], strategy: 'replace' }), ddl_edit_close: () => null });
    await store().load(TAB, target);
    const mine = `${VIEW}, 2 AS b`;
    store().setText(TAB, mine);
    store().missing(TAB, VIEW);
    // The text stays; the edit is a create of the same name, ready to review.
    expect(state()).toMatchObject({ base: '', text: mine, strategy: null, missing: { name: 'v', text: VIEW } });
    expect(state().target.name).toBeNull();
    expect(state().open).toMatchObject({ object: null, strategies: [], strategy: null });
    expect(store().isDirty(TAB)).toBe(true);
    expect(store().findTab(target)).toBeNull();
    // The original definition is still offered, and stays offered once put in the editor.
    store().restore(TAB, VIEW);
    expect(state()).toMatchObject({ text: VIEW, base: '', missing: { name: 'v', text: VIEW } });
    // A successful create clears it.
    await store().applied(TAB, {
      outcome: 'applied',
      error: null,
      warnings: [],
      statements_sent: 1,
      executed: [],
      failed: null,
      object_state: 'changed',
      restore_text: null,
      object: { database: 'shop', schema: null, name: 'v', kind: 'View' },
      text: VIEW,
    } as DdlEditReport);
    expect(state().missing).toBeNull();
    expect(state().target.name).toBe('v');
  });

  it('lost_on_connection_close', async () => {
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    await store().load(TAB, target);
    store().setText(TAB, 'x');
    useConnections.getState().clearInfo(ids.prod);
    expect(state()).toMatchObject({ lost: true, text: 'x' });
    expect(store().isDirty(TAB)).toBe(true);
    await store().reopen(TAB);
    expect(state()).toMatchObject({ lost: false, text: 'x', base: VIEW });
  });

  it('dispose_closes_backend_edit', async () => {
    await store().load(TAB, target);
    store().dispose(TAB);
    expect(store().byTab[TAB]).toBeUndefined();
    expect(callsOf('ddl_edit_close')).toEqual([{ editId: 'e1' }]);
  });

  it('closing the tab disposes its edit; reopening the same object reuses the tab', async () => {
    const id = openDdlEditTab(ids.prod, scope, 'View', 'v', null);
    expect(openDdlEditTab(ids.prod, scope, 'View', 'v', null)).toBe(id);
    expect(useTabs.getState().tabs).toHaveLength(1);
    // New objects always get a new tab.
    const a = openDdlEditTab(ids.prod, scope, 'Function', null, null);
    const b = openDdlEditTab(ids.prod, scope, 'Function', null, null);
    expect(a).not.toBe(b);
    expect(useTabs.getState().tabs.find((t) => t.id === a)?.title).toBe('New function');
    await new Promise((r) => setTimeout(r, 0));
    useTabs.getState().close(id);
    expect(store().byTab[id]).toBeUndefined();
    expect(callsOf('ddl_edit_close')).toHaveLength(1);
  });

  it('confirmDiscardDdl asks with the dirty count', async () => {
    await store().load(TAB, target);
    expect(await confirmDiscardDdl([TAB])).toBe(true);
    store().setText(TAB, 'x');
    const answer = confirmDiscardDdl([TAB, 'other']);
    const q = useDialogs.getState().question;
    expect(q).toMatchObject({ kind: 'discardEdits', count: 1 });
    (q as { done(d: boolean): void }).done(false);
    expect(await answer).toBe(false);
  });
});
