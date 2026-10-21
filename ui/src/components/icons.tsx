import type { ReactNode } from 'react';

interface IconProps {
  size?: number;
  strokeWidth?: number;
  children: ReactNode;
}

function Icon({ size = 15, strokeWidth = 1.8, children }: IconProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export const LogoIcon = () => (
  <Icon size={20} strokeWidth={2}>
    <ellipse cx="12" cy="5" rx="8" ry="3" />
    <path d="M4 5v14c0 1.7 3.6 3 8 3s8-1.3 8-3V5" />
    <path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3" />
  </Icon>
);

export const ChevronIcon = () => (
  <Icon size={10} strokeWidth={2.5}>
    <path d="M9 6l6 6-6 6" />
  </Icon>
);

export const ChevronDownIcon = () => (
  <Icon size={10} strokeWidth={2.5}>
    <path d="M6 9l6 6 6-6" />
  </Icon>
);

/** Padlock: read-only connections. */
export const LockIcon = () => (
  <Icon size={11} strokeWidth={2.2}>
    <rect x="5" y="11" width="14" height="9" rx="2" />
    <path d="M8 11V8a4 4 0 018 0v3" />
  </Icon>
);

/** Database with a gear: system databases / schemas. */
export const SystemIcon = () => (
  <Icon>
    <ellipse cx="10" cy="5" rx="6.5" ry="2.5" />
    <path d="M3.5 5v12c0 1.4 2.9 2.5 6.5 2.5M3.5 11c0 1.4 2.9 2.5 6.5 2.5" />
    <circle cx="17.5" cy="16.5" r="2.5" />
    <path d="M17.5 11.5v2M17.5 19.5v2M12.5 16.5h2M20.5 16.5h2" />
  </Icon>
);

export const PlusIcon = () => (
  <Icon size={14} strokeWidth={2}>
    <path d="M12 5v14M5 12h14" />
  </Icon>
);

export const FolderIcon = () => (
  <Icon>
    <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2z" />
  </Icon>
);

export const DatabaseIcon = () => (
  <Icon>
    <ellipse cx="12" cy="5" rx="7" ry="2.5" />
    <path d="M5 5v14c0 1.4 3.1 2.5 7 2.5s7-1.1 7-2.5V5" />
  </Icon>
);

export const SchemaIcon = () => (
  <Icon>
    <path d="M3 4h7v6H3zM14 14h7v6h-7zM6.5 10v4a2 2 0 002 2H14" />
  </Icon>
);

export const ListIcon = () => (
  <Icon>
    <path d="M4 6h16M4 12h16M4 18h10" />
  </Icon>
);

export const TableIcon = () => (
  <Icon>
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <path d="M3 10h18M9 10v10" />
  </Icon>
);

export const ObjectIcon = () => (
  <Icon>
    <path d="M7 4h10a1 1 0 011 1v14a1 1 0 01-1 1H7a1 1 0 01-1-1V5a1 1 0 011-1zM9 9h6M9 13h6" />
  </Icon>
);

export const EyeIcon = () => (
  <Icon>
    <path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12z" />
    <circle cx="12" cy="12" r="3" />
  </Icon>
);

/** Pushpin: pinned tabs. */
export const PinIcon = () => (
  <Icon size={11} strokeWidth={2.2}>
    <path d="M9 4h6l-1 6 3 3H7l3-3zM12 13v7" />
  </Icon>
);

/** Function / procedure glyph for the quick-open list. */
export const FunctionIcon = () => (
  <Icon>
    <path d="M9 4c-2 0-3 1-3 3v10c0 2-1 3-3 3M3 11h8M14 8l6 8M20 8l-6 8" />
  </Icon>
);

export const SearchIcon = () => (
  <Icon size={13} strokeWidth={2}>
    <circle cx="11" cy="11" r="6.5" />
    <path d="M16 16l4.5 4.5" />
  </Icon>
);

/** Three dots: overflow menus. */
export const MoreIcon = () => (
  <Icon size={14} strokeWidth={2.4}>
    <path d="M5 12h.01M12 12h.01M19 12h.01" />
  </Icon>
);

/** Snippet / code glyph: Manage snippets. */
export const SnippetIcon = () => (
  <Icon size={14}>
    <path d="M8 7l-5 5 5 5M16 7l5 5-5 5M14 4l-4 16" />
  </Icon>
);

/** Gear-and-arrow: stored procedures. */
export const ProcedureIcon = () => (
  <Icon>
    <path d="M5 5h9l5 5v9H5zM9 12l2.5 2.5L9 17M13 17h3" />
  </Icon>
);

/** Lightning bolt: triggers. */
export const TriggerIcon = () => (
  <Icon>
    <path d="M13 3L5 14h6l-1 7 8-11h-6z" />
  </Icon>
);

/** Clock: scheduled events. */
export const EventIcon = () => (
  <Icon>
    <circle cx="12" cy="12" r="8" />
    <path d="M12 8v4l3 2" />
  </Icon>
);

export const CloseIcon = () => (
  <Icon size={12} strokeWidth={2}>
    <path d="M6 6l12 12M18 6L6 18" />
  </Icon>
);

