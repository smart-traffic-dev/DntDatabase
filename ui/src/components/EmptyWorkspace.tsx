import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { openNewQuery } from './newQuery';

/** Nothing open: a first-start call to action, else the keys that get a user going. */
export function EmptyWorkspace() {
  const hasConnections = useConnections((s) => s.summaries.size > 0);
  const requestDialog = useDialogs((s) => s.requestDialog);
  if (!hasConnections) {
    return (
      <div className="empty" data-testid="workspace-empty">
        <strong>No connections yet</strong>
        <span>Add a connection to start browsing and querying.</span>
        <button type="button" className="btn primary" onClick={() => requestDialog({ kind: 'newConnection' })}>
          New connection
        </button>
      </div>
    );
  }
  return (
    <div className="empty" data-testid="workspace-empty">
      <strong>No tab open</strong>
      <span>Double-click a table or view in the connection tree to open it.</span>
      <span>
        <kbd>Ctrl+T</kbd> opens a new query; <kbd>Ctrl+Enter</kbd> runs the statement at the cursor.
      </span>
      <button type="button" className="btn" onClick={() => openNewQuery()}>
        New query
      </button>
    </div>
  );
}
