import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { ROOT } from '../store/treeModel';
import { ConnectionTree } from './ConnectionTree';
import { PlusIcon, SystemIcon } from './icons';

export function Sidebar() {
  const filter = useTree((s) => s.filter);
  const setFilter = useTree((s) => s.setFilter);
  const showSystem = useTree((s) => s.showSystem);
  const setShowSystem = useTree((s) => s.setShowSystem);
  const error = useTree((s) => s.error);
  const requestDialog = useDialogs((s) => s.requestDialog);
  // Loaded and nothing stored: show the first-start call to action instead of a blank tree.
  const treeEmpty = useTree((s) => s.childIds.get(ROOT)?.length === 0);
  const storedCount = useConnections((c) => c.summaries.size);
  const noConnections = treeEmpty && storedCount === 0;

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <input
          type="search"
          aria-label="Filter objects"
          placeholder="Filter objects…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <button
          type="button"
          className={`icon-btn${showSystem ? ' on' : ''}`}
          aria-label="Show system schemas"
          aria-pressed={showSystem}
          title="Show system schemas"
          onClick={() => setShowSystem(!showSystem)}
        >
          <SystemIcon />
        </button>
        <button
          type="button"
          className="icon-btn"
          aria-label="New connection"
          title="New connection"
          onClick={() => requestDialog({ kind: 'newConnection' })}
        >
          <PlusIcon />
        </button>
      </div>
      {error && (
        <div className="banner error" role="alert">
          <span>{error.message}</span>
          <button type="button" className="btn small" onClick={() => void useTree.getState().reloadStored()}>
            Retry
          </button>
        </div>
      )}
      {noConnections ? (
        <div className="sidebar-empty" data-testid="sidebar-empty">
          <strong>No connections yet</strong>
          <span>Add a database connection to browse its tables.</span>
          <button type="button" className="btn primary" onClick={() => requestDialog({ kind: 'newConnection' })}>
            New connection
          </button>
          <button type="button" className="btn" onClick={() => requestDialog({ kind: 'import' })}>
            Import…
          </button>
        </div>
      ) : (
        <ConnectionTree />
      )}
    </aside>
  );
}
