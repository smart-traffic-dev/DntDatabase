import { fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { EDITOR_RATIO_DEFAULT, useLayout } from '../../store/layout';
import { useQueryDocs } from '../../store/queryTabs';
import { config, ids, resetStores } from '../../test/backend';
import { QueryTab } from './QueryTab';

beforeEach(() => {
  resetStores();
  useLayout.getState().reset();
  localStorage.clear();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
});

describe('editor / results split', () => {
  it('sizes the editor from the layout store and the splitter moves it', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    const bar = screen.getByRole('separator', { name: 'Resize editor and results' });
    // jsdom reports a 600 px body: 60 % is 360 px.
    expect(bar).toHaveAttribute('aria-valuenow', '360');
    expect(bar.previousElementSibling).toHaveStyle({ flex: '0 0 360px' });
    fireEvent.keyDown(bar, { key: 'ArrowDown' });
    expect(bar).toHaveAttribute('aria-valuenow', '376');
    expect(useLayout.getState().editorRatio).toBeCloseTo(376 / 600);
    fireEvent.keyDown(bar, { key: 'End' });
    // The result panel keeps at least 120 px.
    expect(bar).toHaveAttribute('aria-valuenow', '480');
    fireEvent.doubleClick(bar);
    expect(useLayout.getState().editorRatio).toBeCloseTo(EDITOR_RATIO_DEFAULT);
  });
});
