import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { AUTO_HIDE_MS, MAX_TOASTS, useToasts } from '../store/toasts';
import { appError, resetStores } from '../test/backend';
import { ToastHost } from './ToastHost';

const sheet = import.meta.glob('./toasts.css', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;
const css = (Object.values(sheet)[0] ?? '').replace(/\/\*[\s\S]*?\*\//g, '');
const rule = (selector: string) => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`(^|\\})\\s*${escaped}\\s*\\{([^}]*)\\}`).exec(css)?.[2] ?? '';
};

const toasts = () => [...document.querySelectorAll('.toast')];
const titles = () => toasts().map((t) => t.querySelector('.toast-title')?.textContent);

beforeEach(() => {
  resetStores();
  vi.useFakeTimers();
});
afterEach(() => vi.useRealTimers());

describe('toasts (spec §6)', () => {
  it('stack bottom-right on the raised surface with shadow-2', () => {
    expect(rule('.toast-stack')).toMatch(/position:\s*fixed/);
    expect(rule('.toast-stack')).toMatch(/right:/);
    expect(rule('.toast-stack')).toMatch(/bottom:/);
    expect(rule('.toast')).toMatch(/background:\s*var\(--dnt-bg-raised\)/);
    expect(rule('.toast')).toMatch(/box-shadow:\s*var\(--dnt-shadow-2\)/);
  });

  it('shows at most three; a new one pushes the oldest dismissible one out', () => {
    expect(MAX_TOASTS).toBe(3);
    render(<ToastHost />);
    act(() => {
      useToasts.getState().show({ kind: 'error', title: 'Export failed' });
      useToasts.getState().show({ kind: 'info', title: 'one' });
      useToasts.getState().show({ kind: 'info', title: 'two' });
      useToasts.getState().show({ kind: 'success', title: 'three' });
    });
    // The error is kept: it waits to be read; the oldest info goes.
    expect(titles()).toEqual(['Export failed', 'two', 'three']);
  });

  it('success and info hide after 4 s, paused while hovered or focused', () => {
    expect(AUTO_HIDE_MS).toBe(4000);
    render(<ToastHost />);
    act(() => void useToasts.getState().show({ kind: 'success', title: 'Committed', message: '1 statement · 12 ms' }));
    const toast = screen.getByRole('status');
    expect(toast).toHaveTextContent('Committed1 statement · 12 ms');
    act(() => vi.advanceTimersByTime(3000));
    fireEvent.mouseEnter(toast);
    act(() => vi.advanceTimersByTime(10_000));
    expect(screen.getByRole('status')).toBeInTheDocument();
    fireEvent.mouseLeave(toast);
    act(() => vi.advanceTimersByTime(900));
    expect(screen.getByRole('status')).toBeInTheDocument();
    // Focus pauses too.
    fireEvent.focusIn(within(toast).getByRole('button', { name: 'Dismiss' }));
    act(() => vi.advanceTimersByTime(10_000));
    expect(screen.getByRole('status')).toBeInTheDocument();
    fireEvent.focusOut(within(toast).getByRole('button', { name: 'Dismiss' }));
    act(() => vi.advanceTimersByTime(1100));
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('an error stays until dismissed and offers Copy details', async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
    render(<ToastHost />);
    act(
      () =>
        void useToasts.getState().show({
          kind: 'error',
          title: 'Export failed',
          message: 'D:\\exports\\orders.csv is open in another app.',
          details: 'os error 32',
        }),
    );
    act(() => vi.advanceTimersByTime(60_000));
    const toast = screen.getByRole('alert');
    await act(async () => fireEvent.click(within(toast).getByRole('button', { name: 'Copy details' })));
    expect(writeText).toHaveBeenCalledWith('Export failed\nD:\\exports\\orders.csv is open in another app.\nos error 32');
    expect(within(toast).getByRole('button', { name: 'Copied' })).toBeInTheDocument();
    fireEvent.click(within(toast).getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('a progress toast shows a bar and stays until it is finished', () => {
    render(<ToastHost />);
    let id = 0;
    act(() => void (id = useToasts.getState().show({ kind: 'progress', title: 'Exporting orders.csv', progress: 0.25 })));
    const bar = screen.getByRole('progressbar', { name: 'Exporting orders.csv' });
    expect(bar).toHaveAttribute('aria-valuenow', '25');
    act(() => vi.advanceTimersByTime(60_000));
    act(() => useToasts.getState().update(id, { progress: 0.62, message: '21,400 / 34,120 rows' }));
    expect(screen.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '62');
    expect(screen.getByText('21,400 / 34,120 rows')).toBeInTheDocument();
    // Done: it turns into a success toast, which hides on its own.
    act(() => useToasts.getState().update(id, { kind: 'success', title: 'Exported orders.csv', progress: undefined }));
    expect(screen.queryByRole('progressbar')).toBeNull();
    act(() => vi.advanceTimersByTime(AUTO_HIDE_MS));
    expect(toasts()).toHaveLength(0);
  });

  it('toast actions run and close the toast', () => {
    const retry = vi.fn();
    render(<ToastHost />);
    act(() => void useToasts.getState().show({ kind: 'error', title: 'Export failed', actions: [{ label: 'Retry', run: retry }] }));
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    expect(retry).toHaveBeenCalledTimes(1);
    expect(toasts()).toHaveLength(0);
  });
});

describe('notices on the toast stack', () => {
  it('setNotice shows an info toast; the same text again restarts its timer; dismissing clears it', () => {
    render(<ToastHost />);
    act(() => useDialogs.getState().setNotice('Imported 3 connections'));
    expect(screen.getByRole('status')).toHaveTextContent('Imported 3 connections');
    act(() => vi.advanceTimersByTime(3000));
    act(() => useDialogs.getState().setNotice('Imported 3 connections'));
    expect(toasts()).toHaveLength(1);
    act(() => vi.advanceTimersByTime(3000));
    expect(screen.getByRole('status')).toBeInTheDocument();
    act(() => vi.advanceTimersByTime(1100));
    expect(screen.queryByRole('status')).toBeNull();
    expect(useDialogs.getState().notice).toBeNull();

    act(() => useDialogs.getState().setNotice('Saved'));
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByRole('status')).toBeNull();
    expect(useDialogs.getState().notice).toBeNull();
  });

  it('different notices stack; setNotice(null) hides the current one', () => {
    render(<ToastHost />);
    act(() => useDialogs.getState().setNotice('first'));
    act(() => useDialogs.getState().setNotice('second'));
    expect(titles()).toEqual(['first', 'second']);
    expect(useDialogs.getState().notice).toBe('second');
    act(() => useDialogs.getState().setNotice(null));
    expect(titles()).toEqual(['first']);
  });

  it('an error notice stays and can be copied', () => {
    render(<ToastHost />);
    act(() => useDialogs.getState().setNotice('Could not save x.sql: access denied', 'error'));
    act(() => vi.advanceTimersByTime(60_000));
    const toast = screen.getByRole('alert');
    expect(toast).toHaveTextContent('Could not save x.sql: access denied');
    expect(within(toast).getByRole('button', { name: 'Copy details' })).toBeInTheDocument();
  });

  it('the internal error toast is part of the stack and counts toward the three', () => {
    render(<ToastHost />);
    act(() => {
      useDialogs.getState().reportInternal(appError('internal', 'An internal error occurred.'));
      useToasts.getState().show({ kind: 'info', title: 'one' });
      useToasts.getState().show({ kind: 'info', title: 'two' });
      useToasts.getState().show({ kind: 'info', title: 'three' });
    });
    const stack = document.querySelector('.toast-stack')!;
    expect(within(stack as HTMLElement).getByRole('alert', { name: 'Internal error' })).toBeInTheDocument();
    expect(toasts()).toHaveLength(3);
    expect(titles()).toEqual(['An internal error occurred.', 'two', 'three']);
  });
});
