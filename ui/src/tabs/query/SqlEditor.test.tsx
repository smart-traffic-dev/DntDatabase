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
  it('uses vs in light and vs-dark in dark', () => {
    localStorage.setItem('theme', 'light');
    const light = render(<ThemeProvider><SqlEditor doc={doc} engine="mysql" /></ThemeProvider>);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-theme', 'vs');
    light.unmount();
    localStorage.setItem('theme', 'dark');
    render(<ThemeProvider><SqlEditor doc={doc} engine="mysql" /></ThemeProvider>);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-theme', 'vs-dark');
  });
});
