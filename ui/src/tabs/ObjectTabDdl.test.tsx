import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { mockBackend, resetStores, ids } from '../test/backend';
import { objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import { ObjectTab } from './ObjectTab';

// A Monaco stand-in that exposes the options it was created with.
vi.mock('@monaco-editor/react', async () => {
  const React = await import('react');
  return {
    default: (props: { value?: string; options?: unknown }) =>
      React.createElement('div', { 'data-testid': 'monaco-options', 'data-options': JSON.stringify(props.options), 'data-value': props.value }),
    loader: { config: () => {} },
  };
});

describe('ObjectTab DDL editor', () => {
  it('is_created_read_only', async () => {
    resetStores();
    mockBackend({ object_ddl: () => 'CREATE FUNCTION f()' });
    const objectRef = { database: 'shop', schema: null, name: 'f', kind: 'Function' as const };
    const tab: ObjectTabRef = { id: objectTabId(ids.prod, objectRef), kind: 'object', connectionId: ids.prod, objectRef, title: 'f' };
    useTabs.getState().open(tab);
    render(<ObjectTab tab={tab} />);
    const el = await screen.findByTestId('monaco-options');
    expect(JSON.parse(el.dataset.options!)).toMatchObject({ readOnly: true });
    expect(el.dataset.value).toBe('CREATE FUNCTION f()');
  });
});