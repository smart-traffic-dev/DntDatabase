import { render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { ThemeProvider } from '../../theme/ThemeProvider';
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

  it('explains_the_keys_while_empty', () => {
    const empty = { ...doc, sql: '' } as typeof doc;
    const view = render(<ThemeProvider><SqlEditor doc={empty} engine="mysql" /></ThemeProvider>);
    expect(screen.getByTestId('editor-placeholder')).toHaveTextContent('Ctrl+Enter runs the statement at the cursor');
    view.unmount();
    render(<ThemeProvider><SqlEditor doc={doc} engine="mysql" /></ThemeProvider>);
    expect(screen.queryByTestId('editor-placeholder')).toBeNull();
  });
});