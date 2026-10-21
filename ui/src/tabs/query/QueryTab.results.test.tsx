import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { useLayout } from '../../store/layout';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { config, ids, mockBackend, resetStores } from '../../test/backend';
import { QueryTab } from './QueryTab';

beforeEach(() => {
  resetStores();
  localStorage.clear();
  useLayout.getState().reset();
  mockBackend({ tabs_save: () => null });
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { default_db: 'shop' })]);
});

describe('result panel flag (Ctrl+J)', () => {
  it('hides the result panel and the splitter, and the editor takes the full height', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    // A tab that ran once: its results are open (before that they are collapsed, QueryTab.split.test).
    useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, runId: 'r1' }]]) });
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    expect(screen.getByTestId('result-panel')).toBeInTheDocument();
    expect(screen.getByRole('separator', { name: 'Resize editor and results' })).toBeInTheDocument();
    const pane = document.querySelector<HTMLElement>('.query-editor-pane')!;
    expect(pane.style.flex).not.toMatch(/^1 1 0/);

    act(() => useLayout.getState().toggleResults());
    expect(screen.queryByTestId('result-panel')).toBeNull();
    expect(screen.queryByRole('separator', { name: 'Resize editor and results' })).toBeNull();
    expect(document.querySelector<HTMLElement>('.query-editor-pane')!.style.flex).toMatch(/^1 1 0/);
    // The editor stays mounted, so its text survives the toggle.
    expect(screen.getByTestId('query-editor')).toBeInTheDocument();

    act(() => useLayout.getState().toggleResults());
    expect(screen.getByTestId('result-panel')).toBeInTheDocument();
  });

  it('starts hidden when the stored layout says so', () => {
    localStorage.setItem('dnt.layout', JSON.stringify({ resultsVisible: false }));
    useLayout.setState({ resultsVisible: false });
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<QueryTab tab={{ id, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />);
    expect(screen.queryByTestId('result-panel')).toBeNull();
  });
});
