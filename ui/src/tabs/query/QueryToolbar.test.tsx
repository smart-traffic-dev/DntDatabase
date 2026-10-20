import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { setHasSelection } from '../../store/runQuery';
import { config, ids, resetStores, serverInfo } from '../../test/backend';
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

  it('explain_is_enabled_with_a_connection', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    expect(screen.getByTestId('query-explain')).toBeEnabled();
  });

  it('analyze_disabled_on_mysql57', async () => {
    const user = userEvent.setup();
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql57', [5, 7, 44]));
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    await user.click(screen.getByTestId('query-explain-menu'));
    expect(screen.getByRole('menuitem', { name: 'Explain Analyze' })).toBeDisabled();
  });

  it('analyze_disabled_on_mysql8_before_0_18_but_enabled_after', async () => {
    const user = userEvent.setup();
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8', [8, 0, 17]));
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    await user.click(screen.getByTestId('query-explain-menu'));
    expect(screen.getByRole('menuitem', { name: 'Explain Analyze' })).toBeDisabled();
    act(() => useConnections.getState().setInfo(ids.prod, serverInfo('MySql8', [8, 0, 36])));
    expect(screen.getByRole('menuitem', { name: 'Explain Analyze' })).toBeEnabled();
  });
});
