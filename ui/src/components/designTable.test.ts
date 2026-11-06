import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ObjectRef } from '../bindings';
import { useConnections } from '../store/connections';
import { objectTabId, useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { config, ids, resetStores, serverInfo } from '../test/backend';
import { canDesignNow, designCurrent, designTargetNow } from './designTable';
import { paletteCommands } from './paletteCommands';

const runEditorCommand = vi.hoisted(() => vi.fn(() => true));
const getActiveEditor = vi.hoisted(() => vi.fn<() => object | null>(() => null));
vi.mock('../tabs/query/activeEditor', async (orig) => ({
  ...(await orig<typeof import('../tabs/query/activeEditor')>()),
  runEditorCommand,
  getActiveEditor,
}));

const ref = (kind: ObjectRef['kind'], name: string): ObjectRef => ({ database: 'shop', schema: null, name, kind });

const connect = (designer = true) => {
  const info = serverInfo('MySql8');
  useConnections.setState({
    summaries: new Map([[ids.prod, config(ids.prod, 'prod')]]),
    info: new Map([[ids.prod, { ...info, capabilities: { ...info.capabilities, designer } }]]),
  });
};

const selectInTree = (o: ObjectRef) => {
  const id = `${connNodeId(ids.prod)}/db:shop/folder:${o.kind}/obj:${o.kind}:${o.name}`;
  useTree.setState({ nodes: new Map([[id, { id, kind: 'object', label: o.name, objectRef: o }]]), selectedId: id });
};

beforeEach(() => {
  resetStores();
  runEditorCommand.mockClear();
  getActiveEditor.mockReturnValue(null);
});

describe('Design table from anywhere', () => {
  it('takes the active table tab first, then the table selected in the tree', () => {
    connect();
    selectInTree(ref('Table', 'customers'));
    expect(designTargetNow()?.object.name).toBe('customers');
    const orders = ref('Table', 'orders');
    useTabs.getState().open({ id: objectTabId(ids.prod, orders), kind: 'object', connectionId: ids.prod, objectRef: orders, title: 'orders' });
    expect(designTargetNow()?.object.name).toBe('orders');
  });

  it('a view, a closed connection or no designer: nothing to design', () => {
    connect();
    selectInTree(ref('View', 'order_totals'));
    expect(designTargetNow()).toBeNull();
    selectInTree(ref('Table', 'orders'));
    connect(false);
    expect(designTargetNow()).toBeNull();
    useConnections.setState({ info: new Map() });
    expect(designTargetNow()).toBeNull();
    expect(canDesignNow()).toBe(false);
    expect(designCurrent()).toBe(false);
  });

  it('opens the designer of the target; in a SQL editor runs its F4 action', () => {
    connect();
    selectInTree(ref('Table', 'orders'));
    expect(designCurrent()).toBe(true);
    expect(useTabs.getState().tabs.find((t) => t.kind === 'design')).toMatchObject({ table: 'orders' });

    getActiveEditor.mockReturnValue({});
    designCurrent();
    expect(runEditorCommand).toHaveBeenCalledWith('dnt.designTable');
  });

  it('is in the command palette only when there is something to design', () => {
    const listed = () => paletteCommands().some((c) => c.id === 'table.design');
    expect(listed()).toBe(false);
    connect();
    selectInTree(ref('Table', 'orders'));
    expect(listed()).toBe(true);
    expect(paletteCommands().find((c) => c.id === 'table.design')).toMatchObject({ label: 'Table: Design table', hint: 'F4' });
  });
});
