import { act, fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { EDITOR_RATIO_DEFAULT, useLayout } from '../../store/layout';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
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
    await userEvent.setup().click(screen.getByTestId('panel-history'));
    expect(separator()).toBeInTheDocument();
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
