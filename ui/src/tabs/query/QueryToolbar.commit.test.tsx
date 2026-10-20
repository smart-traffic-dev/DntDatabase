import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { useTxMode } from '../../store/txMode';
import { callsOf, config, fail, ids, mockBackend, resetStores } from '../../test/backend';
import { QueryToolbar } from './QueryToolbar';

describe('QueryToolbar commit mode', () => {
  let id: string;
  beforeEach(() => {
    resetStores();
    useQueryRun.setState({ runs: new Map() });
    useTxMode.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  });
  const radio = (name: string) => screen.getByRole('radio', { name });
  const doc = () => useQueryDocs.getState().docs.get(id)!;

  it('manual_toggle_calls_tx_mode', async () => {
    mockBackend({ query_tx_mode: () => 'idle' });
    render(<QueryToolbar doc={doc()} />);
    expect(radio('Auto')).toHaveAttribute('aria-checked', 'true');
    expect(radio('Manual')).toHaveAttribute(
      'title',
      expect.stringContaining('Procedures that commit internally need Auto mode.'),
    );
    const user = userEvent.setup();
    await user.click(radio('Manual'));
    expect(callsOf('query_tx_mode')).toEqual([{ tabId: id, mode: 'Manual' }]);
    expect(radio('Manual')).toHaveAttribute('aria-checked', 'true');
    await user.click(radio('Auto'));
    expect(callsOf('query_tx_mode')[1]).toEqual({ tabId: id, mode: 'Auto' });
    expect(radio('Auto')).toHaveAttribute('aria-checked', 'true');
  });

  it('toggle_disabled_with_open_manual_tx', () => {
    mockBackend({});
    act(() => useTxMode.getState().patch(id, { mode: 'Manual' }));
    render(<QueryToolbar doc={doc()} />);
    expect(radio('Auto')).toBeEnabled();
    act(() => useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, txState: 'active' }]]) }));
    expect(radio('Auto')).toBeDisabled();
    expect(radio('Manual')).toBeDisabled();
    expect(radio('Auto')).toHaveAttribute('title', 'Commit or roll back the open transaction first');
  });

  it('a_refused_switch_to_auto_keeps_manual_and_shows_the_error', async () => {
    mockBackend({
      query_tx_mode: () => {
        throw fail('transaction_open');
      },
    });
    act(() => useTxMode.getState().patch(id, { mode: 'Manual' }));
    render(<QueryToolbar doc={doc()} />);
    await userEvent.setup().click(radio('Auto'));
    expect(radio('Manual')).toHaveAttribute('aria-checked', 'true');
    expect(useQueryRun.getState().get(id).messages.at(-1)?.error?.kind).toBe('transaction_open');
  });
});
