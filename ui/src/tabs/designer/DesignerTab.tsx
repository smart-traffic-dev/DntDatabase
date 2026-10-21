import { useEffect, useId, type KeyboardEvent } from 'react';
import type { ElementRef, TableDef } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useQueryDocs } from '../../store/queryTabs';
import { confirmDiscardDesign, useTableDesigns } from '../../store/tableDesigns';
import type { DesignTabRef } from '../../store/tabs';
import { addColumn } from './columnModel';
import { ColumnInspector } from './ColumnInspector';
import { ChecksGrid } from './ChecksGrid';
import { ColumnsGrid } from './ColumnsGrid';
import { CommentPanel } from './CommentPanel';
import { checksUnsupported, CHECKS_UNSUPPORTED } from './elementModel';
import { ForeignKeysGrid } from './ForeignKeysGrid';
import { IndexesGrid } from './IndexesGrid';
import { OptionsPanel } from './OptionsPanel';
import { DesignerHeader, subPanelId, subTabId } from './DesignerHeader';
import { SqlPreview } from './SqlPreview';
import './designer.css';

/** The table designer (board 10): header, the section's editor with its inspector, and the live SQL. */
export function DesignerTab({ tab }: { tab: DesignTabRef }) {
  const uid = useId();
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
  const { open, draft, render, loading, lostDraft, subTab, selection } = state;
  const lost = lostDraft !== null;
  const reopened = lost && open.design_id !== state.lostDesignId;
  const readOnlyReason = open.def.read_only_reason ?? (open.read_only ? 'the connection is read-only.' : null);
  const editable = !readOnlyReason && !lost && !loading;
  const errors = render?.errors ?? [];
  const canReview =
    !!render && errors.length === 0 && !render.error && dirty && !open.read_only && !open.def.read_only_reason && !lost && !loading;

  const edit = (recipe: (d: TableDef) => TableDef, coalesce?: string) => designs.edit(tab.id, recipe, coalesce);
  const select = (el: ElementRef | null) => designs.select(tab.id, el);
  const add = () => {
    let name: string | null = null;
    designs.edit(tab.id, (d) => {
      const next = addColumn(d, open.catalog);
      name = next.columns[next.columns.length - 1]!.name;
      return next;
    });
    if (name !== null) select({ kind: 'column', name });
  };
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
    }
  };

  const where = [connectionName, tab.scope.schema ? `${tab.scope.database} › ${tab.scope.schema}` : tab.scope.database].filter(Boolean).join(' › ');
  const engine = draft.options.engine;
  const grid = { open, draft, errors, editable, selection, onSelect: select, edit };
  const tableProblems =errors.filter((e) => e.element.kind === 'table' || e.element.kind === 'primary_key');

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
        onReview={() => useDialogs.getState().requestDialog({ kind: 'tableDesign', tabId: tab.id })}
      />
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
                edit={edit}
                onAdd={add}
              />
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
