// Editing commands and shortcuts of the SQL editor: run keys (F9, Ctrl+R, Ctrl+Shift+R),
// Format SQL (Format in a MongoDB editor), case change, duplicate, go to table / view (F12, Shift+F12) and Ctrl+click on a
// table or view name (Object info panel). The logic is
// plain functions over a small editor surface so it is testable without Monaco; only
// `registerEditorCommands` touches Monaco's keybinding API.
import { format as formatWithSqlFormatter, type SqlLanguage } from 'sql-formatter';
import type * as Monaco from 'monaco-editor';
import type { ObjectKind, ObjectRef } from '../../bindings';
import { SQL_EDITOR_WHEN, formatAction, isKind, type EditorFormatter } from '../../engine';
import { objectTabId, useTabs } from '../../store/tabs';
import { useCompletion } from '../../store/completion';
import { useConnections } from '../../store/connections';
import { useObjectData, type SubTab } from '../../store/objectData';
import { useObjectInfo } from '../../store/objectInfo';
import { openDesignTab } from '../../components/openObject';
import { capsOfConnection } from '../../engine/useCaps';
import { currentSchemaOf } from '../../query/completion/provider';
import { queryDocTarget, type CompletionTarget } from '../../query/completion/monacoProvider';
import { runFromEditor } from '../../store/runQuery';
import { formatMongo, formatMongoRange } from '../../query/mongo/format';
import './editorLink.css';

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
  if (target && isKind(target.engine, 'postgres')) return 'postgres';
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

/**
 * Format in a MongoDB editor (Ctrl+Shift+F): the whole statements the selection touches if
 * there is one (never a part of a comment: a selection in comments only formats nothing),
 * else the whole script. One undo step. A script that does not lex, or whose brackets do not
 * pair up, stays untouched and a message says where. Returns whether the text is formatted.
 */
export function formatMongoActive(editor: CommandEditor): boolean {
  const model = editor.getModel();
  if (!model) return false;
  const full = model.getFullModelRange();
  const all = model.getValueInRange(full);
  const selection = editor.getSelection();
  const at = (p: Pos) => model.getOffsetAt(p);
  let result: { ok: true; from: number; to: number; text: string } | { ok: false; message: string; offset: number } | null;
  if (selection && !selection.isEmpty()) {
    const from = at({ lineNumber: selection.startLineNumber, column: selection.startColumn });
    const to = at({ lineNumber: selection.endLineNumber, column: selection.endColumn });
    result = formatMongoRange(all, from, to);
  } else {
    const r = formatMongo(all);
    result = r.ok ? { ok: true, from: 0, to: all.length, text: r.text } : r;
  }
  if (!result) {
    showEditorMessage(editor, 'Nothing to format');
    return false;
  }
  if (!result.ok) {
    const p = model.getPositionAt(result.offset);
    showEditorMessage(editor, `Cannot format: ${result.message} (line ${p.lineNumber}, column ${p.column})`);
    return false;
  }
  if (result.text !== all.slice(result.from, result.to)) {
    const a = model.getPositionAt(result.from);
    const b = model.getPositionAt(result.to);
    const range: Rng = { startLineNumber: a.lineNumber, startColumn: a.column, endLineNumber: b.lineNumber, endColumn: b.column };
    editor.pushUndoStop();
    editor.executeEdits('dnt-format', [{ range, text: result.text }]);
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

/**
 * The (possibly qualified) identifier at `column` with its columns on the line (1-based,
 * `end` exclusive): `shop.orders` -> { parts: ['shop', 'orders'], start, end }.
 */
export function identifierSpanAt(line: string, column: number): { parts: string[]; start: number; end: number } | null {
  const at = column - 1;
  for (const m of line.matchAll(QUALIFIED)) {
    const start = m.index ?? 0;
    if (at >= start && at <= start + m[0].length) {
      const parts = m[0].match(new RegExp(PART, 'g')) ?? [];
      return parts.length > 0 ? { parts: parts.map(unquote), start: start + 1, end: start + m[0].length + 1 } : null;
    }
  }
  return null;
}

/** The (possibly qualified) identifier under the cursor, as unquoted parts: `shop.orders` -> ['shop', 'orders']. */
export function identifierAt(line: string, column: number): string[] | null {
  return identifierSpanAt(line, column)?.parts ?? null;
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

  if (isKind(engine, 'postgres')) {
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

/** Whether the table designer works on this connection now (open, and the engine has one). */
export const designerOn = (connectionId: string) =>
  useConnections.getState().info.has(connectionId) && (capsOfConnection(connectionId)?.designer ?? false);

/**
 * F4 in the SQL editor: opens the table designer on the table under the cursor. A view, an
 * unknown name or a connection without a designer: a message says why. Returns whether a
 * designer tab was opened.
 */
export async function designRelationActive(editor: CommandEditor): Promise<boolean> {
  const model = editor.getModel();
  const position = editor.getPosition();
  if (!model || !position) return false;
  const parts = identifierAt(model.getLineContent(position.lineNumber), position.column);
  if (!parts) {
    showEditorMessage(editor, 'Put the cursor on a table name');
    return false;
  }
  const label = parts.join('.');
  const target = queryDocTarget(model);
  if (!target?.connId) {
    showEditorMessage(editor, `No table named ${label} (this tab has no connection)`);
    return false;
  }
  if (!designerOn(target.connId)) {
    showEditorMessage(editor, 'The table designer is not available on this connection');
    return false;
  }
  const ref = await resolveRelation(target, parts).catch(() => null);
  if (!ref) {
    showEditorMessage(editor, `No table named ${label}`);
    return false;
  }
  if (ref.kind !== 'Table') {
    showEditorMessage(editor, `${ref.name} is a view: only tables can be designed`);
    return false;
  }
  openDesignTab(target.connId, { database: ref.database, schema: ref.schema }, ref.name);
  return true;
}

/**
 * Ctrl+click (Cmd+click) on a table or view name: shows it in the Object info panel of the
 * right sidebar. Anything else (a keyword, a column, an unknown name) does nothing, as a
 * Ctrl+click on a word without a definition does in any editor. Returns whether it is shown.
 */
export async function showRelationInfoAt(editor: CommandEditor, position: Pos): Promise<boolean> {
  const model = editor.getModel();
  if (!model) return false;
  const parts = identifierAt(model.getLineContent(position.lineNumber), position.column);
  const target = queryDocTarget(model);
  if (!parts || !target?.connId) return false;
  const ref = await resolveRelation(target, parts).catch(() => null);
  if (!ref) return false;
  useObjectInfo.getState().show(target.connId, ref);
  return true;
}

/** The part of a Monaco mouse event `bindCtrlClickInfo` reads. */
export interface EditorMouse {
  event: { ctrlKey: boolean; metaKey: boolean; altKey: boolean; shiftKey: boolean; leftButton: boolean };
  target: { type: number; position: Pos | null };
}
interface Decorations {
  set(decorations: { range: Rng; options: { inlineClassName: string } }[]): unknown;
  clear(): void;
}

/** The part of a Monaco editor `bindCtrlClickInfo` uses (each listener optional, for fakes). */
export type LinkEditor = CommandEditor & {
  onMouseDown?(listener: (e: EditorMouse) => void): unknown;
  onMouseMove?(listener: (e: EditorMouse) => void): unknown;
  onMouseLeave?(listener: () => void): unknown;
  onDidDispose?(listener: () => void): unknown;
  createDecorationsCollection?(): Decorations;
};

/** The class of a table or view name under the mouse while Ctrl (Cmd) is held: a link. */
export const TABLE_LINK_CLASS = 'dnt-table-link';

/**
 * Ctrl+click on a name in the text runs `showRelationInfoAt`. While Ctrl (Cmd) is held, a
 * table or view name under the mouse is underlined as a link (only names that resolve: a
 * keyword or a column never is).
 */
export function bindCtrlClickInfo(editor: LinkEditor, contentText: number): void {
  const withCtrl = (e: { ctrlKey: boolean; metaKey: boolean }) => e.ctrlKey || e.metaKey;
  editor.onMouseDown?.((e) => {
    const { event, target } = e;
    if (!withCtrl(event) || event.altKey || event.shiftKey || !event.leftButton) return;
    if (target.type !== contentText || !target.position) return;
    void showRelationInfoAt(editor, target.position);
  });

  const decorations = editor.createDecorationsCollection?.();
  if (!decorations) return;
  let hovered: Pos | null = null;
  let linked: string | null = null;
  let seq = 0;
  const unlink = () => {
    seq++;
    if (linked === null) return;
    linked = null;
    decorations.clear();
  };
  const link = (position: Pos) => {
    const model = editor.getModel();
    const span = model ? identifierSpanAt(model.getLineContent(position.lineNumber), position.column) : null;
    const target = model ? queryDocTarget(model) : null;
    if (!span || !target?.connId) return unlink();
    const key = `${position.lineNumber}:${span.start}:${span.end}`;
    if (key === linked) return;
    const mine = ++seq;
    void resolveRelation(target, span.parts)
      .catch(() => null)
      .then((ref) => {
        if (mine !== seq) return;
        if (!ref) return unlink();
        linked = key;
        const range = { startLineNumber: position.lineNumber, startColumn: span.start, endLineNumber: position.lineNumber, endColumn: span.end };
        decorations.set([{ range, options: { inlineClassName: TABLE_LINK_CLASS } }]);
      });
  };
  editor.onMouseMove?.((e) => {
    hovered = e.target.type === contentText ? e.target.position : null;
    if (hovered && withCtrl(e.event) && !e.event.altKey && !e.event.shiftKey) link(hovered);
    else unlink();
  });
  editor.onMouseLeave?.(() => {
    hovered = null;
    unlink();
  });
  // Pressing Ctrl with the mouse already on a name underlines it; releasing it removes the line.
  // Window events, not Monaco's: the editor may not have the focus, and Monaco reports the
  // Ctrl key's own keyup with `ctrlKey` still set.
  const down = (e: KeyboardEvent) => {
    if (withCtrl(e) && hovered) link(hovered);
  };
  const up = (e: KeyboardEvent) => {
    if (!withCtrl(e)) unlink();
  };
  window.addEventListener('keydown', down, true);
  window.addEventListener('keyup', up, true);
  window.addEventListener('blur', unlink);
  editor.onDidDispose?.(() => {
    window.removeEventListener('keydown', down, true);
    window.removeEventListener('keyup', up, true);
    window.removeEventListener('blur', unlink);
  });
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

/** Ctrl+Shift+F of a formatter: shown (and bound) only while the editor speaks its language. */
function formatBinding(f: EditorFormatter): Omit<Binding, 'run'> {
  const { id, label, when } = formatAction(f);
  return { id, label, keys: (m) => [m.KeyMod.CtrlCmd | m.KeyMod.Shift | m.KeyCode.KeyF], precondition: when, order: 4 };
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
    label: 'Run',
    keys: (m) => [m.KeyMod.CtrlCmd | m.KeyCode.KeyR],
    order: 2,
    // Navicat's Run: the selection if there is one, else every statement.
    run: (_e, tabId) => void runFromEditor(tabId, 'script'),
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
    ...formatBinding('sql'),
    run: (e) => void formatActiveEditor(e as unknown as CommandEditor),
  },
  {
    // The same key in a MongoDB editor (the two are never on together); none in Redis.
    ...formatBinding('mongoScript'),
    run: (e) => void formatMongoActive(e as unknown as CommandEditor),
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
    // Tables and views: SQL editors only.
    precondition: SQL_EDITOR_WHEN,
    order: 8,
    run: (e) => void gotoRelationActive(e as unknown as CommandEditor, 'data'),
  },
  {
    id: 'dnt.openTableDdl',
    label: 'Open Table or View (DDL)',
    keys: (m) => [m.KeyMod.Shift | m.KeyCode.F12],
    precondition: SQL_EDITOR_WHEN,
    order: 9,
    run: (e) => void gotoRelationActive(e as unknown as CommandEditor, 'ddl'),
  },
  {
    id: 'dnt.designTable',
    label: 'Design Table',
    keys: (m) => [m.KeyCode.F4],
    precondition: SQL_EDITOR_WHEN,
    order: 10,
    run: (e) => void designRelationActive(e as unknown as CommandEditor),
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
  // Ctrl+click on a table or view name shows it in the Object info panel.
  const contentText = monaco.editor?.MouseTargetType?.CONTENT_TEXT;
  if (contentText !== undefined) bindCtrlClickInfo(editor as unknown as LinkEditor, contentText);
}
