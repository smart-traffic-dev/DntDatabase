import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/react';
import { afterEach, vi } from 'vitest';

// Every command goes through `invoke`; tests drive it with `mockBackend`.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
  Channel: class {
    onmessage: (message: unknown) => void = () => {};
  },
}));

// Monaco cannot run in jsdom: a textarea that reports the language it was given.
vi.mock('@monaco-editor/react', async () => {
  const React = await import('react');
  return {
    default: (props: { value?: string; language?: string; onChange?: (value: string | undefined) => void }) =>
      React.createElement('textarea', {
        'data-testid': 'monaco-mock',
        'data-language': props.language,
        value: props.value ?? '',
        onChange: (e: { target: { value: string } }) => props.onChange?.(e.target.value),
      }),
    loader: { config: () => {} },
  };
});

// Glide draws on a canvas: a placeholder is enough.
vi.mock('@glideapps/glide-data-grid', async () => {
  const React = await import('react');
  return {
    default: () => React.createElement('div', { 'data-testid': 'glide-mock' }),
    DataEditor: () => React.createElement('div', { 'data-testid': 'glide-mock' }),
    GridCellKind: { Text: 'text' },
  };
});

// jsdom has no layout: give elements a size so the virtualized tree renders rows.
Object.defineProperty(HTMLElement.prototype, 'clientHeight', { configurable: true, value: 600 });
Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, value: 280 });
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};
Element.prototype.scrollTo = () => {};

afterEach(() => {
  cleanup();
});
