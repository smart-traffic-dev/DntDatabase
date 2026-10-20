import { getCurrentWindow } from '@tauri-apps/api/window';
import { useEffect, useRef, useState, type MouseEvent, type ReactNode } from 'react';
import { useTabs } from '../store/tabs';
import { LogoIcon } from './icons';
import { MenuBar } from './MenuBar';
import './titleBar.css';

/** The window calls the title bar makes (a subset of Tauri's `Window`). */
interface TitleBarWindow {
  isMaximized(): Promise<boolean>;
  toggleMaximize(): Promise<void>;
  minimize(): Promise<void>;
  close(): Promise<void>;
  startDragging(): Promise<void>;
  onResized(handler: () => void): Promise<() => void>;
  onFocusChanged(handler: (event: { payload: boolean }) => void): Promise<() => void>;
}

function currentWindow(): TitleBarWindow | null {
  try {
    return getCurrentWindow();
  } catch {
    return null; // Not inside Tauri (plain browser preview).
  }
}

const ignore = () => {};

/** Window-control glyphs drawn like Windows 11's (10 px, 1 px strokes). */
const Glyph = ({ children }: { children: ReactNode }) => (
  <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1">
    {children}
  </svg>
);

/**
 * The window's title bar (the native one is off: `decorations: false`). Drag to move (Aero
 * snap included: the OS runs the move loop), double-click to maximize / restore, and the
 * Windows minimize / maximize / close buttons. Close asks the window to close, so the close
 * guard (installCloseGuard) runs exactly as for Alt+F4.
 *
 * Not available with custom decorations: the Windows 11 snap-layout flyout on the maximize
 * button.
 */
export function TitleBar() {
  const [win] = useState(currentWindow);
  const [maximized, setMaximized] = useState(false);
  const [focused, setFocused] = useState(() => document.hasFocus());
  const title = useTabs((s) => s.tabs.find((t) => t.id === s.activeId)?.title);

  /** Re-reads the maximized state; replaced by the effect while mounted. */
  const sync = useRef<() => void>(() => {});

  useEffect(() => {
    if (!win) return;
    let live = true;
    // Only the latest request's reply is applied: replies can arrive out of order (rapid toggles).
    let seq = 0;
    sync.current = () => {
      const mine = ++seq;
      win.isMaximized().then((m) => live && mine === seq && setMaximized(m), ignore);
    };
    sync.current();
    // Maximize / restore also happen outside the bar (Win+Up / Down, snap, restore from the taskbar).
    const unResized = win.onResized(() => sync.current());
    const unFocus = win.onFocusChanged(({ payload }) => live && setFocused(payload));
    return () => {
      live = false;
      sync.current = () => {};
      for (const un of [unResized, unFocus]) void un.then((f) => f(), ignore);
    };
  }, [win]);

  const toggleMaximize = () => {
    if (!win) return;
    win.toggleMaximize().then(() => sync.current(), ignore);
  };

  const onDragMouseDown = (e: MouseEvent) => {
    if (!win || e.button !== 0) return;
    e.preventDefault();
    if (e.detail === 2) toggleMaximize();
    else if (e.detail === 1) win.startDragging().catch(ignore);
  };

  /** Window controls never take focus: the editor keeps its caret. */
  const keepFocus = (e: MouseEvent) => e.preventDefault();

  const maxLabel = maximized ? 'Restore down' : 'Maximize';
  return (
    <div className={`titlebar${focused ? '' : ' inactive'}`}>
      <div className="titlebar-brand" onMouseDown={onDragMouseDown}>
        <span className="titlebar-icon" aria-hidden="true">
          <LogoIcon />
        </span>
      </div>
      <MenuBar />
      <div className="titlebar-drag" data-testid="titlebar-drag" onMouseDown={onDragMouseDown}>
        <span className="titlebar-title" data-testid="titlebar-title">
          {title ? `${title} - ` : ''}
          <span>DntDatabase</span>
        </span>
      </div>
      <div className="titlebar-controls">
        <button
          type="button"
          className="titlebar-btn"
          tabIndex={-1}
          aria-label="Minimize"
          title="Minimize"
          onMouseDown={keepFocus}
          onClick={() => void win?.minimize().catch(ignore)}
        >
          <Glyph>
            <path d="M0 5.5h10" />
          </Glyph>
        </button>
        <button
          type="button"
          className="titlebar-btn"
          tabIndex={-1}
          aria-label={maxLabel}
          title={maxLabel}
          onMouseDown={keepFocus}
          onClick={toggleMaximize}
        >
          <Glyph>
            {maximized ? (
              <>
                <rect x="0.5" y="2.5" width="7" height="7" />
                <path d="M2.5 2.5V0.5h7v7h-2" />
              </>
            ) : (
              <rect x="0.5" y="0.5" width="9" height="9" />
            )}
          </Glyph>
        </button>
        <button
          type="button"
          className="titlebar-btn close"
          tabIndex={-1}
          aria-label="Close"
          title="Close"
          onMouseDown={keepFocus}
          onClick={() => void win?.close().catch(ignore)}
        >
          <Glyph>
            <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" />
          </Glyph>
        </button>
      </div>
    </div>
  );
}
