import { useConnections } from '../store/connections';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';

/**
 * The connection of the active tab (else of the selected tree node), its server version
 * and whether it goes through SSH; coloured by its environment tag.
 */
export function StatusBar() {
  const selectedId = useTree((s) => s.selectedId);
  const tabConnection = useTabs((s) => s.tabs.find((t) => t.id === s.activeId)?.connectionId);
  const connectionId = tabConnection ?? (selectedId ? connectionIdOf(selectedId) : undefined);
  const summary = useConnections((s) => (connectionId ? s.summaries.get(connectionId) : undefined));
  const info = useConnections((s) => (connectionId ? s.info.get(connectionId) : undefined));

  const cfg = summary?.config;
  const production = cfg?.env_tag === 'production';
  return (
    <footer className={`statusbar${production ? ' env-production' : ''}`}>
      {cfg ? (
        <span className="status-conn">
          <span className={`dot${info ? ' on' : ''}`} aria-hidden="true" />
          <span>
            {cfg.name}
            {info ? ` · ${info.version_string}` : ' · not connected'}
            {cfg.ssh ? ` · via SSH ${cfg.ssh.host}` : ''}
          </span>
          {production && <span className="env-badge">PRODUCTION</span>}
        </span>
      ) : (
        <span className="status-conn">No connection selected</span>
      )}
      <span className="status-right">UTF-8</span>
    </footer>
  );
}
