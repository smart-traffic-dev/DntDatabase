// Editing commands and shortcuts of the SQL editor: run keys (F9, Ctrl+R, Ctrl+Shift+R),
// Format SQL, case change, duplicate, and go to table / view (F12, Shift+F12). The logic is
// plain functions over a small editor surface so it is testable without Monaco; only
// `registerEditorCommands` touches Monaco's keybinding API.
import { format as formatWithSqlFormatter, type SqlLanguage } from 'sql-formatter';
import type * as Monaco from 'monaco-editor';
import type { ObjectKind, ObjectRef } from '../../bindings';
import { objectTabId, useTabs } from '../../store/tabs';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { useObjectData, type SubTab } from '../../store/objectData';
import { currentSchemaOf } from '../../query/completion/provider';
import { queryDocTarget, type CompletionTarget } from '../../query/completion/monacoProvider';
import { runFromEditor } from '../../store/runQuery';

interface Pos {
  lineNumber: number;
  column: number;
}
interface Rng {
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
}
interface Sel extends Rng {
  isEmpty(): boolean;
}

/** The part of a Monaco editor these commands use (a plain subset, so tests can fake it). */
export interface CommandEditor {
  getModel(): {
    uri?: unknown;
    getValueInRange(r: Rng): string;
    getFullModelRange(): Rng;
    getLineContent(line: number): string;
    getWordAtPosition(p: Pos): { startColumn: number; endColumn: number } | null;
    getOffsetAt(p: Pos): number;
    getPositionAt(offset: number): Pos;
  } | null;
  getSelection(): Sel | null;
  getSelections(): Sel[] | null;
  getPosition(): Pos | null;
  setSelection(r: Rng): void;
  pushUndoStop(): unknown;
  executeEdits(source: string, edits: { range: Rng; text: string }[]): unknown;
  trigger(source: string, handlerId: string, payload: unknown): void;
  getContribution?(id: string): unknown;
}

/** Shows a short non-blocking message under the cursor (Monaco's message controller). */
export function showEditorMessage(editor: CommandEditor, text: string): void {
  const controller = editor.getContribution?.('editor.contrib.messageController') as
    | { showMessage(message: string, position: Pos): void }
    | null
    | undefined;
  controller?.showMessage(text, editor.getPosition() ?? { lineNumber: 1, column: 1 });
}

// ---------------------------------------------------------------------------------------
// Format SQL
// ---------------------------------------------------------------------------------------

export type FormatDialect = 'mysql' | 'mariadb' | 'postgres';

const FORMATTER_LANGUAGE: Record<FormatDialect, SqlLanguage> = {
  mysql: 'mysql',
  mariadb: 'mariadb',
  postgres: 'postgresql',
};

/** `DELIMITER` scripts are client directives, not SQL: the formatter cannot lay them out. */
const DELIMITER_DIRECTIVE = /^[ \t]*delimiter\b/im;

/** Formats SQL text; throws when it cannot be parsed. Whitespace around the text is kept. */
export function formatSql(text: string, dialect: FormatDialect): string {
  if (DELIMITER_DIRECTIVE.test(text)) throw new Error('DELIMITER scripts cannot be formatted');
  const lead = /^\s*/.exec(text)?.[0] ?? '';
  const tail = text.trim() === '' ? '' : (/\s*$/.exec(text)?.[0] ?? '');
  if (text.trim() === '') return text;
  const formatted = formatWithSqlFormatter(text.trim(), {
    language: FORMATTER_LANGUAGE[dialect],
    keywordCase: 'upper',
    tabWidth: 2,
    useTabs: false,
  });
  return lead + formatted + tail;
}

/** The formatter dialect of an editor: MariaDB is told apart through its connection's server variant. */
function dialectOfEditor(editor: CommandEditor): FormatDialect {
  const target = queryDocTarget(editor.getModel());
  if (target?.engine === 'postgres') return 'postgres';
  const variant = target?.connId ? useConnections.getState().info.get(target.connId)?.variant : undefined;
  return variant === 'MariaDb' ? 'mariadb' : 'mysql';
}

/**
 * Format SQL (Ctrl+Shift+F): the selection if there is one, else the whole document. One
 * undo step. A script the formatter cannot parse stays untouched and a message says so.
 * Returns whether the text is formatted.
 */
export function formatActiveEditor(editor: CommandEditor, dialect?: FormatDialect): boolean {
  const model = editor.getModel();
  if (!model) return false;
  const selection = editor.getSelection();
  const range: Rng = selection && !selection.isEmpty() ? selection : model.getFullModelRange();
  const text = model.getValueInRange(range);
  let formatted: string;
  try {
    formatted = formatSql(text, dialect ?? dialectOfEditor(editor));
  } catch (e) {
    const reason = e instanceof Error ? e.message.split('\n')[0] : '';
    showEditorMessage(editor, `Cannot format this SQL${reason ? `: ${reason}` : ''}`);
    return false;
  }
  if (formatted !== text) {
    editor.pushUndoStop();
    editor.executeEdits('dnt-format', [{ range, text: formatted }]);
    editor.pushUndoStop();
  }
  return true;
}

// ---------------------------------------------------------------------------------------
// Case change, duplicate
// ---------------------------------------------------------------------------------------

function changeCase(editor: CommandEditor, to: (s: string) => string): void {
  const model = editor.getModel();
  if (!model) return;
  const edits: { range: Rng; text: string }[] = [];
  for (const sel of editor.getSelections() ?? []) {
    let range: Rng = sel;
    if (sel.isEmpty()) {
      const word = model.getWordAtPosition({ lineNumber: sel.startLineNumber, column: sel.startColumn });
      if (!word) continue;
      range = { ...sel, startColumn: word.startColumn, endColumn: word.endColumn };
    }
    const text = model.getValueInRange(range);
    const changed = to(text);
    if (changed !== text) edits.push({ range, text: changed });
  }
  if (edits.length === 0) return;
  editor.pushUndoStop();
  editor.executeEdits('dnt-case', edits);
  editor.pushUndoStop();
}

/** Ctrl+Shift+U: the selection (or the word at the cursor) in UPPER CASE. */
export function toUpperCaseActive(editor: CommandEditor): void {
  changeCase(editor, (s) => s.toUpperCase());
}

/** Ctrl+U: the selection (or the word at the cursor) in lower case. */
export function toLowerCaseActive(editor: CommandEditor): void {
  changeCase(editor, (s) => s.toLowerCase());
}

/**
 * Ctrl+D: duplicates the current line below (no selection), or the selected text right after
 * itself, selecting the copy.
 */
export function duplicateActive(editor: CommandEditor): void {
  const model = editor.getModel();
  const selections = editor.getSelections() ?? [];
  if (!model || selections.length === 0) return;
  if (selections.every((s) => s.isEmpty())) {
    editor.trigger('dnt', 'editor.action.copyLinesDownAction', null);
    return;
  }
  const edits = selections
    .filter((s) => !s.isEmpty())
    .map((s) => ({
      range: { startLineNumber: s.endLineNumber, startColumn: s.endColumn, endLineNumber: s.endLineNumber, endColumn: s.endColumn },
      text: model.getValueInRange(s),
    }));
  const single = selections.length === 1 ? selections[0]! : null;
  const start = single ? model.getOffsetAt({ lineNumber: single.endLineNumber, column: single.endColumn }) : 0;
  editor.pushUndoStop();
  editor.executeEdits('dnt-duplicate', edits);
  editor.pushUndoStop();
  if (single) {
    const length = edits[0]!.text.length;
    // `model` is live: after the edit it holds the copy at `start`..`start + length`.
    const from = model.getPositionAt(start);
    const to = model.getPositionAt(start + length);
    editor.setSelection({ startLineNumber: from.lineNumber, startColumn: from.column, endLineNumber: to.lineNumber, endColumn: to.column });
  }
}

// ---------------------------------------------------------------------------------------
// Go to table / view
// ---------------------------------------------------------------------------------------

const PART = '(?:`(?:[^`]|``)*`|"(?:[^"]|"")*"|[A-Za-z0-9_$]+)';
const QUALIFIED = new RegExp(`${PART}(?:\\.${PART})*`, 'g');

const unquote = (part: string): string => {
  const q = part[0];
  return (q === '`' || q === '"') && part.length >= 2 ? part.slice(1, -1).split(q + q).join(q) : part;
};

/** The (possibly qualified) identifier under the cursor, as unquoted parts: `shop.orders` -> ['shop', 'orders']. */
export function identifierAt(line: string, column: number): string[] | null {
  const at = column - 1;
  for (const m of line.matchAll(QUALIFIED)) {
    const start = m.index ?? 0;
    if (at >= start && at <= start + m[0].length) {
      const parts = m[0].match(new RegExp(PART, 'g')) ?? [];
      return parts.length > 0 ? parts.map(unquote) : null;
    }
  }
  return null;
}

const RELATIONS: ObjectKind[] = ['Table', 'View', 'MaterializedView'];
const eqi = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();
const isRelation = (k: ObjectKind) => RELATIONS.includes(k);

/** Finds the table or view named by `parts` (`name` or `qualifier.name`) for a tab's target. */
export async function resolveRelation(target: CompletionTarget, parts: string[]): Promise<ObjectRef | null> {
  const { connId, db, engine } = target;
  if (!connId || parts.length === 0) return null;
  const name = parts[parts.length - 1]!;
  const qualifier = parts.length >= 2 ? parts[parts.length - 2]! : null;
  const pick = <T extends { name: string; kind: ObjectKind }>(items: T[]) =>
    items.find((t) => isRelation(t.kind) && t.name === name) ?? items.find((t) => isRelation(t.kind) && eqi(t.name, name));

  if (engine === 'postgres') {
    if (!db) return null;
    const scope = await useCompletion.scope(connId, db);
    const tables = (scope?.tables ?? []).filter((t) => isRelation(t.kind) && eqi(t.name, name));
    const inSchema = qualifier ? tables.filter((t) => t.schema !== null && eqi(t.schema, qualifier)) : tables;
    const current = currentSchemaOf(scope);
    const hit =
      inSchema.find((t) => t.name === name && t.schema === current) ??
      inSchema.find((t) => t.schema === current) ??
      inSchema.find((t) => t.name === name) ??
      inSchema[0];
    if (hit) return { database: db, schema: hit.schema, name: hit.name, kind: hit.kind };
    if (!qualifier) return null;
    const other = pick((await useCompletion.objects(connId, db, engine, qualifier)) ?? []);
    return other ? { database: db, schema: other.schema ?? qualifier, name: other.name, kind: other.kind } : null;
  }

  // MySQL / MariaDB: a qualifier is a database.
  const database = qualifier ?? db;
  if (!database) return null;
  if (db && eqi(database, db)) {
    const scope = await useCompletion.scope(connId, db);
    const hit = pick(scope?.tables ?? []);
    return hit ? { database: db, schema: null, name: hit.name, kind: hit.kind } : null;
  }
  const hit = pick((await useCompletion.objects(connId, null, engine, database)) ?? []);
  return hit ? { database, schema: null, name: hit.name, kind: hit.kind } : null;
}

/** Opens (or activates) the object tab of `ref` on `sub`. */
export function openRelation(connectionId: string, ref: ObjectRef, sub: SubTab): void {
  const id = objectTabId(connectionId, ref);
  useTabs.getState().open({ id, kind: 'object', connectionId, objectRef: ref, title: ref.name });
  useObjectData.getState().setSub(id, sub);
}

/**
 * F12 (`data`) / Shift+F12 (`ddl`): opens the table or view under the cursor. Not found: a
 * message "No table or view named X". Returns whether a tab was opened.
 */
export async function gotoRelationActive(editor: CommandEditor, sub: 'data' | 'ddl'): Promise<boolean> {
  const model = editor.getModel();
  const position = editor.getPosition();
  if (!model || !position) return false;
  const parts = identifierAt(model.getLineContent(position.lineNumber), position.column);
  if (!parts) {
    showEditorMessage(editor, 'Put the cursor on a table or view name');
    return false;
  }
  const label = parts.join('.');
  const target = queryDocTarget(model);
  if (!target?.connId) {
    showEditorMessage(editor, `No table or view named ${label} (this tab has no connection)`);
    return false;
  }
  const ref = await resolveRelation(target, parts).catch(() => null);
  if (!ref) {
    showEditorMessage(editor, `No table or view named ${label}`);
    return false;
  }
  openRelation(target.connId, ref, sub);
  return true;
}

// ---------------------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------------------

interface Binding {
  id: string;
  label: string;
  keys: (m: typeof Monaco) => number[];
  /** Monaco context key expression that must hold. */
  precondition?: string;
  order: number;
  run(editor: Monaco.editor.ICodeEditor, tabId: string): void;
}

const bindings: Binding[] = [
  {
    id: 'dnt.runAtCursor',
    label: 'Run Statement at Cursor',
    keys: (m) => [m.KeyCode.F9],
    order: 1,
    run: (_e, tabId) => void runFromEditor(tabId, 'cursor'),
  },
  {
    id: 'dnt.runAll',
    label: 'Run All',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyCode.KeyR],
    order: 2,
    run: (_e, tabId) => void runFromEditor(tabId, 'all'),
  },
  {
    id: 'dnt.runSelection',
    label: 'Run Selection',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyMod.Shift | m.KeyCode.KeyR],
    precondition: 'editorHasSelection',
    order: 3,
    run: (_e, tabId) => void runFromEditor(tabId, 'selection'),
  },
  {
    id: 'dnt.formatSql',
    label: 'Format SQL',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyMod.Shift | m.KeyCode.KeyF],
    order: 4,
    run: (e) => void formatActiveEditor(e as unknown as CommandEditor),
  },
  {
    id: 'dnt.toUpperCase',
    label: 'To Upper Case',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyMod.Shift | m.KeyCode.KeyU],
    order: 5,
    run: (e) => toUpperCaseActive(e as unknown as CommandEditor),
  },
  {
    id: 'dnt.toLowerCase',
    label: 'To Lower Case',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyCode.KeyU],
    order: 6,
    run: (e) => toLowerCaseActive(e as unknown as CommandEditor),
  },
  {
    id: 'dnt.duplicate',
    label: 'Duplicate Line or Selection',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyCode.KeyD],
    order: 7,
    run: (e) => duplicateActive(e as unknown as CommandEditor),
  },
  {
    id: 'dnt.openTableData',
    label: 'Open Table or View (Data)',
    keys: (m) => [m.KeyCode.F12],
    order: 8,
    run: (e) => void gotoRelationActive(e as unknown as CommandEditor, 'data'),
  },
  {
    id: 'dnt.openTableDdl',
    label: 'Open Table or View (DDL)',
    keys: (m) => [m.KeyMod.Shift | m.KeyCode.F12],
    order: 9,
    run: (e) => void gotoRelationActive(e as unknown as CommandEditor, 'ddl'),
  },
];

/**
 * Registers the shortcuts as Monaco actions (they appear in the editor's context menu).
 * Ctrl+D replaces Monaco's "add selection to next match"; Ctrl+/ (comment, `-- `) and
 * Alt+Up / Alt+Down (move line) stay Monaco's own defaults.
 */
export function registerEditorCommands(tabId: string, editor: Monaco.editor.IStandaloneCodeEditor, monaco: typeof Monaco): void {
  for (const b of bindings) {
    editor.addAction({
      id: b.id,
      label: b.label,
      keybindings: b.keys(monaco),
      precondition: b.precondition,
      contextMenuGroupId: 'dnt',
      contextMenuOrder: b.order,
      run: (e) => b.run(e, tabId),
    });
  }
}
