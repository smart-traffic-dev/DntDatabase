import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useConnections } from '../store/connections';
import { useObjectData } from '../store/objectData';
import { config, mockBackend, resetStores, ids } from '../test/backend';
import { objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import type { ObjectRef } from '../bindings';
import { ObjectTab } from './ObjectTab';

// A Monaco stand-in that exposes the options and language it was created with.
vi.mock('@monaco-editor/react', async () => {
  const React = await import('react');
  return {
    default: (props: { value?: string; options?: unknown; language?: string }) =>
      React.createElement('div', { 'data-testid': 'monaco-options', 'data-options': JSON.stringify(props.options), 'data-value': props.value, 'data-language': props.language }),
    loader: { config: () => {} },
  };
});

const tabOf = (objectRef: ObjectRef): ObjectTabRef => ({ id: objectTabId(ids.prod, objectRef), kind: 'object', connectionId: ids.prod, objectRef, title: objectRef.name });

describe('ObjectTab DDL editor', () => {
  beforeEach(() => resetStores());

  it('is_created_read_only', async () => {
    mockBackend({ object_ddl: () => 'CREATE FUNCTION f()' });
    const tab = tabOf({ database: 'shop', schema: null, name: 'f', kind: 'Function' });
    useTabs.getState().open(tab);
    render(<ObjectTab tab={tab} />);
    const el = await screen.findByTestId('monaco-options');
    expect(JSON.parse(el.dataset.options!)).toMatchObject({ readOnly: true });
    expect(el.dataset.value).toBe('CREATE FUNCTION f()');
  });

  it('highlights SQL by the connection engine', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    mockBackend({ object_ddl: () => 'CREATE FUNCTION f()' });
    const tab = tabOf({ database: 'shop', schema: 'public', name: 'f', kind: 'Function' });
    useTabs.getState().open(tab);
    render(<ObjectTab tab={tab} />);
    expect((await screen.findByTestId('monaco-options')).dataset.language).toBe('pgsql');
  });

  it('a Collection DDL (JSON) is highlighted as JSON, not SQL', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'docs', { kind: 'mongo', port: 27017 })]);
    mockBackend({ object_ddl: () => '{ "validator": {} }' });
    const tab = tabOf({ database: 'shop', schema: null, name: 'orders', kind: 'Collection' });
    useTabs.getState().open(tab);
    useObjectData.getState().setSub(tab.id, 'ddl');
    render(<ObjectTab tab={tab} />);
    const el = await screen.findByTestId('monaco-options');
    expect(el.dataset.value).toBe('{ "validator": {} }');
    expect(el.dataset.language).toBe('json');
  });
});
