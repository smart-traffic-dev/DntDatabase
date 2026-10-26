import { useEffect, useId, useState, type KeyboardEvent } from 'react';
import type { ElementRef, TableDef } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useQueryDocs } from '../../store/queryTabs';
import { confirmDiscardDesign, describeChanges, useTableDesigns } from '../../store/tableDesigns';
import type { DesignTabRef } from '../../store/tabs';
import { addColumn, dropColumn, moveColumn, togglePrimaryKey } from './columnModel';
import { DesignerToolbar, ICONS, type ToolAction } from './DesignerToolbar';
import { ColumnInspector } from './ColumnInspector';
import { ChecksGrid } from './ChecksGrid';
import { ColumnsGrid } from './ColumnsGrid';
import { CommentPanel } from './CommentPanel';
import { ADD, checksUnsupported, CHECKS_UNSUPPORTED, KIND_OF, removeElement, type ElementKey } from './elementModel';
import type { DesignSubTab } from '../../store/tableDesigns';

const ELEMENT_KEY: Partial<Record<DesignSubTab, ElementKey>> = { indexes: 'indexes', foreign_keys: 'foreign_keys', checks: 'checks' };
const NOUNS: Record<ElementKey, string> = { indexes: 'index', foreign_keys: 'foreign key', checks: 'check' };
import { ForeignKeysGrid } from './ForeignKeysGrid';
import { IndexesGrid } from './IndexesGrid';
import { OptionsPanel } from './OptionsPanel';
import { DesignerHeader, subPanelId, subTabId } from './DesignerHeader';
import { SqlPreview } from './SqlPreview';
import './designer.css';

/** The changes of the draft a stale apply left behind, read-only (for comparison, never restored wholesale). */
function PreviousDraft({ lines }: { lines: string[] }) {
  return (
    <section className="dz-previous" aria-label="Previous draft">
      <strong className="dz-caption">Your previous changes</strong>
      {lines.length === 0 ? (
        <span className="dz-subtle">No changes.</span>
      ) : (
        <ul>
          {lines.map((line, i) => (
            <li key={i}>{line}</li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** The table designer (board 10): header, the section's editor with its inspector, and the live SQL. */
export function DesignerTab({ tab }: { tab: DesignTabRef }) {
  const uid = useId();
  const [showPrevious, setShowPrevious] = useState(false);
  const state = useTableDesigns((s) => s.byTab[tab.id]);
  const dirty = useTableDesigns((s) => s.isDirty(tab.id));
  const changes = useTableDesigns((s) => s.changeCount(tab.id));
  const connectionName = useConnections((s) => s.summaries.get(tab.connectionId)?.config.name);
  const started = state !== undefined;
  useEffect(() => {
    // Loaded once per tab; a tab moved between groups remounts and keeps its draft.
    if (!started) void useTableDesigns.getState().load(tab.id, tab.connectionId, tab.scope, tab.table);
  }, [started, tab.id, tab.connectionId, tab.scope, tab.table]);

  if (!state?.open || !state.draft) {
    return (
      <div className="designer dz-empty-state" data-testid="designer-tab">
        {state?.error && !state.loading ? (
          <div role="alert" className="dz-banner dz-banner-error">
            <span>{state.error}</span>
            <button type="button" className="btn small" onClick={() => void useTableDesigns.getState().load(tab.id, tab.connectionId, tab.scope, tab.table)}>
              Retry
            </button>
          </div>
        ) : (
          <span className="dz-subtle">Loading table design…</span>
        )}
      </div>
    );
  }

  const designs = useTableDesigns.getState();
  const { open, draft, render, loading, lostDraft, subTab, selection, staleDraft } = state;
  const lost = lostDraft !== null;
  const reopened = lost && open.design_id !== state.lostDesignId;
  const readOnlyReason = open.def.read_only_reason ?? (open.read_only ? 'the connection is read-only.' : null);
  const editable = !readOnlyReason && !lost && !loading;
  const errors = render?.errors ?? [];
  // A new table can be created as it opens (its starter `id` column is already a table).
  const isNew = open.def.original_name === null;
  const canReview =
    !!render && errors.length === 0 && !render.error && (dirty || isNew) && !open.read_only && !open.def.read_only_reason && !lost && !loading;

  const edit = (recipe: (d: TableDef) => TableDef, coalesce?: string) => designs.edit(tab.id, recipe, coalesce);
  const select = (el: ElementRef | null) => designs.select(tab.id, el);
  /** Adds a column at `at` (the end by default) and selects it. */
  const add = (at?: number) => {
    let name: string | null = null;
    designs.edit(tab.id, (d) => {
      const pos = at ?? d.columns.length;
      const next = addColumn(d, open.catalog, pos);
      name = next.columns[pos]!.name;
      return next;
    });
    if (name !== null) select({ kind: 'column', name });
  };
  const review = () => useDialogs.getState().requestDialog({ kind: 'tableDesign', tabId: tab.id });
  const openInQuery = (sql: string) => {
    const docs = useQueryDocs.getState();
    const id = docs.newQuery(tab.connectionId, tab.scope.database);
    docs.update(id, { sql });
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
    if (e.key.toLowerCase() === 'z') {
      e.preventDefault();
      if (lost) return;
      if (e.shiftKey) designs.redo(tab.id);
      else designs.undo(tab.id);
    } else if (e.key === 'Enter' && subTab === 'columns' && editable) {
      e.preventDefault();
      add();
    } else if (e.key.toLowerCase() === 's' && !e.shiftKey) {
      e.preventDefault();
      if (canReview) review();
    }
  };

  const where = [connectionName, tab.scope.schema ? `${tab.scope.database} › ${tab.scope.schema}` : tab.scope.database].filter(Boolean).join(' › ');
  const engine = draft.options.engine;
  const grid = { open, draft, errors, editable, selection, onSelect: select, edit };
  // The name error also marks the header's name field, whose message is only a tooltip: listed
  // here too (but on Options, which shows it), so a disabled "Review & apply" always shows why.
  const tableProblems = errors.filter(
    (e) => (e.element.kind === 'table' && (e.field !== 'name' || subTab !== 'options')) || e.element.kind === 'primary_key',
  );
  const nameError = errors.find((e) => e.element.kind === 'table' && e.field === 'name')?.message;

  // Toolbar: the selected column's actions (Columns), undo / redo everywhere.
  const pg = open.server.kind === 'postgres';
  const at = selection?.kind === 'column' ? draft.columns.findIndex((c) => c.name === selection.name) : -1;
  const current = at >= 0 ? draft.columns[at] : undefined;
  const pk = draft.primary_key;
  const noColumn = !current ? 'Select a column first.' : current.locked ? 'This column is locked.' : false;
  const columnTools: ToolAction[] = subTab !== 'columns' ? [] : [
    { label: 'Add column', icon: ICONS.add, shortcut: 'Ctrl+Enter', disabled: !editable, onClick: () => add() },
    // PostgreSQL appends a column added to an existing table, wherever the draft put it.
    { label: 'Insert column', icon: ICONS.insert, disabled: !editable || (pg && !isNew && 'PostgreSQL adds new columns at the end.') || (!current && 'Select the column to insert before.'), onClick: () => add(at) },
    { label: 'Delete column', icon: ICONS.delete, shortcut: 'Delete', disabled: !editable || noColumn, onClick: () => { edit((d) => dropColumn(d, at)); select(null); } },
  ];
  const keyTools: ToolAction[] = subTab !== 'columns' ? [] : [
    { label: 'Primary key', icon: ICONS.key, pressed: !!current && !!pk?.columns.includes(current.name), disabled: !editable || noColumn || (!!pk?.locked && 'The primary key is locked.'), onClick: () => current && edit((d) => togglePrimaryKey(d, current.name)) },
  ];
  const cannotMove = pg ? 'PostgreSQL cannot reorder columns.' : noColumn;
  const moveTools: ToolAction[] = subTab !== 'columns' ? [] : [
    { label: 'Move up', icon: ICONS.up, shortcut: 'Ctrl+↑', disabled: !editable || cannotMove || at <= 0, onClick: () => edit((d) => moveColumn(d, at, at - 1)) },
    { label: 'Move down', icon: ICONS.down, shortcut: 'Ctrl+↓', disabled: !editable || cannotMove || at >= draft.columns.length - 1, onClick: () => edit((d) => moveColumn(d, at, at + 1)) },
  ];
  // Indexes, Foreign keys, Checks: Add / Delete of the selected element, as Columns has.
  const listKey = ELEMENT_KEY[subTab];
  const elementTools: ToolAction[] = [];
  if (listKey) {
    const kind = KIND_OF[listKey];
    const noun = NOUNS[listKey];
    const list = draft[listKey] as { name: string; locked: boolean }[];
    const index = selection?.kind === kind ? list.findIndex((e) => e.name === selection.name) : -1;
    const item = index >= 0 ? list[index] : undefined;
    elementTools.push(
      {
        label: `Add ${noun}`,
        icon: ICONS.add,
        disabled: !editable,
        onClick: () => {
          const next = (ADD[listKey](draft)[listKey] as { name: string }[]).at(-1);
          edit((d) => ADD[listKey](d));
          if (next) select({ kind, name: next.name } as ElementRef);
        },
      },
      {
        label: `Delete ${noun}`,
        icon: ICONS.delete,
        shortcut: 'Delete',
        disabled: !editable || (!item ? `Select the ${noun} to delete.` : item.locked ? `This ${noun} is locked.` : false),
        onClick: () => {
          edit((d) => removeElement(d, listKey, index));
          select(null);
        },
      },
    );
  }
  const historyTools: ToolAction[] = [
    { label: 'Undo', icon: ICONS.undo, shortcut: 'Ctrl+Z', disabled: lost || state.past.length === 0, onClick: () => designs.undo(tab.id) },
    { label: 'Redo', icon: ICONS.redo, shortcut: 'Ctrl+Shift+Z', disabled: lost || state.future.length === 0, onClick: () => designs.redo(tab.id) },
  ];

  return (
    <div className="designer" data-testid="designer-tab" onKeyDown={onKeyDown}>
      {lost ? (
        <div className="dz-banner dz-banner-warning" role="status">
          <span>The connection to this table was lost.</span>
          {reopened ? (
            <button type="button" className="btn small" disabled={loading} onClick={() => designs.restoreLostDraft(tab.id)}>
              Restore my changes
            </button>
          ) : (
            <button type="button" className="btn small" disabled={loading} onClick={() => void designs.reopen(tab.id)}>
              Reopen
            </button>
          )}
        </div>
      ) : null}
      {staleDraft ? (
        <div className="dz-banner dz-banner-warning" role="status">
          <span>The table changed. Your previous draft is kept for comparison.</span>
          <button type="button" className="btn small" aria-expanded={showPrevious} onClick={() => setShowPrevious((v) => !v)}>
            {showPrevious ? 'Hide previous draft' : 'Show previous draft'}
          </button>
          <button
            type="button"
            className="btn small"
            onClick={() => {
              setShowPrevious(false);
              designs.setStaleDraft(tab.id, null);
            }}
          >
            Dismiss
          </button>
        </div>
      ) : null}
      {staleDraft && showPrevious ? <PreviousDraft lines={describeChanges(state.staleBase ?? open.def, staleDraft)} /> : null}
      {state.error ? (
        <div className="dz-banner dz-banner-error" role="alert">
          <span>{state.error}</span>
        </div>
      ) : null}
      {readOnlyReason ? (
        <div className="dz-banner dz-banner-info" role="note">
          <span>This table is shown read-only: {readOnlyReason}</span>
        </div>
      ) : null}
      <DesignerHeader
        uid={uid}
        name={draft.name}
        nameError={nameError}
        nameEditable={editable}
        focusName={isNew}
        onRename={(text, key) => edit((d) => ({ ...d, name: text }), key)}
        context={engine ? `${where} · ${engine}` : where}
        counts={{ columns: draft.columns.length, indexes: draft.indexes.length, foreign_keys: draft.foreign_keys.length, checks: draft.checks.length }}
        unavailable={checksUnsupported(open.server) ? { checks: CHECKS_UNSUPPORTED } : undefined}
        subTab={subTab}
        onSubTab={(sub) => designs.setSubTab(tab.id, sub)}
        changes={changes}
        dirty={dirty}
        // A lost design is discarded only once reopened (there is nothing to go back to before).
        canDiscard={dirty && !loading && (!lost || reopened)}
        canReview={canReview}
        onDiscard={() => void confirmDiscardDesign(tab.id).then((ok) => ok && useTableDesigns.getState().discard(tab.id))}
        onReview={review}
      />
      <DesignerToolbar groups={[columnTools, keyTools, moveTools, elementTools, historyTools]} />
      <div className="dz-main">
        <div className="dz-left">
          <div role="tabpanel" id={subPanelId(uid)} aria-labelledby={subTabId(uid, subTab)} className="dz-panel">
            {subTab === 'columns' ? (
              <ColumnsGrid
                open={open}
                draft={draft}
                errors={errors}
                editable={editable}
                selection={selection}
                onSelect={select}
                edit={edit}              />
            ) : null}
            {subTab === 'indexes' ? <IndexesGrid {...grid} /> : null}
            {subTab === 'foreign_keys' ? <ForeignKeysGrid {...grid} connectionId={tab.connectionId} scope={tab.scope} /> : null}
            {subTab === 'checks' ? <ChecksGrid {...grid} /> : null}
            {subTab === 'options' ? <OptionsPanel open={open} draft={draft} errors={errors} editable={editable} edit={edit} /> : null}
            {subTab === 'comment' ? <CommentPanel draft={draft} errors={errors} editable={editable} edit={edit} /> : null}
          </div>
          {tableProblems.length > 0 ? (
            <ul className="dz-problems">
              {tableProblems.map((p, i) => (
                <li key={i}>{p.message}</li>
              ))}
            </ul>
          ) : null}
          <SqlPreview statements={render?.statements ?? []} impacts={render?.impacts ?? []} error={render?.error}
            live
            engine={open.server.kind}
            onOpenInQuery={openInQuery}
          />
        </div>
        {subTab === 'columns' ? (
          <ColumnInspector open={open} draft={draft} errors={errors} editable={editable} selection={selection} onSelect={select} edit={edit} />
        ) : null}
      </div>
    </div>
  );
}
