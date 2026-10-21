import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent, { type UserEvent } from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useConnections } from '../../store/connections';
import { setExporting } from '../../store/queryExport';
import { emptyRun, startRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { setHasSelection } from '../../store/runQuery';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { setActiveEditor } from './activeEditor';
import { QueryToolbar } from './QueryToolbar';

const PLAN = {
  plan_id: 'p1',
  stmts: [{ index: 0, range: [0, 8] as [number, number], kind: 'read' as const, verdict: 'allow' as const, preview: 'S0' }],
  dialect: { engine: 'mysql' as const },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle' as const,
};

/** The toolbar measures itself: a ResizeObserver that reports `width` once observing. */
function stubWidth(width: number) {
  class FixedWidth {
    constructor(private readonly cb: ResizeObserverCallback) {}
    observe() {
      this.cb([{ contentRect: { width } } as ResizeObserverEntry], this as unknown as ResizeObserver);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal('ResizeObserver', FixedWidth);
}

const openRunMenu = (user: UserEvent) => user.click(screen.getByTestId('query-run-menu'));
const openContext = (user: UserEvent) => user.click(screen.getByTestId('query-context'));

describe('QueryToolbar', () => {
  beforeEach(() => {
    resetStores();
    useQueryRun.setState({ runs: new Map() });
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  });
  afterEach(() => vi.unstubAllGlobals());

  it('run_selection_needs_a_selection', async () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    expect(screen.getByTestId('query-run')).toBeEnabled();
    await openRunMenu(userEvent.setup());
    expect(screen.getByTestId('query-run-selection')).toBeDisabled();
    act(() => setHasSelection(id, true));
    expect(screen.getByTestId('query-run-selection')).toBeEnabled();
    act(() => setHasSelection(id, false));
    expect(screen.getByTestId('query-run-selection')).toBeDisabled();
  });

  it('run_all_runs_every_statement_and_says_why_it_is_disabled', async () => {
    const user = userEvent.setup();
    const none = useQueryDocs.getState().newQuery(null, null);
    const view = render(<QueryToolbar doc={useQueryDocs.getState().docs.get(none)!} />);
    await openRunMenu(user);
    expect(screen.getByTestId('query-run-all')).toBeDisabled();
    expect(screen.getByTestId('query-run-all')).toHaveAttribute('title', 'Choose a connection to run queries');
    expect(screen.getByTestId('query-run-selection')).toHaveAttribute('title', 'Choose a connection to run queries');
    view.unmount();
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    expect(screen.getByTestId('query-run')).toHaveTextContent('Ctrl+Enter');
    await openRunMenu(user);
    expect(screen.getByTestId('query-run-all')).toBeEnabled();
    expect(screen.getByTestId('query-run-all')).toHaveTextContent('Ctrl+Shift+Enter');
    expect(screen.getByTestId('query-run-selection')).toHaveAttribute('title', 'Select text in the editor to run only that');
  });

  it('the run menu holds run all, run selection, continue on error and add to favorites', async () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    for (const gone of ['Run all', 'Run selection', 'Add to favorites']) expect(screen.queryByRole('button', { name: gone })).toBeNull();
    expect(screen.queryByTestId('query-continue-on-error')).toBeNull();
    const user = userEvent.setup();
    await openRunMenu(user);
    const menu = screen.getByRole('menu', { name: 'Run options' });
    expect([...menu.querySelectorAll('[role^="menuitem"]')].map((i) => i.querySelector('.menu-label')?.textContent)).toEqual([
      'Run all',
      'Run selection',
      'Continue on error',
      'Add to favorites',
    ]);
    expect(screen.getByTestId('query-add-favorite')).toHaveTextContent('Ctrl+Shift+D');
    const toggle = screen.getByTestId('query-continue-on-error');
    expect(toggle).toHaveAttribute('role', 'menuitemcheckbox');
    expect(toggle).toHaveAttribute('aria-checked', 'false');
    await user.click(toggle);
    expect(useQueryDocs.getState().docs.get(id)!.continueOnError).toBe(true);
    // Esc closes the menu and gives the focus back to the caret.
    await openRunMenu(user);
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('menu')).toBeNull();
    expect(screen.getByTestId('query-run-menu')).toHaveFocus();
  });

  it('a production connection runs from a danger outline button labelled Run on PROD', () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    const run = screen.getByTestId('query-run');
    expect(run).toHaveTextContent('Run on PROD');
    expect(run).toHaveClass('danger-outline');
    expect(run).not.toHaveClass('qt-primary');
  });

  it('a running statement turns the Run slot into a danger Stop', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    expect(screen.getByTestId('query-run')).toHaveClass('qt-primary');
    expect(screen.queryByTestId('query-stop')).toBeNull();
    act(() => useQueryRun.setState({ runs: new Map([[id, startRun(emptyRun, PLAN as never, 0)]]) }));
    expect(screen.queryByTestId('query-run')).toBeNull();
    const stop = screen.getByTestId('query-stop');
    expect(stop).toBeEnabled();
    expect(stop).toHaveClass('danger');
    expect(stop).toHaveAttribute('title', 'Stop (Esc)');
    act(() => useQueryRun.setState({ runs: new Map() }));
    expect(screen.getByTestId('query-run')).toBeEnabled();
  });

  it('the second click of a double-click on Run does not stop the run it started; focus stays on the slot', () => {
    vi.useFakeTimers();
    try {
      const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
      render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
      mockBackend({ query_cancel: () => null });
      const run = screen.getByTestId('query-run');
      run.focus();
      act(() => useQueryRun.setState({ runs: new Map([[id, { ...startRun(emptyRun, PLAN as never, 0), runId: 'r1' }]]) }));
      const stop = screen.getByTestId('query-stop');
      // Same element: the keyboard focus is not lost when Run turns into Stop.
      expect(stop).toBe(run);
      expect(stop).toHaveFocus();
      fireEvent.click(stop, { detail: 2 });
      fireEvent.click(stop, { detail: 1 });
      expect(callsOf('query_cancel')).toHaveLength(0);
      act(() => void vi.advanceTimersByTime(500));
      fireEvent.click(stop, { detail: 1 });
      expect(callsOf('query_cancel')).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('an_export_of_the_tabs_result_makes_it_busy_and_stoppable', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    expect(screen.queryByTestId('query-stop')).toBeNull();
    act(() => setExporting(id, 'run-1'));
    expect(screen.queryByTestId('query-run')).toBeNull();
    expect(screen.getByTestId('query-stop')).toBeEnabled();
    expect(screen.getByTestId('query-stop')).toHaveAttribute('title', 'Stop the export (Esc)');
    // Explain says why it cannot run meanwhile.
    expect(screen.getByTestId('query-explain')).toHaveAttribute('title', 'An export is reading this tab’s result — stop it first');
    act(() => setExporting(id, null));
    expect(screen.getByTestId('query-run')).toBeEnabled();
    expect(screen.queryByTestId('query-stop')).toBeNull();
  });

  it('Format formats the editor of the tab', async () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    const editor = { focus: vi.fn(), trigger: vi.fn() };
    setActiveEditor(id, editor);
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    const format = screen.getByTestId('query-format');
    expect(format).toHaveAttribute('title', 'Format SQL (Ctrl+Shift+F)');
    await userEvent.setup().click(format);
    expect(editor.trigger).toHaveBeenCalledWith('toolbar', 'dnt.formatSql', null);
    setActiveEditor(id, null);
  });

  describe('context pill', () => {
    it('shows the environment, connection and database and opens the picker', async () => {
      useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
      const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
      render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
      const pill = screen.getByTestId('query-context');
      expect(within(pill).getByTestId('query-context-env')).toHaveTextContent('PROD');
      expect(pill).toHaveTextContent('shop-prod');
      expect(pill).toHaveTextContent('shop');
      expect(pill).toHaveAttribute('aria-haspopup', 'dialog');
      expect(screen.queryByTestId('query-connection')).toBeNull();
      const user = userEvent.setup();
      await openContext(user);
      const picker = screen.getByRole('dialog', { name: 'Connection and database' });
      expect(within(picker).getByTestId('query-connection')).toHaveFocus();
      expect(within(picker).getByTestId('query-database')).toHaveValue('shop');
      expect(pill).toHaveAttribute('aria-expanded', 'true');
      await user.keyboard('{Escape}');
      expect(screen.queryByRole('dialog')).toBeNull();
      expect(pill).toHaveFocus();
    });

    it('without a connection it asks for one', () => {
      const id = useQueryDocs.getState().newQuery(null, null);
      render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
      expect(screen.getByTestId('query-context')).toHaveTextContent('Choose a connection');
      expect(screen.queryByTestId('query-context-env')).toBeNull();
    });

    it('choosing a connection in the picker binds the tab', async () => {
      useConnections.getState().setStored([], [config(ids.prod, 'shop-prod'), config(ids.analytics, 'analytics')]);
      const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
      render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
      const user = userEvent.setup();
      await openContext(user);
      await user.selectOptions(screen.getByTestId('query-connection'), ids.analytics);
      expect(useQueryDocs.getState().docs.get(id)).toMatchObject({ connectionId: ids.analytics, database: null });
    });
  });

  it('lists the databases of an open connection whose tree node was never expanded', async () => {
    mockBackend({ list_databases: () => [{ name: 'shop' }, { name: 'blog' }] });
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    await openContext(userEvent.setup());
    const select = screen.getByTestId('query-database');
    await waitFor(() => expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual(['Default database', 'shop', 'blog']));
    expect(callsOf('list_databases')).toEqual([{ id: ids.prod, showSystem: false }]);
    expect(select).not.toHaveAttribute('title');
  });

  it('does not list databases of a closed connection and says why', async () => {
    mockBackend({});
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
    await openContext(userEvent.setup());
    expect(callsOf('list_databases')).toEqual([]);
    expect(screen.getByTestId('query-database')).toHaveAttribute('title', 'Open the connection to list all its databases');
    expect(screen.getByTestId('query-explain-menu')).toHaveAttribute('title', 'Explain options');
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

  describe('narrow toolbar', () => {
    const mount = (env: 'production' | null = null) => {
      useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: env })]);
      const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
      render(<QueryToolbar doc={useQueryDocs.getState().docs.get(id)!} />);
      return id;
    };

    it('keeps Explain, Format and Limit in the toolbar when wide', () => {
      stubWidth(900);
      mount();
      expect(screen.getByTestId('query-explain')).toBeInTheDocument();
      expect(screen.getByTestId('query-format')).toBeInTheDocument();
      expect(screen.getByTestId('query-row-limit')).toBeInTheDocument();
      expect(screen.queryByTestId('query-more')).toBeNull();
    });

    it('under 560 px moves Explain, Format and Limit into the more menu', async () => {
      stubWidth(500);
      const id = mount();
      expect(screen.queryByTestId('query-explain')).toBeNull();
      expect(screen.queryByTestId('query-format')).toBeNull();
      expect(screen.queryByTestId('query-row-limit')).toBeNull();
      expect(screen.getByTestId('query-run')).toHaveTextContent('Run');
      const more = screen.getByTestId('query-more');
      expect(more).toHaveAttribute('aria-label', 'More query actions');
      expect(more).toHaveAttribute('title', 'More query actions');
      const user = userEvent.setup();
      await user.click(more);
      const menu = screen.getByRole('menu', { name: 'More query actions' });
      const labels = [...menu.querySelectorAll('[role^="menuitem"]')].map((i) => i.querySelector('.menu-label')?.textContent);
      expect(labels).toEqual(['Explain', 'Explain Analyze', 'Format SQL', 'Limit 1,000', 'Limit 10,000', 'Limit 100,000', 'Limit 1,000,000']);
      expect(within(menu).getByRole('menuitemradio', { name: /Limit 10,000$/ })).toHaveAttribute('aria-checked', 'true');
      await user.click(within(menu).getByRole('menuitemradio', { name: /Limit 1,000$/ }));
      expect(useQueryDocs.getState().docs.get(id)!.rowLimit).toBe(1000);
    });

    it('under 400 px Run is an icon with its name and shortcut, and the environment stays', () => {
      stubWidth(380);
      mount('production');
      const run = screen.getByTestId('query-run');
      expect(run).toHaveAttribute('aria-label', 'Run on PROD');
      expect(run).toHaveAttribute('title', 'Run the statement at the cursor (Ctrl+Enter)');
      expect(run).not.toHaveTextContent('Run');
      expect(screen.getByTestId('query-context-env')).toHaveTextContent('PROD');
    });
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
