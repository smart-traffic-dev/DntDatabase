import { act, render, renderHook, screen } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useDntMonacoTheme } from '../../monaco/dntTheme';
import { ThemeProvider, useTheme } from '../../theme/ThemeProvider';
import { SqlEditor } from './SqlEditor';

afterEach(() => {
  localStorage.clear();
  document.documentElement.classList.remove('dark');
});

const doc = { id: 'q1', sql: 'select 1', dirty: false } as unknown as Parameters<typeof SqlEditor>[0]['doc'];

describe('SqlEditor theme', () => {
  it('uses the app themes (dnt-light, dnt-dark)', () => {
    localStorage.setItem('theme', 'light');
    const light = render(<ThemeProvider><SqlEditor doc={doc} engine="mysql" /></ThemeProvider>);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-theme', 'dnt-light');
    light.unmount();
    localStorage.setItem('theme', 'dark');
    render(<ThemeProvider><SqlEditor doc={doc} engine="mysql" /></ThemeProvider>);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-theme', 'dnt-dark');
  });

  // The run keys are shown by the collapsed result panel (QueryTab.split.test), not inside the editor.
  it('shows no placeholder text while empty', () => {
    const empty = { ...doc, sql: '' } as typeof doc;
    render(<ThemeProvider><SqlEditor doc={empty} engine="mysql" /></ThemeProvider>);
    expect(screen.queryByTestId('editor-placeholder')).toBeNull();
    expect(screen.getByTestId('query-editor')).not.toHaveTextContent('Ctrl+Enter');
  });

  it('dark_toggle_redefines_and_sets_the_dnt_dark_theme', () => {
    localStorage.setItem('theme', 'light');
    const defineTheme = vi.fn();
    const setTheme = vi.fn();
    const monaco = { editor: { defineTheme, setTheme } } as unknown as Parameters<ReturnType<typeof useDntMonacoTheme>['beforeMount']>[0];
    const wrapper = ({ children }: { children: ReactNode }) => <ThemeProvider>{children}</ThemeProvider>;
    const { result } = renderHook(() => ({ theme: useDntMonacoTheme(), app: useTheme() }), { wrapper });
    act(() => result.current.theme.beforeMount(monaco));
    expect(defineTheme).toHaveBeenLastCalledWith('dnt-light', expect.objectContaining({ base: 'vs' }));
    act(() => result.current.app.setTheme('dark'));
    expect(defineTheme).toHaveBeenLastCalledWith('dnt-dark', expect.objectContaining({ base: 'vs-dark' }));
    expect(setTheme).toHaveBeenLastCalledWith('dnt-dark');
  });
});