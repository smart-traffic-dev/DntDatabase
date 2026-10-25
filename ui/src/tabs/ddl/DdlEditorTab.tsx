import { useEffect, useRef, type KeyboardEvent } from 'react';
import Editor, { type OnMount } from '@monaco-editor/react';
import type { ObjectKind, Strategy } from '../../bindings';
import { shortVersion } from '../../components/statusFormat';
import { useDntMonacoTheme } from '../../monaco/dntTheme';
import { editorFontOptions } from '../../monaco/fonts';
import { useConnections } from '../../store/connections';
import { useDdlEdits } from '../../store/ddlEdits';
import { useDialogs } from '../../store/dialogs';
import type { DdlTabRef } from '../../store/tabs';
import './ddlEditor.css';

const KIND_LABELS: Partial<Record<ObjectKind, string>> = {
  View: 'View',
  MaterializedView: 'Materialized view',
  Function: 'Function',
  Procedure: 'Procedure',
  Trigger: 'Trigger',
  Event: 'Event',
};

const STRATEGY_LABELS: Record<Strategy, string> = {
  replace: 'Replace',
  recreate: 'Drop and re-create',
};

/**
 * The DDL editor tab: one header row (what and where, the strategy, Revert, Review & apply…),
 * the notes and banners, and the edited text in Monaco. The edit is loaded by its opener
 * (`openDdlEditTab`); this tab only reopens it.
 */
export function DdlEditorTab({ tab }: { tab: DdlTabRef }) {
  const monacoTheme = useDntMonacoTheme();
  const state = useDdlEdits((s) => s.byTab[tab.id]);
  const dirty = useDdlEdits((s) => s.isDirty(tab.id));
  const running = useDialogs((s) => s.ddlEditRunning);
  const connectionName = useConnections((s) => s.summaries.get(tab.connectionId)?.config.name);

  const open = state?.open ?? null;
  const blocked = open?.blocked_reason ?? null;
  const isEdit = state?.target.name != null;
  const canReview = !!state && !!open && !state.loading && !state.lost && !blocked && !open.read_only && !running && (!isEdit || dirty);

  // Ctrl+Enter is bound once at mount: it reads the current state through this ref.
  const review = useRef<() => void>(() => undefined);
  useEffect(() => {
    review.current = () => {
      if (canReview) useDialogs.getState().requestDialog({ kind: 'ddlEdit', tabId: tab.id });
    };
  }, [canReview, tab.id]);

  if (!state || !open) {
    return (
      <div className="ddl-editor ddl-empty" data-testid="ddl-editor">
        {state?.error && !state.loading ? (
          <div role="alert" className="ddl-banner ddl-banner-error">
            <span>{state.error}</span>
            <button type="button" className="btn small" onClick={() => void useDdlEdits.getState().reopen(tab.id)}>
              Retry
            </button>
          </div>
        ) : (
          <span className="ddl-subtle">Loading…</span>
        )}
      </div>
    );
  }

  const edits = useDdlEdits.getState();
  const { target, loading } = state;
  const title = `${KIND_LABELS[target.kind] ?? target.kind} · ${target.name ?? 'new'}`;
  const where = [connectionName, target.scope.schema ? `${target.scope.database}.${target.scope.schema}` : target.scope.database]
    .filter(Boolean)
    .join(' › ');
  const context = `${where} · ${shortVersion(open.server)}`;

  const onMount: OnMount = (editor, monaco) => {
    // An action, not `addCommand`: it is scoped to this editor (see SqlEditor).
    const handle = editor.addAction({
      id: 'dnt.ddl.review',
      label: 'Review & apply',
      keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter],
      run: () => review.current(),
    });
    editor.onDidDispose(() => handle.dispose());
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.defaultPrevented || !(e.ctrlKey || e.metaKey) || e.altKey || e.shiftKey || e.key !== 'Enter') return;
    e.preventDefault();
    review.current();
  };

  const reopen = () => void edits.reopen(tab.id);

  return (
    <div className="ddl-editor" data-testid="ddl-editor" onKeyDown={onKeyDown}>
      <div className="ddl-header">
        <strong className="ddl-title">{title}</strong>
        <span className="ddl-subtle">{context}</span>
        {open.strategies.length > 1 ? (
          <div role="radiogroup" aria-label="Strategy" className="segmented ddl-strategy">
            {open.strategies.map((s) => {
              const on = state.strategy === s;
              return (
                <button
                  key={s}
                  type="button"
                  role="radio"
                  aria-checked={on}
                  className={`segment${on ? ' on' : ''}`}
                  disabled={!!blocked}
                  onClick={() => edits.setStrategy(tab.id, s)}
                >
                  {STRATEGY_LABELS[s]}
                </button>
              );
            })}
          </div>
        ) : null}
        <span className="ddl-spacer" />
        <button type="button" className="btn ddl-btn" disabled={!dirty || loading} onClick={() => edits.revert(tab.id)}>
          Revert
        </button>
        <button type="button" className="btn primary ddl-btn" disabled={!canReview} onClick={() => review.current()}>
          Review &amp; apply…
        </button>
      </div>
      {open.notes.length > 0 ? (
        <ul className="ddl-notes" aria-label="Notes">
          {open.notes.map((n, i) => (
            <li key={i}>{n}</li>
          ))}
        </ul>
      ) : null}
      {blocked ? (
        <div className="ddl-banner ddl-banner-info" role="note">
          <span>{blocked}</span>
        </div>
      ) : null}
      {state.lost ? (
        <div className="ddl-banner ddl-banner-warning" role="status">
          <span>The connection to this object was lost. Your text is kept.</span>
          <button type="button" className="btn small" disabled={loading} onClick={reopen}>
            Reopen
          </button>
        </div>
      ) : null}
      {state.missing && target.name === null ? (
        <div className="ddl-banner ddl-banner-warning" role="status">
          <span>{state.missing.name} no longer exists on the server. Applying this text creates it.</span>
          {state.missing.text !== null ? (
            <button
              type="button"
              className="btn small"
              disabled={loading || state.text === state.missing.text}
              onClick={() => edits.restore(tab.id, state.missing!.text!)}
            >
              Put the original definition in the editor
            </button>
          ) : null}
        </div>
      ) : null}
      {state.stale && !state.lost ? (
        <div className="ddl-banner ddl-banner-warning" role="status">
          <span>The object changed on the server since you opened it. Your text is kept; review it before applying.</span>
          <button type="button" className="btn small" disabled={loading} onClick={reopen}>
            Reopen
          </button>
        </div>
      ) : null}
      {state.error ? (
        <div className="ddl-banner ddl-banner-error" role="alert">
          <span>{state.error}</span>
        </div>
      ) : null}
      <div className="ddl-body">
        <Editor
          language={open.server.kind === 'postgres' ? 'pgsql' : 'mysql'}
          value={state.text}
          theme={monacoTheme.theme}
          beforeMount={monacoTheme.beforeMount}
          onMount={onMount}
          onChange={(v) => edits.setText(tab.id, v ?? '')}
          options={{
            readOnly: !!blocked,
            domReadOnly: !!blocked,
            automaticLayout: true,
            minimap: { enabled: false },
            ...editorFontOptions(),
            scrollBeyondLastLine: false,
            lightbulb: { enabled: 'off' as never },
            ariaLabel: 'DDL editor',
          }}
        />
      </div>
    </div>
  );
}
