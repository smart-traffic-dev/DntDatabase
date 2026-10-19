import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { ConnectionTree } from './ConnectionTree';
import { PlusIcon } from './icons';

export function Sidebar() {
  const filter = useTree((s) => s.filter);
  const setFilter = useTree((s) => s.setFilter);
  const showSystem = useTree((s) => s.showSystem);
  const setShowSystem = useTree((s) => s.setShowSystem);
  const error = useTree((s) => s.error);
  const requestDialog = useDialogs((s) => s.requestDialog);

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
          aria-label="Show system databases"
          aria-pressed={showSystem}
          title="Show system databases"
          onClick={() => setShowSystem(!showSystem)}
        >
          S
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
      <ConnectionTree />
    </aside>
  );
}
