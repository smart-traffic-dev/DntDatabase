import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { FavoriteInput } from '../bindings';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useFavorites } from '../store/favorites';
import { useQueryDocs } from '../store/queryTabs';
import { QueryToolbar } from '../tabs/query/QueryToolbar';
import { appError, callsOf, config, Failure, ids, mockBackend, resetStores } from '../test/backend';
import { fav } from '../test/favorites';
import { DialogHost } from './DialogHost';

beforeEach(() => {
  resetStores();
  useFavorites.getState().reset();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useFavorites.setState({ favorites: [fav('x', 'Existing', { folder: 'Reports/Daily' })] });
});

const created = (input: FavoriteInput) => fav('new', input.name, { ...input });

function ask(over: Partial<{ sql: string; connectionId: string | null; database: string | null; fromSelection: boolean }> = {}) {
  act(() =>
    useDialogs.getState().requestDialog({
      kind: 'addFavorite',
      sql: 'SELECT * FROM orders',
      name: 'Query 1',
      connectionId: ids.prod,
      database: 'shop',
      fromSelection: false,
      ...over,
    }),
  );
}

describe('AddFavoriteDialog', () => {
  it('saves name, a new folder and the connection', async () => {
    mockBackend({ favorites_create: ({ input }) => created(input as FavoriteInput) });
    const user = userEvent.setup();
    render(<DialogHost />);
    ask();
    const dialog = screen.getByRole('dialog', { name: 'Add to favorites' });
    expect(within(dialog).getByText('Saves the whole query.')).toBeInTheDocument();
    // Existing folders are offered (ancestors too).
    const options = [...dialog.querySelectorAll('datalist option')].map((o) => o.getAttribute('value'));
    expect(options).toEqual(['Reports', 'Reports/Daily']);

    const name = within(dialog).getByLabelText('Name');
    await user.clear(name);
    await user.type(name, 'Orders');
    await user.type(within(dialog).getByLabelText('Folder'), 'Reports/Weekly');
    expect(within(dialog).getByRole('checkbox', { name: /Keep connection \(shop-prod \/ shop\)/ })).toBeChecked();
    await user.click(within(dialog).getByRole('button', { name: 'Add' }));

    await waitFor(() =>
      expect(callsOf('favorites_create')).toEqual([
        { input: { name: 'Orders', folder: 'Reports/Weekly', sql: 'SELECT * FROM orders', connection_id: ids.prod, database: 'shop' } },
      ]),
    );
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(useDialogs.getState().notice).toBe('Added "Orders" to favorites.');
    expect(useFavorites.getState().favorites.map((f) => f.name)).toContain('Orders');
  });

  it('unchecking keep connection saves an unbound favorite', async () => {
    mockBackend({ favorites_create: ({ input }) => created(input as FavoriteInput) });
    const user = userEvent.setup();
    render(<DialogHost />);
    ask({ fromSelection: true, sql: 'SELECT 1' });
    const dialog = screen.getByRole('dialog', { name: 'Add to favorites' });
    expect(within(dialog).getByText('Saves the selected text.')).toBeInTheDocument();
    await user.click(within(dialog).getByRole('checkbox'));
    await user.click(within(dialog).getByRole('button', { name: 'Add' }));
    await waitFor(() => expect(callsOf('favorites_create')[0]).toMatchObject({ input: { folder: null, connection_id: null, database: null } }));
  });

  it('without a connection the checkbox is off and disabled', () => {
    mockBackend({});
    render(<DialogHost />);
    ask({ connectionId: null, database: null });
    const box = screen.getByRole('checkbox', { name: /the tab has none/ });
    expect(box).not.toBeChecked();
    expect(box).toBeDisabled();
  });

  it('shows the refusal of SQL holding a secret and stays open', async () => {
    const message = 'This SQL contains a password or another secret literal. Favorites never store secrets.';
    mockBackend({
      favorites_create: () => {
        throw new Failure(appError('config', message, { field: 'sql' }));
      },
    });
    const user = userEvent.setup();
    render(<DialogHost />);
    ask({ sql: "CREATE USER u IDENTIFIED BY 'x'" });
    await user.click(screen.getByRole('button', { name: 'Add' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(message);
    expect(screen.getByRole('dialog', { name: 'Add to favorites' })).toBeInTheDocument();
  });

  it('the query toolbar Run menu item opens it for the tab', async () => {
    mockBackend({});
    const user = userEvent.setup();
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    useQueryDocs.getState().update(id, { sql: 'SELECT 7' });
    const doc = useQueryDocs.getState().docs.get(id)!;
    render(
      <>
        <QueryToolbar doc={doc} />
        <DialogHost />
      </>,
    );
    await user.click(screen.getByTestId('query-run-menu'));
    await user.click(screen.getByTestId('query-add-favorite'));
    expect(screen.getByRole('dialog', { name: 'Add to favorites' })).toBeInTheDocument();
    expect(useDialogs.getState().request).toMatchObject({ kind: 'addFavorite', sql: 'SELECT 7', name: 'Query 1' });
  });
});
