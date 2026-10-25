import { useDdlEdits } from '../../store/ddlEdits';
import type { DdlTabRef } from '../../store/tabs';

/**
 * The DDL editor tab. Placeholder until the editor itself lands: it shows the edit's text,
 * read-only, with its loading, error, lost and blocked states.
 */
export function DdlEditorTab({ tab }: { tab: DdlTabRef }) {
  const state = useDdlEdits((s) => s.byTab[tab.id]);
  if (!state || (state.loading && !state.open)) return <p className="muted">Loading…</p>;
  return (
    <div className="ddl-editor" data-testid="ddl-editor">
      {state.error ? <div role="alert">{state.error}</div> : null}
      {state.lost ? <div role="status">The connection closed. Your text is kept.</div> : null}
      {state.open?.blocked_reason ? <div role="status">{state.open.blocked_reason}</div> : null}
      <pre data-testid="ddl-editor-text">{state.text}</pre>
    </div>
  );
}
