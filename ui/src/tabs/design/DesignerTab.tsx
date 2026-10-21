import { useEffect } from 'react';
import { useTableDesigns } from '../../store/tableDesigns';
import type { DesignTabRef } from '../../store/tabs';

/** The table designer's tab. Placeholder: the editor itself arrives with the designer UI. */
export function DesignerTab({ tab }: { tab: DesignTabRef }) {
  const state = useTableDesigns((s) => s.byTab[tab.id]);
  const started = state !== undefined;
  useEffect(() => {
    // Loaded once per tab; a tab moved between groups remounts and keeps its draft.
    if (!started) void useTableDesigns.getState().load(tab.id, tab.connectionId, tab.scope, tab.table);
  }, [started, tab.id, tab.connectionId, tab.scope, tab.table]);

  return (
    <div className="empty" data-testid="designer-tab">
      {state?.error ? (
        <span role="alert">{state.error}</span>
      ) : state?.loading || !state?.draft ? (
        <span>Loading table design…</span>
      ) : (
        <span>Designing {state.draft.name}</span>
      )}
    </div>
  );
}
