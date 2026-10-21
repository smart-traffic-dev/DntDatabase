import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { config, ids, mockBackend, resetStores } from '../../test/backend';
import { QueryToolbar } from './QueryToolbar';

const rect = (left: number, width: number) => ({ left, right: left + width, width, top: 0, bottom: 30, height: 30, x: left, y: 0, toJSON() {} }) as DOMRect;

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  mockBackend({ list_databases: () => [{ name: 'shop' }], tabs_save: () => null, query_tx: () => 'idle' });
});
afterEach(() => vi.restoreAllMocks());

describe('context pill picker in a narrow group', () => {
  it('shifts left to stay inside the query tab instead of being cut off', async () => {
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
      if (this.classList.contains('query-tab')) return rect(0, 360);
      if (this.classList.contains('qt-popover')) return rect(130, 260); // right edge 390 > 360
      return rect(0, 0);
    });
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(
      <div className="query-tab">
        <QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />
      </div>,
    );
    await userEvent.setup().click(screen.getByTestId('query-context'));
    expect(screen.getByRole('dialog', { name: 'Connection and database' }).style.transform).toBe('translateX(-38px)');
  });

  it('stays put when it fits', async () => {
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
      if (this.classList.contains('query-tab')) return rect(0, 1000);
      if (this.classList.contains('qt-popover')) return rect(130, 260);
      return rect(0, 0);
    });
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(
      <div className="query-tab">
        <QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />
      </div>,
    );
    await userEvent.setup().click(screen.getByTestId('query-context'));
    expect(screen.getByRole('dialog', { name: 'Connection and database' }).style.transform).toBe('');
  });
});
