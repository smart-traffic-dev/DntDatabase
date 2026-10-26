import type { ReactNode } from 'react';

/** One toolbar action; `disabled` with a string gives the reason as its tooltip. */
export interface ToolAction {
  label: string;
  icon: ReactNode;
  shortcut?: string;
  disabled?: boolean | string;
  pressed?: boolean;
  onClick(): void;
}

const icon = (d: string) => (
  <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    <path d={d} />
  </svg>
);

export const ICONS = {
  add: icon('M8 3v10M3 8h10'),
  insert: icon('M2 4h12M2 12h12M8 6v4M6 8h4'),
  delete: icon('M3 4h10M6.5 4V2.5h3V4M4.5 4l.6 9.5h5.8l.6-9.5'),
  key: icon('M5.5 13.5a3 3 0 1 1 0-6 3 3 0 0 1 0 6zM7.8 8.2l6-6M11.5 4.5l2 2'),
  up: icon('M8 13V3M4 7l4-4 4 4'),
  down: icon('M8 3v10M4 9l4 4 4-4'),
  undo: icon('M5 3 2 6l3 3M2 6h7.5a4 4 0 0 1 0 8H7'),
  redo: icon('M11 3l3 3-3 3M14 6H6.5a4 4 0 0 0 0 8H9'),
};

/** The designer's toolbar (Navicat's Add / Insert / Delete / Primary key / Move): groups split by dividers. */
export function DesignerToolbar({ groups }: { groups: ToolAction[][] }) {
  return (
    <div role="toolbar" aria-label="Table design" className="dz-toolbar">
      {groups
        .filter((g) => g.length > 0)
        .map((group, i) => (
          <div key={i} className="dz-tool-group">
            {group.map((a) => {
              const reason = typeof a.disabled === 'string' ? a.disabled : undefined;
              return (
                <button
                  key={a.label}
                  type="button"
                  className={`dz-tool${a.pressed ? ' on' : ''}`}
                  disabled={!!a.disabled}
                  aria-pressed={a.pressed}
                  aria-keyshortcuts={a.shortcut?.replaceAll('Ctrl', 'Control')}
                  title={reason ?? (a.shortcut ? `${a.label} (${a.shortcut})` : a.label)}
                  onClick={a.onClick}
                >
                  {a.icon}
                  <span>{a.label}</span>
                </button>
              );
            })}
          </div>
        ))}
    </div>
  );
}
