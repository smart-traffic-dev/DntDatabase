import { act, renderHook } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { useTokenPx } from './useTokenPx';

const root = document.documentElement;

afterEach(() => {
  root.style.removeProperty('--t-row');
  root.removeAttribute('data-density');
});

describe('useTokenPx', () => {
  it('reads a px custom property, else the fallback', () => {
    const { result } = renderHook(() => useTokenPx('--t-row', 24));
    expect(result.current).toBe(24);
    root.style.setProperty('--t-row', '28px');
    const again = renderHook(() => useTokenPx('--t-row', 24));
    expect(again.result.current).toBe(28);
  });

  it('re-reads when the density changes', async () => {
    root.style.setProperty('--t-row', '24px');
    const { result } = renderHook(() => useTokenPx('--t-row', 10));
    expect(result.current).toBe(24);
    await act(async () => {
      root.style.setProperty('--t-row', '28px');
      root.setAttribute('data-density', 'comfortable');
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(result.current).toBe(28);
  });
});
