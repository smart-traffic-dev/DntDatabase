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

  describe('explain menu keyboard', () => {
    const open = async () => {
      const user = userEvent.setup();
      useConnections.getState().setInfo(ids.prod, serverInfo('MySql8', [8, 0, 36]));
      const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
      render(
        <>
          <QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />
          <button type="button">elsewhere</button>
        </>,
      );
      await user.click(screen.getByTestId('query-explain-menu'));
      return user;
    };
    const items = () => screen.getAllByRole('menuitem');

    it('focus moves into the menu on open and arrows cycle the items', async () => {
      const user = await open();
      expect(items().map((i) => i.textContent)).toEqual(['Explain', 'Explain Analyze']);
      expect(items()[0]).toHaveFocus();
      await user.keyboard('{ArrowDown}');
      expect(items()[1]).toHaveFocus();
      await user.keyboard('{ArrowDown}');
      expect(items()[0]).toHaveFocus();
      await user.keyboard('{ArrowUp}');
      expect(items()[1]).toHaveFocus();
      await user.keyboard('{Home}');
      expect(items()[0]).toHaveFocus();
      await user.keyboard('{End}');
      expect(items()[1]).toHaveFocus();
    });

    it('Escape closes and gives focus back to the caret', async () => {
      const user = await open();
      await user.keyboard('{Escape}');
      expect(screen.queryByRole('menu')).toBeNull();
      expect(screen.getByTestId('query-explain-menu')).toHaveFocus();
      expect(screen.getByTestId('query-explain-menu')).toHaveAttribute('aria-expanded', 'false');
    });

    it('an outside click closes it', async () => {
      const user = await open();
      await user.click(screen.getByRole('button', { name: 'elsewhere' }));
      expect(screen.queryByRole('menu')).toBeNull();
    });

    it('focus leaving the menu closes it', async () => {
      const user = await open();
      await user.tab();
      expect(screen.queryByRole('menu')).toBeNull();
    });

    it('ArrowDown on the caret opens the menu', async () => {
      const user = await open();
      await user.keyboard('{Escape}');
      await user.keyboard('{ArrowDown}');
      expect(screen.getByRole('menu')).toBeInTheDocument();
      expect(items()[0]).toHaveFocus();
    });

    it('a disabled Explain Analyze is skipped by the arrows', async () => {
      const user = userEvent.setup();
      useConnections.getState().setInfo(ids.prod, serverInfo('MySql57', [5, 7, 44]));
      const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
      render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
      await user.click(screen.getByTestId('query-explain-menu'));
      expect(items()[0]).toHaveFocus();
      await user.keyboard('{ArrowDown}');
      expect(items()[0]).toHaveFocus();
    });
  });
});
