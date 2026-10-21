import { act, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useLayout } from '../store/layout';
import { usePalette } from '../store/palette';
import { useTabs } from '../store/tabs';

type Handler<T> = (event: { payload: T }) => void;

const win = vi.hoisted(() => {
  const state = { maximized: false, resized: [] as Handler<unknown>[], focus: [] as Handler<boolean>[] };
  return {
    state,
    isMaximized: vi.fn(async () => state.maximized),
    toggleMaximize: vi.fn(async () => {
      state.maximized = !state.maximized;
    }),
    minimize: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
    destroy: vi.fn(async () => {}),
    startDragging: vi.fn(async () => {}),
    onResized: vi.fn(async (h: Handler<unknown>) => {
      state.resized.push(h);
      return () => {
        state.resized = state.resized.filter((x) => x !== h);
      };
    }),
    onFocusChanged: vi.fn(async (h: Handler<boolean>) => {
      state.focus.push(h);
      return () => {
        state.focus = state.focus.filter((x) => x !== h);
      };
    }),
  };
});

vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => win }));

import { TitleBar } from './TitleBar';

async function flush() {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });
}

beforeEach(() => {
  win.state.maximized = false;
  win.state.resized = [];
  win.state.focus = [];
  useTabs.getState().reset();
  vi.clearAllMocks();
  win.isMaximized.mockImplementation(async () => win.state.maximized);
});

describe('TitleBar', () => {
  it('shows the window controls with Windows tooltips', async () => {
    render(<TitleBar />);
    await flush();
    expect(screen.getByRole('button', { name: 'Minimize' })).toHaveAttribute('title', 'Minimize');
    expect(screen.getByRole('button', { name: 'Maximize' })).toHaveAttribute('title', 'Maximize');
    expect(screen.getByRole('button', { name: 'Close' })).toHaveAttribute('title', 'Close');
  });

  it('leaves the tab title to the window title, not the bar', async () => {
    useTabs.getState().open({ id: 'q1', kind: 'query', connectionId: null, title: 'Query 1' });
    render(<TitleBar />);
    await flush();
    expect(screen.queryByTestId('titlebar-title')).not.toBeInTheDocument();
    expect(screen.queryByText(/Query 1/)).not.toBeInTheDocument();
    expect(document.title).toBe('Query 1 - DntDatabase');
  });

  it('the search button opens the palette in objects mode', async () => {
    usePalette.setState({ open: false, mode: 'commands' });
    render(<TitleBar />);
    await flush();
    const search = screen.getByRole('button', { name: 'Search tables, views, commands' });
    expect(search).toHaveTextContent('Search tables, views, commands…');
    expect(search).toHaveTextContent('Ctrl P');
    fireEvent.click(search);
    expect(usePalette.getState()).toMatchObject({ open: true, mode: 'objects' });
  });

  it('the layout button picks an editor layout', async () => {
    render(<TitleBar />);
    await flush();
    fireEvent.click(screen.getByRole('button', { name: 'Editor layout' }));
    fireEvent.click(screen.getByRole('menuitemradio', { name: /Two rows/ }));
    expect(useTabs.getState().layout).toBe('rows');
    expect(useTabs.getState().groups).toHaveLength(2);
  });

  it('the right sidebar button shows its state and toggles the panel', async () => {
    useLayout.setState({ rightVisible: false });
    render(<TitleBar />);
    await flush();
    const button = screen.getByRole('button', { name: 'Right sidebar' });
    expect(button).toHaveAttribute('title', 'Toggle right sidebar (Ctrl+Alt+B)');
    expect(button).toHaveAttribute('aria-pressed', 'false');
    fireEvent.click(button);
    expect(useLayout.getState().rightVisible).toBe(true);
    expect(button).toHaveAttribute('aria-pressed', 'true');
    fireEvent.mouseDown(button, { button: 0, detail: 1 });
    expect(win.startDragging).not.toHaveBeenCalled();
    useLayout.setState({ rightVisible: false });
  });

  it('the search button does not start a window drag', async () => {
    render(<TitleBar />);
    await flush();
    fireEvent.mouseDown(screen.getByRole('button', { name: 'Search tables, views, commands' }), { button: 0, detail: 1 });
    expect(win.startDragging).not.toHaveBeenCalled();
  });

  it('window controls do not take focus from the editor', async () => {
    render(<TitleBar />);
    await flush();
    for (const name of ['Minimize', 'Maximize', 'Close']) {
      expect(screen.getByRole('button', { name })).toHaveAttribute('tabindex', '-1');
    }
    const down = new MouseEvent('mousedown', { bubbles: true, cancelable: true, button: 0 });
    screen.getByRole('button', { name: 'Close' }).dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
    expect(win.startDragging).not.toHaveBeenCalled();
  });

  it('minimizes', async () => {
    render(<TitleBar />);
    await flush();
    fireEvent.click(screen.getByRole('button', { name: 'Minimize' }));
    expect(win.minimize).toHaveBeenCalledOnce();
  });

  it('maximize toggles and the button turns into Restore down', async () => {
    render(<TitleBar />);
    await flush();
    fireEvent.click(screen.getByRole('button', { name: 'Maximize' }));
    await flush();
    expect(win.toggleMaximize).toHaveBeenCalledOnce();
    const restore = screen.getByRole('button', { name: 'Restore down' });
    expect(restore).toHaveAttribute('title', 'Restore down');
    fireEvent.click(restore);
    await flush();
    expect(screen.getByRole('button', { name: 'Maximize' })).toBeInTheDocument();
  });

  it('rapid toggles: a stale isMaximized reply never overwrites a newer one', async () => {
    render(<TitleBar />);
    await flush();
    const replies: ((m: boolean) => void)[] = [];
    win.isMaximized.mockImplementation(() => new Promise<boolean>((r) => replies.push(r)));
    fireEvent.click(screen.getByRole('button', { name: 'Maximize' }));
    await flush();
    fireEvent.click(screen.getByRole('button', { name: 'Maximize' }));
    await flush();
    expect(win.toggleMaximize).toHaveBeenCalledTimes(2);
    expect(replies).toHaveLength(2);
    // The newer reply (restored) lands first, then the stale one (maximized).
    await act(async () => replies[1]?.(false));
    await act(async () => replies[0]?.(true));
    await flush();
    expect(screen.getByRole('button', { name: 'Maximize' })).toBeInTheDocument();
  });

  it('a resize reply after a toggle wins over the older toggle reply', async () => {
    render(<TitleBar />);
    await flush();
    const replies: ((m: boolean) => void)[] = [];
    win.isMaximized.mockImplementation(() => new Promise<boolean>((r) => replies.push(r)));
    fireEvent.click(screen.getByRole('button', { name: 'Maximize' }));
    await flush();
    await act(async () => {
      for (const h of win.state.resized) h({ payload: { width: 1, height: 1 } });
    });
    expect(replies).toHaveLength(2);
    await act(async () => replies[1]?.(true));
    await act(async () => replies[0]?.(false));
    await flush();
    expect(screen.getByRole('button', { name: 'Restore down' })).toBeInTheDocument();
  });

  it('starts with the real maximized state', async () => {
    win.state.maximized = true;
    render(<TitleBar />);
    await flush();
    expect(screen.getByRole('button', { name: 'Restore down' })).toBeInTheDocument();
  });

  it('follows maximize / restore done by the OS (snap, Win+Up, restore)', async () => {
    render(<TitleBar />);
    await flush();
    win.state.maximized = true;
    await act(async () => {
      for (const h of win.state.resized) h({ payload: { width: 1, height: 1 } });
    });
    await flush();
    expect(screen.getByRole('button', { name: 'Restore down' })).toBeInTheDocument();
    win.state.maximized = false;
    await act(async () => {
      for (const h of win.state.resized) h({ payload: { width: 1, height: 1 } });
    });
    await flush();
    expect(screen.getByRole('button', { name: 'Maximize' })).toBeInTheDocument();
  });

  it('close asks the window to close (the close guard runs), never destroys it', async () => {
    render(<TitleBar />);
    await flush();
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    expect(win.close).toHaveBeenCalledOnce();
    expect(win.destroy).not.toHaveBeenCalled();
  });

  it('drags from the title area and toggles maximize on double-click', async () => {
    render(<TitleBar />);
    await flush();
    const drag = screen.getByTestId('titlebar-drag');
    fireEvent.mouseDown(drag, { button: 0, detail: 1 });
    expect(win.startDragging).toHaveBeenCalledOnce();
    fireEvent.mouseDown(drag, { button: 0, detail: 2 });
    expect(win.toggleMaximize).toHaveBeenCalledOnce();
    expect(win.startDragging).toHaveBeenCalledOnce();
  });

  it('right and middle buttons do not drag', async () => {
    render(<TitleBar />);
    await flush();
    const drag = screen.getByTestId('titlebar-drag');
    fireEvent.mouseDown(drag, { button: 2, detail: 1 });
    fireEvent.mouseDown(drag, { button: 1, detail: 1 });
    expect(win.startDragging).not.toHaveBeenCalled();
  });

  it('dims while the window is inactive', async () => {
    const { container } = render(<TitleBar />);
    await flush();
    const bar = container.querySelector('.titlebar');
    act(() => {
      for (const h of win.state.focus) h({ payload: false });
    });
    expect(bar).toHaveClass('inactive');
    act(() => {
      for (const h of win.state.focus) h({ payload: true });
    });
    expect(bar).not.toHaveClass('inactive');
  });

  it('unsubscribes from the window events on unmount', async () => {
    const { unmount } = render(<TitleBar />);
    await flush();
    expect(win.state.resized).toHaveLength(1);
    expect(win.state.focus).toHaveLength(1);
    unmount();
    await flush();
    expect(win.state.resized).toHaveLength(0);
    expect(win.state.focus).toHaveLength(0);
  });
});
