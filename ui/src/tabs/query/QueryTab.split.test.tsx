import { act, fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { EDITOR_RATIO_DEFAULT, useLayout } from '../../store/layout';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { useTabs } from '../../store/tabs';
import { config, ids, mockBackend, resetStores } from '../../test/backend';
import { QueryTab } from './QueryTab';

beforeEach(() => {
  resetStores();
  useLayout.getState().reset();
  useQueryRun.setState({ runs: new Map() });
  localStorage.clear();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
});

const ran = (id: string) => act(() => useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, runId: 'r1' }]]) }));
const separator = () => screen.queryByRole('separator', { name: 'Resize editor and results' });

describe('editor / results split', () => {
  it('collapses the results to their tab bar until the first run, showing the run keys', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    expect(separator()).toBeNull();
    const panel = screen.getByTestId('result-panel');
    expect(panel).toHaveClass('collapsed');
    expect(document.querySelector<HTMLElement>('.query-editor-pane')!.style.flex).toMatch(/^1 1 0/);
    const keys = within(panel).getByTestId('result-empty-keys');
    expect([...keys.querySelectorAll('.keycap')].map((k) => k.textContent)).toEqual([
      'Ctrl',
      'Enter',
      'Ctrl',
      'Shift',
      'Enter',
      'Ctrl',
      'Shift',
      'E',
    ]);
    expect(keys).toHaveTextContent(/Run statement.*Run all.*Explain/);
    // The first run opens the panel.
    ran(id);
    expect(screen.getByTestId('result-panel')).not.toHaveClass('collapsed');
    expect(separator()).toBeInTheDocument();
    expect(screen.queryByTestId('result-empty-keys')).toBeNull();
  });

  it('opens when one of its tabs is clicked before any run', async () => {
    mockBackend({ history_list: () => [] });
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    expect(separator()).toBeNull();
    await userEvent.setup().click(screen.getByTestId('panel-messages'));
    expect(separator()).toBeInTheDocument();
    expect(screen.getByTestId('result-panel')).not.toHaveClass('collapsed');
  });

  it('keeps the visibility and split per editor group', () => {
    const a = useQueryDocs.getState().newQuery(ids.prod, null);
    const b = useQueryDocs.getState().newQuery(ids.prod, null);
    act(() => useQueryRun.setState({ runs: new Map([[a, { ...emptyRun, runId: 'r1' }], [b, { ...emptyRun, runId: 'r2' }]]) }));
    act(() => useTabs.setState({ groups: [{ id: 'g1' }, { id: 'g2' }], groupOf: { [a]: 'g1', [b]: 'g2' } }));
    act(() => useLayout.getState().setGroupRatio('g2', 0.25));
    act(() => useLayout.getState().toggleGroupResults('g1'));
    render(
      <>
        <QueryTab tab={{ id: a, kind: 'query', connectionId: ids.prod, title: 'A' }} />
        <QueryTab tab={{ id: b, kind: 'query', connectionId: ids.prod, title: 'B' }} />
      </>,
    );
    // g1 hid its results; g2 keeps them, at its own ratio.
    expect(screen.getAllByTestId('result-panel')).toHaveLength(1);
    const bar = screen.getByRole('separator', { name: 'Resize editor and results' });
    expect(bar).toHaveAttribute('aria-valuenow', '150');
    fireEvent.keyDown(bar, { key: 'ArrowDown' });
    expect(useLayout.getState().groupResults['g2']?.ratio).toBeCloseTo(166 / 600);
    expect(useLayout.getState().groupResults['g1']?.ratio).toBeCloseTo(EDITOR_RATIO_DEFAULT);
  });

  it('maximizes the results (editor folds, no splitter) and restores the ratio', async () => {
    const user = userEvent.setup();
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    ran(id);
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    const pane = document.querySelector<HTMLElement>('.query-editor-pane')!;
    expect(pane).toHaveStyle({ flex: '0 0 360px' });
    const toggle = screen.getByTestId('result-maximize');
    expect(toggle).toHaveAccessibleName('Maximize results');
    expect(toggle).toHaveAttribute('title', expect.stringContaining('Ctrl+Shift+J'));
    await user.click(toggle);
    expect(pane).toHaveAttribute('hidden');
    expect(separator()).toBeNull();
    expect(screen.getByTestId('result-maximize')).toHaveAccessibleName('Restore results');
    await user.click(screen.getByTestId('result-maximize'));
    expect(pane).not.toHaveAttribute('hidden');
    expect(pane).toHaveStyle({ flex: '0 0 360px' });
    expect(separator()).toBeInTheDocument();
  });

  it('maximizing a tab that never ran opens its results', async () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    expect(screen.getByTestId('result-panel')).toHaveClass('collapsed');
    await userEvent.setup().click(screen.getByTestId('result-maximize'));
    expect(screen.getByTestId('result-panel')).not.toHaveClass('collapsed');
  });

  it('after the first run sizes the editor from the layout store and the splitter moves it', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    ran(id);
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
