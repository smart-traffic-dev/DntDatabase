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

export const CloseIcon = () => (
  <Icon size={12} strokeWidth={2}>
    <path d="M6 6l12 12M18 6L6 18" />
  </Icon>
);

/** Toolbar glyphs: stroke paths of the canvas. */
export const TOOL_PATHS = {
  connection: 'M10 14a5 5 0 007 0l3-3a5 5 0 00-7-7l-1 1M14 10a5 5 0 00-7 0l-3 3a5 5 0 007 7l1-1',
  query: 'M4 4h16v16H4zM8 9l3 3-3 3M13 15h3',
  table: 'M3 4h18v16H3zM3 10h18M9 10v10',
  view: 'M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM12 9a3 3 0 100 6 3 3 0 000-6z',
  fn: 'M9 4c-2 0-3 1-3 3v10c0 2-1 3-3 3M3 11h8M14 8l6 8M20 8l-6 8',
  io: 'M12 3v12M7 10l5 5 5-5M4 19h16',
  backup: 'M4 7h16v13H4zM4 7l2-3h12l2 3M10 12h4',
  erd: 'M3 4h7v6H3zM14 14h7v6h-7zM6.5 10v4a2 2 0 002 2H14',
  compare: 'M8 3v18M16 3v18M4 7h4M16 17h4M4 12h16',
} as const;

export const ToolIcon = ({ path }: { path: string }) => (
  <Icon size={18}>
    <path d={path} />
  </Icon>
);
