import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { useObjectData } from '../store/objectData';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId, groupNodeId } from '../store/treeModel';
import { callsOf, ids, mockBackend, openResult, resetStores, sampleTree, serverInfo } from '../test/backend';
import { ConnectionTree } from './ConnectionTree';
import { Sidebar } from './Sidebar';

const prodNode = connNodeId(ids.prod);
const dbNode = `${prodNode}/db:shop`;
const tablesNode = `${dbNode}/folder:Table`;
const ordersNode = `${tablesNode}/obj:Table:orders`;

function backend() {
  return mockBackend({
    list_tree: () => sampleTree(),
    open_connection: () => openResult(serverInfo('MySql8')),
    list_databases: () => [{ name: 'shop' }, { name: 'blog' }],
    list_objects: () => [
      { name: 'customers', kind: 'Table', comment: null, row_estimate: null },
      { name: 'orders', kind: 'Table', comment: null, row_estimate: null },
    ],
    refresh_node: () => null,
    tabs_save: () => null,
  });
}

async function expandPath(...path: string[]) {
  for (const id of path) {
    act(() => useTree.getState().expand(id));
    await waitFor(() => expect(useTree.getState().childIds.has(id)).toBe(true));
  }
}

async function setup(ui: React.ReactElement = <ConnectionTree />) {
  backend();
  await useTree.getState().reloadStored();
  useTree.getState().expand(groupNodeId(ids.groupA));
  useTree.getState().expand(groupNodeId(ids.groupB));
  render(ui);
  await expandPath(prodNode, dbNode, tablesNode);
}

const select = (id: string) => act(() => useTree.getState().select(id));
const key = (k: string, init: KeyboardEventInit = {}) => fireEvent.keyDown(screen.getByRole('tree'), { key: k, ...init });

beforeEach(() => {
  resetStores();
  useObjectData.setState({ byTab: {} });
});

describe('ConnectionTree keys', () => {
  it('Enter opens a table on its Data tab, and expands a connection or database', async () => {
    await setup();
    select(ordersNode);
    key('Enter');
    const tab = useTabs.getState().tabs[0]!;
    expect(tab).toMatchObject({ kind: 'object', title: 'orders' });
    expect(useObjectData.getState().byTab[tab.id]?.sub).toBeUndefined();

    select(`${prodNode}/db:blog`);
    key('Enter');
    expect(useTree.getState().expanded.has(`${prodNode}/db:blog`)).toBe(true);
    await waitFor(() => expect(useTree.getState().childIds.has(`${prodNode}/db:blog`)).toBe(true));
    select(prodNode);
    key('Enter');
    expect(useTree.getState().expanded.has(prodNode)).toBe(false);
    key('Enter');
    expect(useTree.getState().expanded.has(prodNode)).toBe(true);
  });

  it('Ctrl+Enter opens the object on its DDL tab', async () => {
    await setup();
    select(ordersNode);
    key('Enter', { ctrlKey: true });
    const tab = useTabs.getState().tabs[0]!;
    expect(tab.title).toBe('orders');
    expect(useObjectData.getState().byTab[tab.id]?.sub).toBe('ddl');
    // On a non-object node Ctrl+Enter does nothing.
    select(dbNode);
    key('Enter', { ctrlKey: true });
    expect(useTabs.getState().tabs).toHaveLength(1);
  });

  it('Ctrl+Q opens a new query bound to the selected node\'s connection and database', async () => {
    await setup();
    select(ordersNode);
    key('q', { ctrlKey: true });
    const id = useTabs.getState().activeId!;
    expect(useQueryDocs.getState().docs.get(id)).toMatchObject({ connectionId: ids.prod, database: 'shop' });
    select(prodNode);
    key('q', { ctrlKey: true });
    expect(useQueryDocs.getState().docs.get(useTabs.getState().activeId!)?.connectionId).toBe(ids.prod);
    expect(useTabs.getState().tabs).toHaveLength(2);
  });

  it('F2 renames: the edit dialog of a connection, the rename dialog of a group, nothing for an object', async () => {
    await setup();
    select(prodNode);
    key('F2');
    expect(useDialogs.getState().request).toEqual({ kind: 'editConnection', id: ids.prod });
    select(groupNodeId(ids.groupA));
    key('F2');
    expect(useDialogs.getState().request).toEqual({ kind: 'renameGroup', id: ids.groupA });
    useDialogs.getState().closeRequest();
    select(ordersNode);
    key('F2');
    expect(useDialogs.getState().request).toBeNull();
  });

  it('F5 refreshes the selected node', async () => {
    await setup();
    select(tablesNode);
    key('F5');
    await waitFor(() => expect(callsOf('refresh_node')).toHaveLength(1));
  });

  it('Right expands then goes to the first child; Left goes to the parent, or collapses', async () => {
    await setup();
    select(dbNode);
    key('ArrowLeft');
    expect(useTree.getState().expanded.has(dbNode)).toBe(false);
    key('ArrowLeft');
    expect(useTree.getState().selectedId).toBe(prodNode);
    select(dbNode);
    key('ArrowRight');
    expect(useTree.getState().expanded.has(dbNode)).toBe(true);
    await waitFor(() => expect(useTree.getState().childIds.has(dbNode)).toBe(true));
    key('ArrowRight');
    expect(useTree.getState().selectedId).toBe(tablesNode);
  });

  it('other Ctrl combinations are left alone', async () => {
    await setup();
    select(ordersNode);
    const before = fireEvent.keyDown(screen.getByRole('tree'), { key: 'a', ctrlKey: true });
    expect(before).toBe(true);
    expect(useTree.getState().filter).toBe('');
  });
});

describe('typing in the tree', () => {
  it('puts the typed text in the sidebar filter and focuses it', async () => {
    await setup(<Sidebar />);
    const filter = screen.getByRole('searchbox', { name: 'Filter objects' });
    select(ordersNode);
    screen.getByRole('tree').focus();
    expect(key('c')).toBe(false); // default prevented: the character is not typed twice
    expect(useTree.getState().filter).toBe('c');
    expect(filter).toHaveFocus();
    expect(filter).toHaveValue('c');
    // More keys typed while the tree still has the focus append.
    screen.getByRole('tree').focus();
    key('u');
    expect(useTree.getState().filter).toBe('cu');
  });

  it('ignores Space without a filter, modifier combinations and non-printing keys', async () => {
    await setup(<Sidebar />);
    select(ordersNode);
    key(' ');
    key('c', { ctrlKey: true });
    key('Shift');
    key('Tab');
    expect(useTree.getState().filter).toBe('');
  });
});
