import { act, renderHook } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { ThemeProvider, useTheme } from './ThemeProvider';

afterEach(() => {
  localStorage.clear();
  document.documentElement.classList.remove('dark');
  document.documentElement.removeAttribute('data-density');
});

describe('density', () => {
  it('defaults to compact (no attribute)', () => {
    const { result } = renderHook(() => useTheme(), { wrapper: ThemeProvider });
    expect(result.current.density).toBe('compact');
    expect(document.documentElement).not.toHaveAttribute('data-density');
  });

  it('setDensity(comfortable) sets data-density and persists', () => {
    const { result } = renderHook(() => useTheme(), { wrapper: ThemeProvider });
    act(() => result.current.setDensity('comfortable'));
    expect(result.current.density).toBe('comfortable');
    expect(document.documentElement).toHaveAttribute('data-density', 'comfortable');
    expect(localStorage.getItem('density')).toBe('comfortable');

    act(() => result.current.setDensity('compact'));
    expect(document.documentElement).not.toHaveAttribute('data-density');
    expect(localStorage.getItem('density')).toBe('compact');
  });

  it('restores the stored density on mount', () => {
    localStorage.setItem('density', 'comfortable');
    const { result } = renderHook(() => useTheme(), { wrapper: ThemeProvider });
    expect(result.current.density).toBe('comfortable');
    expect(document.documentElement).toHaveAttribute('data-density', 'comfortable');
  });

  it('ignores an unknown stored value', () => {
    localStorage.setItem('density', 'huge');
    const { result } = renderHook(() => useTheme(), { wrapper: ThemeProvider });
    expect(result.current.density).toBe('compact');
  });
});

describe('ThemeProvider', () => {
  it('applies and persists an explicit theme', () => {
    const { result } = renderHook(() => useTheme(), { wrapper: ThemeProvider });
    expect(result.current.theme).toBe('system');

    act(() => result.current.setTheme('dark'));
    expect(result.current.dark).toBe(true);
    expect(document.documentElement).toHaveClass('dark');
    expect(localStorage.getItem('theme')).toBe('dark');

    act(() => result.current.setTheme('light'));
    expect(result.current.dark).toBe(false);
    expect(document.documentElement).not.toHaveClass('dark');
    expect(localStorage.getItem('theme')).toBe('light');
  });

  it('restores the stored theme on mount', () => {
    localStorage.setItem('theme', 'dark');
    const { result } = renderHook(() => useTheme(), { wrapper: ThemeProvider });
    expect(result.current.theme).toBe('dark');
    expect(result.current.dark).toBe(true);
    expect(document.documentElement).toHaveClass('dark');
  });
});
