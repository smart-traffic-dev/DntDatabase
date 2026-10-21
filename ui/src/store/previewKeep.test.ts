import { beforeEach, describe, expect, it } from 'vitest';
import { resetStores } from '../test/backend';
import { useDataEdits } from './dataEdits';
import { useTableData } from './tableData';
import { useTabs, type ObjectTabRef } from './tabs';

const tab: ObjectTabRef = { id: 'p', kind: 'object', connectionId: 'c', title: 'p', objectRef: { database: 'd', schema: null, name: 'p', kind: 'Table' } };
const preview = () => useTabs.getState().tabs.find((t) => t.id === 'p')?.preview;

beforeEach(() => {
  resetStores();
  useTabs.getState().openPreview(tab);
});

describe('working in a preview tab keeps it', () => {
  it('sorting', () => {
    expect(preview()).toBe(true);
    useTableData.getState().toggleSort(tab, 'id');
    expect(preview()).toBe(false);
  });

  it('filtering', () => {
    useTableData.getState().applyFilter(tab, null);
    expect(preview()).toBe(false);
  });

  it('editing a cell', () => {
    useDataEdits.getState().stage('p', { kind: 'set', row: 0, column: 'id', value: '1' } as never);
    expect(preview()).toBe(false);
  });
});
