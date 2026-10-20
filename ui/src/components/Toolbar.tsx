import { useState } from 'react';
import { useDialogs } from '../store/dialogs';
import { ActionMenu, transferItems } from './ActionMenu';
import { openNewQuery } from './newQuery';
import { ChevronDownIcon, TOOL_PATHS, ToolIcon } from './icons';

const LATER = 'Coming in a later phase';

const DISABLED_TOOLS = [
  { label: 'Tables', path: TOOL_PATHS.table },
  { label: 'Views', path: TOOL_PATHS.view },
  { label: 'Functions', path: TOOL_PATHS.fn },
  { label: 'Import/Export', path: TOOL_PATHS.io },
  { label: 'Backup', path: TOOL_PATHS.backup },
  { label: 'ER diagram', path: TOOL_PATHS.erd },
  { label: 'Compare', path: TOOL_PATHS.compare },
];

export function Toolbar() {
  const requestDialog = useDialogs((s) => s.requestDialog);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  return (
    <header className="toolbar">
      <div className="tool-split">
        <button
          type="button"
          className="tool"
          data-testid="new-connection"
          onClick={() => requestDialog({ kind: 'newConnection' })}
        >
          <ToolIcon path={TOOL_PATHS.connection} />
          <span>Connection</span>
        </button>
        <button
          type="button"
          className="tool-more"
          aria-label="Connection menu"
          aria-haspopup="menu"
          aria-expanded={menu !== null}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            setMenu({ x: r.left, y: r.bottom });
          }}
        >
          <ChevronDownIcon />
        </button>
      </div>
      {menu && (
        <ActionMenu
          label="Connection actions"
          x={menu.x}
          y={menu.y}
          items={[
            { label: 'New connection', run: () => requestDialog({ kind: 'newConnection' }) },
            ...transferItems(),
            { label: 'Settings…', run: () => requestDialog({ kind: 'settings' }) },
          ]}
          onClose={() => setMenu(null)}
        />
      )}
      <button type="button" className="tool" data-testid="new-query" onClick={() => openNewQuery()}>
        <ToolIcon path={TOOL_PATHS.query} />
        <span>New query</span>
      </button>
      <div className="tool-group" role="group" aria-label={LATER} title={LATER}>
        {DISABLED_TOOLS.map((t) => (
          <button key={t.label} type="button" className="tool" disabled aria-label={`${t.label} (${LATER.toLowerCase()})`}>
            <ToolIcon path={t.path} />
            <span>{t.label}</span>
          </button>
        ))}
      </div>
    </header>
  );
}
