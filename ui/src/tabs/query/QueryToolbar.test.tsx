import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { setHasSelection } from '../../store/runQuery';
import { config, ids, resetStores } from '../../test/backend';
import { QueryToolbar } from './QueryToolbar';

describe('QueryToolbar', () => {
  beforeEach(() => {
    resetStores();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  });

  it('run_selection_needs_a_selection', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    expect(screen.getByTestId('query-run')).toBeEnabled();
    expect(screen.getByTestId('query-run-selection')).toBeDisabled();
    act(() => setHasSelection(id, true));
    expect(screen.getByTestId('query-run-selection')).toBeEnabled();
    act(() => setHasSelection(id, false));
    expect(screen.getByTestId('query-run-selection')).toBeDisabled();
  });
});
