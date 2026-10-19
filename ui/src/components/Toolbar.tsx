import { useDialogs } from '../store/dialogs';
import { LogoIcon, TOOL_PATHS, ToolIcon } from './icons';

const LATER = 'Coming in a later phase';

const DISABLED_TOOLS = [
  { label: 'New query', path: TOOL_PATHS.query },
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
  return (
    <header className="toolbar">
      <div className="brand">
        <LogoIcon />
        <span>DntDatabase</span>
      </div>
      <button
        type="button"
        className="tool"
        data-testid="new-connection"
        onClick={() => requestDialog({ kind: 'newConnection' })}
      >
        <ToolIcon path={TOOL_PATHS.connection} />
        <span>Connection</span>
      </button>
      {DISABLED_TOOLS.map((t) => (
        <button key={t.label} type="button" className="tool" disabled title={LATER}>
          <ToolIcon path={t.path} />
          <span>{t.label}</span>
        </button>
      ))}
    </header>
  );
}
