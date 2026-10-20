import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { SchemaCacheTable } from '../../bindings';
import { installBrowserGuard } from '../../desktop/browserGuard';
import { mockBackend } from '../../test/backend';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { useObjectData } from '../../store/objectData';
import { useQueryDocs } from '../../store/queryTabs';
import { useTabs } from '../../store/tabs';
import {
  duplicateActive,
  formatActiveEditor,
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
    useConnections.setState({ info: new Map([['c-maria', { kind: 'MySql', variant: 'MariaDb', version: [11, 4, 2], version_string: '11.4.2' } as never]]) });
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
});

describe('registered actions', () => {
  /** Monaco's KeyMod / KeyCode numbers are irrelevant here: distinct tokens are enough. */
  const monaco = {
    KeyMod: { CtrlCmd: 1 << 11, Shift: 1 << 10 },
    KeyCode: { F9: 67, F12: 70, KeyR: 48, KeyF: 36, KeyU: 52, KeyD: 34 },
  };

  function register() {
    const actions: { id: string; label: string; keybindings: number[]; precondition?: string; contextMenuGroupId?: string; run(e: unknown): void }[] = [];
    const editor = { addAction: (a: (typeof actions)[number]) => void actions.push(a) };
    registerEditorCommands('tab-1', editor as never, monaco as never);
    return actions;
  }

  beforeEach(() => runFromEditor.mockClear());

  it('F9_Ctrl_R_and_Ctrl_Shift_R_run_cursor_all_selection', () => {
    const actions = register();
    const by = (id: string) => actions.find((a) => a.id === id)!;
    expect(by('dnt.runAtCursor').keybindings).toEqual([monaco.KeyCode.F9]);
    expect(by('dnt.runAll').keybindings).toEqual([monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyR]);
    expect(by('dnt.runSelection').keybindings).toEqual([monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyR]);
    expect(by('dnt.runSelection').precondition).toBe('editorHasSelection');
    by('dnt.runAtCursor').run({});
    by('dnt.runAll').run({});
    by('dnt.runSelection').run({});
    expect(runFromEditor.mock.calls).toEqual([['tab-1', 'cursor'], ['tab-1', 'all'], ['tab-1', 'selection']]);
  });

  it('every_action_is_in_the_context_menu_with_a_label', () => {
    const actions = register();
    expect(actions.map((a) => a.id)).toEqual(
      expect.arrayContaining(['dnt.formatSql', 'dnt.toUpperCase', 'dnt.toLowerCase', 'dnt.duplicate', 'dnt.openTableData', 'dnt.openTableDdl']),
    );
    for (const a of actions) {
      expect(a.label).not.toBe('');
      expect(a.contextMenuGroupId).toBe('dnt');
    }
    const keys = actions.flatMap((a) => a.keybindings);
    expect(new Set(keys).size).toBe(keys.length);
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
