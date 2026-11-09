import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { SchemaCacheTable, ServerInfo } from '../../bindings';
import { installBrowserGuard } from '../../desktop/browserGuard';
import { mockBackend } from '../../test/backend';
import { sqlCaps } from '../../test/fixtures/caps';
import { config } from '../../test/fixtures/data';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { useLayout } from '../../store/layout';
import { useObjectData } from '../../store/objectData';
import { useObjectInfo } from '../../store/objectInfo';
import { useQueryDocs } from '../../store/queryTabs';
import { useTabs } from '../../store/tabs';
import { useTree } from '../../store/tree';
import {
  bindCtrlClickInfo,
  designRelationActive,
  duplicateActive,
  showRelationInfoAt,
  formatActiveEditor,
  formatMongoActive,
  formatSql,
  gotoRelationActive,
  identifierAt,
  registerEditorCommands,
  toLowerCaseActive,
  toUpperCaseActive,
  type CommandEditor,
} from './editorCommands';

// The Monaco sources the editor relies on, read as text (Monaco itself cannot run in jsdom).
const sources = import.meta.glob(
  [
    '/node_modules/monaco-editor/esm/vs/basic-languages/{mysql/mysql,pgsql/pgsql}.js',
    '/node_modules/monaco-editor/esm/vs/editor/{editor.all,edcore.main}.js',
    '/node_modules/monaco-editor/esm/vs/editor/contrib/{comment/browser/comment,linesOperations/browser/linesOperations,readOnlyMessage/browser/contribution}.js',
  ],
  { query: '?raw', import: 'default', eager: true },
);

const runFromEditor = vi.hoisted(() => vi.fn(async () => {}));
vi.mock('../../store/runQuery', async (orig) => ({ ...(await orig<typeof import('../../store/runQuery')>()), runFromEditor }));

type Pos = { lineNumber: number; column: number };
type Rng = { startLineNumber: number; startColumn: number; endLineNumber: number; endColumn: number };

/** A string-backed editor: enough of Monaco's model for the commands. Selections are offsets. */
function fakeEditor(initial: string, sel: [number, number][] = [[0, 0]], uri?: unknown) {
  let text = initial;
  let selections = sel;
  const messages: string[] = [];
  const triggered: string[] = [];
  const posAt = (offset: number): Pos => {
    const before = text.slice(0, offset).split('\n');
    return { lineNumber: before.length, column: before[before.length - 1]!.length + 1 };
  };
  const offAt = (p: Pos) => {
    const lines = text.split('\n');
    return lines.slice(0, p.lineNumber - 1).reduce((n, l) => n + l.length + 1, 0) + p.column - 1;
  };
  const rng = (a: number, b: number): Rng => {
    const s = posAt(a);
    const e = posAt(b);
    return { startLineNumber: s.lineNumber, startColumn: s.column, endLineNumber: e.lineNumber, endColumn: e.column };
  };
  const toSel = ([a, b]: [number, number]) => ({ ...rng(a, b), isEmpty: () => a === b });
  const model = {
    uri,
    getValueInRange: (r: Rng) =>
      text.slice(offAt({ lineNumber: r.startLineNumber, column: r.startColumn }), offAt({ lineNumber: r.endLineNumber, column: r.endColumn })),
    getFullModelRange: () => rng(0, text.length),
    getLineContent: (n: number) => text.split('\n')[n - 1] ?? '',
    getWordAtPosition: (p: Pos) => {
      const line = text.split('\n')[p.lineNumber - 1] ?? '';
      for (const m of line.matchAll(/\w+/g)) {
        const s = m.index ?? 0;
        if (p.column - 1 >= s && p.column - 1 <= s + m[0].length) return { startColumn: s + 1, endColumn: s + m[0].length + 1 };
      }
      return null;
    },
    getOffsetAt: offAt,
    getPositionAt: posAt,
  };
  const editor: CommandEditor & { text(): string; undoStops: number; edits: number } = {
    getModel: () => model,
    getSelection: () => toSel(selections[0]!),
    getSelections: () => selections.map(toSel),
    getPosition: () => posAt(selections[0]![1]),
    setSelection: (r) => {
      selections = [[offAt({ lineNumber: r.startLineNumber, column: r.startColumn }), offAt({ lineNumber: r.endLineNumber, column: r.endColumn })]];
    },
    pushUndoStop: () => void editor.undoStops++,
    executeEdits: (_s, edits) => {
      editor.edits++;
      const withOffsets = edits
        .map((e) => ({ from: offAt({ lineNumber: e.range.startLineNumber, column: e.range.startColumn }), to: offAt({ lineNumber: e.range.endLineNumber, column: e.range.endColumn }), text: e.text }))
        .sort((a, b) => b.from - a.from);
      for (const e of withOffsets) text = text.slice(0, e.from) + e.text + text.slice(e.to);
    },
    trigger: (_s, id) => void triggered.push(id),
    getContribution: () => ({ showMessage: (m: string) => messages.push(m) }),
    text: () => text,
    undoStops: 0,
    edits: 0,
  };
  return { editor, messages, triggered };
}

describe('formatSql', () => {
  it('uppercases_keywords_and_indents_two_spaces', () => {
    expect(formatSql('select a,b from t where a=1', 'mysql')).toBe('SELECT\n  a,\n  b\nFROM\n  t\nWHERE\n  a = 1');
  });

  it('knows_each_dialect', () => {
    // Backtick identifiers are MySQL / MariaDB; double quotes and `::` are PostgreSQL.
    expect(formatSql('select `a` from `t`', 'mariadb')).toContain('`a`');
    expect(formatSql('select "a"::int from "t"', 'postgres')).toContain('"a"::int');
  });

  it('throws_on_delimiter_scripts', () => {
    expect(() => formatSql('DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END//\nDELIMITER ;', 'mysql')).toThrow();
  });

  it('keeps_the_whitespace_around_the_text', () => {
    expect(formatSql('  select 1  \n', 'mysql')).toBe('  SELECT\n  1  \n');
  });
});

describe('formatMongoActive', () => {
  const compact = 'db.orders.find({status:"paid"},{_id:0,customer:1,total:1}).sort({total:-1,createdAt:-1}).limit(10)';
  const lines = (...l: string[]) => l.join('\n');
  const formatted = lines(
    'db.orders.find({ status: "paid" }, { _id: 0, customer: 1, total: 1 })',
    '  .sort({ total: -1, createdAt: -1 })',
    '  .limit(10)',
  );

  it('formats_the_whole_script_as_one_undo_step', () => {
    const { editor } = fakeEditor(lines('use shop', compact, ''));
    expect(formatMongoActive(editor)).toBe(true);
    expect(editor.text()).toBe(lines('use shop', formatted, ''));
    expect(editor.edits).toBe(1);
    expect(editor.undoStops).toBe(2);
  });

  it('formats_the_statements_the_selection_touches_only', () => {
    const src = lines('db.a.find( {x:1} )', compact, 'db.b.find( {y:2} )');
    // A selection inside `.sort(…)`: its whole statement, nothing else.
    const at = src.indexOf('total:-1');
    const { editor } = fakeEditor(src, [[at, at + 5]]);
    expect(formatMongoActive(editor)).toBe(true);
    expect(editor.text()).toBe(lines('db.a.find( {x:1} )', formatted, 'db.b.find( {y:2} )'));
  });

  it('a_selection_inside_a_comment_formats_nothing', () => {
    const src = lines('db.a.find( )', '// old: db.c.find(); db.c.drop()', '/* example: {a :1} */', 'db.b.find( )');
    for (const [from, to] of [
      ['db.c.find();', 'db.c.drop()'],
      ['example', '{a :1}'],
    ]) {
      const { editor, messages } = fakeEditor(src, [[src.indexOf(from!), src.indexOf(to!) + to!.length]]);
      expect(formatMongoActive(editor)).toBe(false);
      expect(editor.text()).toBe(src);
      expect(messages).toEqual(['Nothing to format']);
    }
  });

  it('a_selection_from_a_comment_into_a_statement_formats_the_statement_only', () => {
    const src = lines('db.a.find() // keep; db.c.drop()', 'db.b.find({x :1})');
    const { editor } = fakeEditor(src, [[src.indexOf('keep;'), src.length]]);
    expect(formatMongoActive(editor)).toBe(true);
    expect(editor.text()).toBe(lines('db.a.find() // keep; db.c.drop()', 'db.b.find({ x: 1 })'));
  });

  it('formatted_text_is_left_alone', () => {
    const { editor } = fakeEditor(formatted);
    expect(formatMongoActive(editor)).toBe(true);
    expect(editor.edits).toBe(0);
  });

  it('refuses_a_broken_script_with_its_position_and_changes_nothing', () => {
    const src = lines('db.a.find()', 'db.b.find({x: 1)');
    const { editor, messages } = fakeEditor(src);
    expect(formatMongoActive(editor)).toBe(false);
    expect(editor.text()).toBe(src);
    expect(editor.edits).toBe(0);
    expect(messages).toEqual(["Cannot format: Unexpected ')' (line 2, column 16)"]);
  });

  it('a_refused_selection_reports_the_position_in_the_editor', () => {
    const src = lines('db.a.find()', '', 'db.b.find({x: "open})');
    const at = src.indexOf('x:');
    const { editor, messages } = fakeEditor(src, [[at, at + 1]]);
    expect(formatMongoActive(editor)).toBe(false);
    expect(messages).toEqual(['Cannot format: Unterminated string (line 3, column 15)']);
  });
});

describe('formatActiveEditor', () => {
  it('formats_the_whole_document_as_one_undo_step', () => {
    const { editor } = fakeEditor('select a from t; select b from u', [[0, 0]]);
    expect(formatActiveEditor(editor, 'mysql')).toBe(true);
    expect(editor.text()).toContain('SELECT\n  a\nFROM\n  t;');
    expect(editor.edits).toBe(1);
    // The edit sits between two undo stops: a single Ctrl+Z reverts it.
    expect(editor.undoStops).toBe(2);
  });

  it('formats_only_the_selection', () => {
    const src = 'select 1;\nselect a from t;\nselect 2;';
    const from = src.indexOf('select a');
    const { editor } = fakeEditor(src, [[from, from + 'select a from t;'.length]]);
    formatActiveEditor(editor, 'mysql');
    expect(editor.text().startsWith('select 1;\nSELECT\n  a\nFROM\n  t;\nselect 2;')).toBe(true);
  });

  it('leaves_unparseable_text_untouched_and_shows_a_message', () => {
    const src = 'DELIMITER //\nselect 1//';
    const { editor, messages } = fakeEditor(src);
    expect(formatActiveEditor(editor, 'mysql')).toBe(false);
    expect(editor.text()).toBe(src);
    expect(editor.edits).toBe(0);
    expect(messages[0]).toMatch(/^Cannot format this SQL/);
  });

  it('takes_the_dialect_from_the_tab_engine', () => {
    const maria: ServerInfo = { kind: 'mysql', variant: 'MariaDb', version: [11, 4, 2], version_string: '11.4.2', capabilities: sqlCaps('db') };
    useConnections.setState({ info: new Map([['c-maria', maria]]) });
    const id = useQueryDocs.getState().newQuery('c-maria', 'shop');
    const { editor } = fakeEditor('select `a` from `t`', [[0, 0]], { scheme: 'query', authority: id });
    expect(formatActiveEditor(editor)).toBe(true);
    expect(editor.text()).toContain('`a`');
    useConnections.getState().reset();
  });
});

describe('case and duplicate', () => {
  it('uppercases_the_selection', () => {
    const { editor } = fakeEditor('select id from t', [[7, 9]]);
    toUpperCaseActive(editor);
    expect(editor.text()).toBe('select ID from t');
  });

  it('uppercases_the_word_at_the_cursor_without_selection', () => {
    const { editor } = fakeEditor('select id from t', [[2, 2]]);
    toUpperCaseActive(editor);
    expect(editor.text()).toBe('SELECT id from t');
  });

  it('lowercases_each_selection_and_the_word', () => {
    const { editor } = fakeEditor('SELECT ID FROM T', [[0, 6], [11, 11]]);
    toLowerCaseActive(editor);
    expect(editor.text()).toBe('select ID from T');
  });

  it('does_nothing_without_a_word', () => {
    const { editor } = fakeEditor('a  b', [[2, 2]]);
    toUpperCaseActive(editor);
    expect(editor.edits).toBe(0);
  });

  it('duplicates_the_line_through_monaco_without_selection', () => {
    const { editor, triggered } = fakeEditor('select 1', [[3, 3]]);
    duplicateActive(editor);
    expect(triggered).toEqual(['editor.action.copyLinesDownAction']);
  });

  it('duplicates_the_selection_after_itself_and_selects_the_copy', () => {
    const { editor } = fakeEditor('a = b', [[0, 1]]);
    duplicateActive(editor);
    expect(editor.text()).toBe('aa = b');
    const sel = editor.getSelection()!;
    expect(editor.getModel()!.getValueInRange(sel)).toBe('a');
    expect(sel.startColumn).toBe(2);
  });
});

describe('identifierAt', () => {
  it.each([
    ['select * from orders where', 16, ['orders']],
    ['select * from shop.orders o', 20, ['shop', 'orders']],
    ['select * from `my db`.`order details`', 25, ['my db', 'order details']],
    ['select * from "public"."Order Items"', 30, ['public', 'Order Items']],
    ['select * from t', 15, ['t']],
  ] as [string, number, string[]][])('%s', (line, col, parts) => {
    expect(identifierAt(line, col)).toEqual(parts);
  });

  it('is_null_between_words', () => {
    expect(identifierAt('a   b', 3)).toBeNull();
  });
});

describe('go to table or view', () => {
  const tables: SchemaCacheTable[] = [
    { schema: null, name: 'orders', kind: 'Table', columns: [] },
    { schema: null, name: 'order_totals', kind: 'View', columns: [] },
    { schema: null, name: 'fn_total', kind: 'Function', columns: [] },
  ];
  const pgTables: SchemaCacheTable[] = [
    { schema: 'audit', name: 'orders', kind: 'Table', columns: [] },
    { schema: 'public', name: 'orders', kind: 'Table', columns: [] },
  ];

  beforeEach(() => {
    useCompletion.reset();
    useTabs.getState().reset();
    mockBackend({
      completion_scope: (a) => ({ database: a.database as string, tables: a.database === 'pg' ? pgTables : tables }),
      completion_objects: () => [{ schema: null, name: 'archived', kind: 'Table' }],
      list_databases: () => [{ name: 'shop' }, { name: 'other' }],
    });
  });
  afterEach(() => {
    useTabs.getState().reset();
    useObjectData.setState({ byTab: {} });
  });

  const tab = (db: string | null, conn: string | null = 'c1', dialect: 'mysql' | 'postgres' = 'mysql') => {
    const id = useQueryDocs.getState().newQuery(conn, db);
    useQueryDocs.setState((s) => ({ docs: new Map(s.docs).set(id, { ...s.docs.get(id)!, dialect }) }));
    return { scheme: 'query', authority: id };
  };

  it('F12_opens_the_table_on_its_data', async () => {
    const { editor } = fakeEditor('select * from orders', [[16, 16]], tab('shop'));
    expect(await gotoRelationActive(editor, 'data')).toBe(true);
    const opened = useTabs.getState().tabs.find((t) => t.kind === 'object');
    expect(opened).toMatchObject({ connectionId: 'c1', objectRef: { database: 'shop', schema: null, name: 'orders', kind: 'Table' } });
    expect(useObjectData.getState().byTab[opened!.id]?.sub).toBe('data');
  });

  it('Shift_F12_opens_the_ddl_of_a_view_case_insensitively', async () => {
    const { editor } = fakeEditor('select * from ORDER_TOTALS', [[16, 16]], tab('shop'));
    expect(await gotoRelationActive(editor, 'ddl')).toBe(true);
    const opened = useTabs.getState().tabs.find((t) => t.kind === 'object')!;
    expect(opened).toMatchObject({ objectRef: { name: 'order_totals', kind: 'View' } });
    expect(useObjectData.getState().byTab[opened.id]?.sub).toBe('ddl');
  });

  it('resolves_db_dot_table_in_another_database', async () => {
    const { editor } = fakeEditor('select * from other.archived', [[22, 22]], tab('shop'));
    expect(await gotoRelationActive(editor, 'data')).toBe(true);
    expect(useTabs.getState().tabs[useTabs.getState().tabs.length - 1]).toMatchObject({ objectRef: { database: 'other', name: 'archived' } });
  });

  it('postgres_prefers_the_current_schema_and_honours_a_qualifier', async () => {
    const t = tab('pg', 'c1', 'postgres');
    const plain = fakeEditor('select * from orders', [[16, 16]], t);
    await gotoRelationActive(plain.editor, 'data');
    expect(useTabs.getState().tabs.find((x) => x.kind === 'object')).toMatchObject({ objectRef: { schema: 'public', name: 'orders' } });
    useTabs.getState().reset();
    const qualified = fakeEditor('select * from audit.orders', [[22, 22]], t);
    await gotoRelationActive(qualified.editor, 'data');
    expect(useTabs.getState().tabs.find((x) => x.kind === 'object')).toMatchObject({ objectRef: { database: 'pg', schema: 'audit', name: 'orders' } });
  });

  it('shows_a_message_when_nothing_matches', async () => {
    const { editor, messages } = fakeEditor('select * from nope', [[16, 16]], tab('shop'));
    expect(await gotoRelationActive(editor, 'data')).toBe(false);
    expect(messages).toEqual(['No table or view named nope']);
    expect(useTabs.getState().tabs.some((t) => t.kind === 'object')).toBe(false);
  });

  it('does_not_open_a_function_as_a_table', async () => {
    const { editor, messages } = fakeEditor('select fn_total', [[10, 10]], tab('shop'));
    expect(await gotoRelationActive(editor, 'data')).toBe(false);
    expect(messages[0]).toBe('No table or view named fn_total');
  });

  it('says_so_without_a_connection', async () => {
    const { editor, messages } = fakeEditor('select * from orders', [[16, 16]], tab(null, null));
    expect(await gotoRelationActive(editor, 'data')).toBe(false);
    expect(messages[0]).toMatch(/^No table or view named orders/);
  });

  describe('F4 designs the table under the cursor', () => {
    const connected = (designer = true) =>
      useConnections.setState({
        summaries: new Map([['c1', config('c1', 'local')]]),
        info: new Map([['c1', { kind: 'mysql', variant: 'MySql8', version: [8, 0, 36], version_string: '8.0.36', capabilities: { ...sqlCaps('db'), designer } }]]),
      });
    afterEach(() => useConnections.getState().reset());

    it('opens_the_designer_of_the_table', async () => {
      connected();
      const { editor } = fakeEditor('select * from orders', [[16, 16]], tab('shop'));
      expect(await designRelationActive(editor)).toBe(true);
      expect(useTabs.getState().tabs.find((t) => t.kind === 'design')).toMatchObject({
        connectionId: 'c1',
        scope: { database: 'shop', schema: null },
        table: 'orders',
      });
    });

    it('a_view_is_not_designed', async () => {
      connected();
      const { editor, messages } = fakeEditor('select * from order_totals', [[16, 16]], tab('shop'));
      expect(await designRelationActive(editor)).toBe(false);
      expect(messages).toEqual(['order_totals is a view: only tables can be designed']);
      expect(useTabs.getState().tabs.some((t) => t.kind === 'design')).toBe(false);
    });

    it('says_so_without_a_designer_or_a_name', async () => {
      connected(false);
      const off = fakeEditor('select * from orders', [[16, 16]], tab('shop'));
      expect(await designRelationActive(off.editor)).toBe(false);
      expect(off.messages).toEqual(['The table designer is not available on this connection']);
      connected();
      const gap = fakeEditor('a   b', [[2, 2]], tab('shop'));
      expect(await designRelationActive(gap.editor)).toBe(false);
      expect(gap.messages).toEqual(['Put the cursor on a table name']);
    });
  });

  describe('Ctrl+click shows the Object info panel', () => {
    beforeEach(() => {
      useObjectInfo.getState().reset();
      useLayout.setState({ rightVisible: false, rightActivity: 'history' });
    });

    it('a_table_name_shows_it_and_opens_the_panel_without_a_tab', async () => {
      const { editor } = fakeEditor('select * from orders', [[0, 0]], tab('shop'));
      expect(await showRelationInfoAt(editor, { lineNumber: 1, column: 17 })).toBe(true);
      expect(useObjectInfo.getState().target).toEqual({
        connectionId: 'c1',
        object: { database: 'shop', schema: null, name: 'orders', kind: 'Table' },
      });
      expect(useLayout.getState()).toMatchObject({ rightVisible: true, rightActivity: 'info' });
      expect(useTabs.getState().tabs.some((t) => t.kind === 'object')).toBe(false);
    });

    it('a_keyword_or_a_function_does_nothing_and_says_nothing', async () => {
      const { editor, messages } = fakeEditor('select fn_total from orders', [[0, 0]], tab('shop'));
      expect(await showRelationInfoAt(editor, { lineNumber: 1, column: 3 })).toBe(false);
      expect(await showRelationInfoAt(editor, { lineNumber: 1, column: 10 })).toBe(false);
      expect(useObjectInfo.getState().target).toBeNull();
      expect(useLayout.getState().rightVisible).toBe(false);
      expect(messages).toEqual([]);
    });

    it('only_a_plain_ctrl_or_cmd_left_click_on_text_counts', async () => {
      const { editor } = fakeEditor('select * from orders', [[0, 0]], tab('shop'));
      let listener: ((e: unknown) => void) | undefined;
      const CONTENT_TEXT = 6;
      bindCtrlClickInfo({ ...editor, onMouseDown: (l) => void (listener = l as never) }, CONTENT_TEXT);
      const click = (mods: Partial<Record<'ctrlKey' | 'metaKey' | 'altKey' | 'shiftKey', boolean>>, type = CONTENT_TEXT) =>
        listener!({
          event: { ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, leftButton: true, ...mods },
          target: { type, position: { lineNumber: 1, column: 17 } },
        });
      click({});
      click({ ctrlKey: true, altKey: true });
      click({ ctrlKey: true }, 2);
      await new Promise((r) => setTimeout(r, 30));
      expect(useObjectInfo.getState().target).toBeNull();
      click({ metaKey: true });
      await vi.waitFor(() => expect(useObjectInfo.getState().target?.object?.name).toBe('orders'));
    });

    it('holding_ctrl_underlines_a_table_name_under_the_mouse_only', async () => {
      const { editor } = fakeEditor('select * from orders where id = 1', [[0, 0]], tab('shop'));
      const CONTENT_TEXT = 6;
      const on: Record<string, (e?: unknown) => void> = {};
      const sets: unknown[][] = [];
      let clears = 0;
      bindCtrlClickInfo(
        {
          ...editor,
          onMouseMove: (l) => void (on.move = l as never),
          onMouseLeave: (l) => void (on.leave = l as never),
          onDidDispose: (l) => void (on.dispose = l as never),
          createDecorationsCollection: () => ({ set: (d) => void sets.push(d), clear: () => void clears++ }),
        },
        CONTENT_TEXT,
      );
      const event = (ctrlKey: boolean) => ({ ctrlKey, metaKey: false, altKey: false, shiftKey: false, leftButton: false });
      const move = (column: number, ctrl: boolean) => on.move!({ event: event(ctrl), target: { type: CONTENT_TEXT, position: { lineNumber: 1, column } } });

      move(17, false);
      await new Promise((r) => setTimeout(r, 20));
      expect(sets).toEqual([]);
      // Ctrl pressed with the mouse on `orders`: underlined, columns 15..21.
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Control', ctrlKey: true }));
      await vi.waitFor(() => expect(sets).toHaveLength(1));
      expect(sets[0]).toEqual([
        { range: { startLineNumber: 1, startColumn: 15, endLineNumber: 1, endColumn: 21 }, options: { inlineClassName: 'dnt-table-link' } },
      ]);
      // Moving within the same name does not redo it; a column name is no link.
      move(19, true);
      move(29, true);
      await new Promise((r) => setTimeout(r, 20));
      expect(sets).toHaveLength(1);
      expect(clears).toBe(1);
      move(17, true);
      await vi.waitFor(() => expect(sets).toHaveLength(2));
      window.dispatchEvent(new KeyboardEvent('keyup', { key: 'Control', ctrlKey: false }));
      expect(clears).toBe(2);
      // Disposed: the window listeners are gone.
      on.dispose!();
      move(17, false);
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Control', ctrlKey: true }));
      await new Promise((r) => setTimeout(r, 20));
      expect(sets).toHaveLength(2);
    });

    it('a_new_tree_selection_takes_the_panel_back', async () => {
      const { editor } = fakeEditor('select * from orders', [[0, 0]], tab('shop'));
      await showRelationInfoAt(editor, { lineNumber: 1, column: 17 });
      expect(useObjectInfo.getState().target).not.toBeNull();
      useTree.getState().select('some-node');
      expect(useObjectInfo.getState().target).toBeNull();
      useTree.getState().select(undefined);
    });
  });
});

describe('registered actions', () => {
  /** Monaco's KeyMod / KeyCode numbers are irrelevant here: distinct tokens are enough. */
  const monaco = {
    KeyMod: { CtrlCmd: 1 << 11, Shift: 1 << 10 },
    KeyCode: { F4: 62, F9: 67, F12: 70, KeyR: 48, KeyF: 36, KeyU: 52, KeyD: 34 },
  };

  function register() {
    const actions: { id: string; label: string; keybindings: number[]; precondition?: string; contextMenuGroupId?: string; run(e: unknown): void }[] = [];
    const editor = { addAction: (a: (typeof actions)[number]) => void actions.push(a) };
    registerEditorCommands('tab-1', editor as never, monaco as never);
    return actions;
  }

  beforeEach(() => runFromEditor.mockClear());

  it('F9_Ctrl_R_and_Ctrl_Shift_R_run_cursor_script_selection', () => {
    const actions = register();
    const by = (id: string) => actions.find((a) => a.id === id)!;
    expect(by('dnt.runAtCursor').keybindings).toEqual([monaco.KeyCode.F9]);
    expect(by('dnt.runAll').keybindings).toEqual([monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyR]);
    expect(by('dnt.runSelection').keybindings).toEqual([monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyR]);
    expect(by('dnt.runSelection').precondition).toBe('editorHasSelection');
    by('dnt.runAtCursor').run({});
    by('dnt.runAll').run({});
    by('dnt.runSelection').run({});
    expect(runFromEditor.mock.calls).toEqual([['tab-1', 'cursor'], ['tab-1', 'script'], ['tab-1', 'selection']]);
  });

  it('every_action_is_in_the_context_menu_with_a_label', () => {
    const actions = register();
    expect(actions.map((a) => a.id)).toEqual(
      expect.arrayContaining(['dnt.formatSql', 'dnt.toUpperCase', 'dnt.toLowerCase', 'dnt.duplicate', 'dnt.openTableData', 'dnt.openTableDdl', 'dnt.designTable']),
    );
    for (const a of actions) {
      expect(a.label).not.toBe('');
      expect(a.contextMenuGroupId).toBe('dnt');
    }
    // One action per key, but for Ctrl+Shift+F: Format SQL and Format, never on together.
    const formatKey = monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyF;
    const keys = actions.filter((a) => !a.keybindings.includes(formatKey)).flatMap((a) => a.keybindings);
    expect(new Set(keys).size).toBe(keys.length);
    expect(actions.filter((a) => a.keybindings.includes(formatKey)).map((a) => [a.id, a.label, a.precondition])).toEqual([
      ['dnt.formatSql', 'Format SQL', 'editorLangId == mysql || editorLangId == pgsql'],
      ['dnt.formatMongo', 'Format', 'editorLangId == mongo'],
    ]);
  });

  it('format_in_a_mongodb_editor_runs_the_console_formatter', () => {
    const actions = register();
    const { editor } = fakeEditor('db.c.find({a:1})');
    actions.find((a) => a.id === 'dnt.formatMongo')!.run(editor);
    expect(editor.text()).toBe('db.c.find({ a: 1 })');
  });

  it('format_sql_is_hidden_in_command_editors', () => {
    const actions = register();
    // Shown (and bound) only while the editor speaks a SQL language; so are F12 / Shift+F12.
    for (const id of ['dnt.formatSql', 'dnt.openTableData', 'dnt.openTableDdl', 'dnt.designTable']) {
      expect(actions.find((a) => a.id === id)!.precondition).toBe('editorLangId == mysql || editorLangId == pgsql');
    }
  });
});

describe('browser guard and Monaco', () => {
  let uninstall: () => void;
  let host: HTMLDivElement;
  beforeEach(() => {
    uninstall = installBrowserGuard(window, { allowReload: false });
    host = document.createElement('div');
    host.innerHTML = '<div class="monaco-editor"><textarea id="m"></textarea></div>';
    document.body.appendChild(host);
  });
  afterEach(() => {
    uninstall();
    host.remove();
  });

  it.each([
    ['r', 'KeyR', { ctrlKey: true }],
    ['R', 'KeyR', { ctrlKey: true, shiftKey: true }],
    ['F9', 'F9', {}],
    ['F12', 'F12', {}],
    ['F12', 'F12', { shiftKey: true }],
    ['d', 'KeyD', { ctrlKey: true }],
    ['u', 'KeyU', { ctrlKey: true }],
    ['F', 'KeyF', { ctrlKey: true, shiftKey: true }],
  ])('the_editor_still_receives_%s_%s', (k, code, init) => {
    const seen = vi.fn();
    const m = document.getElementById('m')!;
    // Monaco's own keydown listener (its keybinding service) sits on the editor's DOM node.
    m.addEventListener('keydown', seen);
    m.dispatchEvent(new KeyboardEvent('keydown', { key: k, code, bubbles: true, cancelable: true, ...init }));
    expect(seen).toHaveBeenCalledTimes(1);
  });
});

describe('Monaco defaults the SQL editor relies on', () => {
  const read = (p: string): string => sources[`/node_modules/monaco-editor/esm/vs/${p}`] as string;

  it.each(['mysql/mysql', 'pgsql/pgsql'])('%s_toggles_comments_with_a_line_comment', (lang) => {
    // Ctrl+/ inserts `-- ` from the language configuration.
    expect(read(`basic-languages/${lang}.js`)).toMatch(/comments: \{\s*lineComment: "--"/);
  });

  it('the_editor_bundle_has_comment_and_move_lines', () => {
    // `monaco/setup.ts` bundles `edcore.main`: it must pull in the comment and line contributions.
    const all = read('editor/editor.all.js');
    expect(all).toContain('./contrib/comment/browser/comment.js');
    expect(all).toContain('./contrib/linesOperations/browser/linesOperations.js');
    expect(all).toContain('./contrib/readOnlyMessage/browser/contribution.js');
    expect(read('editor/edcore.main.js')).toContain("./editor.all.js");
    expect(read('editor/contrib/comment/browser/comment.js')).toMatch(/2048 \/\* KeyMod\.CtrlCmd \*\/ \| 90 \/\* KeyCode\.Slash \*\//);
    const lines = read('editor/contrib/linesOperations/browser/linesOperations.js');
    expect(lines).toContain('editor.action.moveLinesUpAction');
    expect(lines).toContain('editor.action.moveLinesDownAction');
    expect(lines).toMatch(/primary: 512 \/\* KeyMod\.Alt \*\/ \| 16 \/\* KeyCode\.UpArrow \*\//);
    expect(lines).toMatch(/primary: 512 \/\* KeyMod\.Alt \*\/ \| 18 \/\* KeyCode\.DownArrow \*\//);
    // The message controller used for "Cannot format" / "No table or view" messages.
    expect(read('editor/contrib/readOnlyMessage/browser/contribution.js')).toContain('messageController');
  });
});
