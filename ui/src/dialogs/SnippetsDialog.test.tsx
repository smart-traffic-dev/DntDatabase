import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { SnippetInput } from '../bindings';
import { useDialogs } from '../store/dialogs';
import { useSnippets } from '../store/snippets';
import { setUserSnippets } from '../store/snippetsBridge';
import { appError, callsOf, Failure, mockBackend, resetStores } from '../test/backend';
import { snip } from '../test/favorites';
import { DialogHost } from './DialogHost';

vi.mock('../store/snippetsBridge', () => ({ setUserSnippets: vi.fn() }));

beforeEach(() => {
  resetStores();
  useSnippets.getState().reset();
  vi.mocked(setUserSnippets).mockClear();
});

function open() {
  render(<DialogHost />);
  act(() => useDialogs.getState().requestDialog({ kind: 'snippets' }));
  return screen.getByRole('dialog', { name: 'Snippets' });
}

describe('SnippetsDialog', () => {
  it('lists snippets and explains placeholders', async () => {
    mockBackend({ snippets_list: () => [snip('1', 'sel'), snip('2', 'ins')] });
    const dialog = open();
    await waitFor(() => expect(within(dialog).getAllByRole('option')).toHaveLength(2));
    expect(within(dialog).getAllByRole('option').map((o) => o.textContent)).toEqual(['insLabel ins', 'selLabel sel']);
    expect(within(dialog).getByTestId('snippet-help')).toHaveTextContent('${1:placeholder}');
  });

  it('adds a snippet and hands it to the completion', async () => {
    mockBackend({
      snippets_list: () => [],
      snippets_create: ({ input }) => snip('9', (input as SnippetInput).prefix, input as SnippetInput),
    });
    const user = userEvent.setup();
    const dialog = open();
    await waitFor(() => expect(within(dialog).getByText('No snippets yet.')).toBeInTheDocument());
    const add = within(dialog).getByRole('button', { name: 'Add snippet' });
    expect(add).toBeDisabled();
    await user.type(within(dialog).getByLabelText('Prefix'), 'top');
    await user.type(within(dialog).getByLabelText('Label'), 'Top rows');
    await user.type(within(dialog).getByLabelText('Description'), 'First rows of a table');
    // user-event treats `{` as a key descriptor: `{{` types one brace.
    await user.type(within(dialog).getByLabelText('Body'), 'SELECT * FROM ${{1:t} LIMIT ${{2:10};');
    await user.click(add);
    await waitFor(() =>
      expect(callsOf('snippets_create')).toEqual([
        { input: { prefix: 'top', label: 'Top rows', description: 'First rows of a table', body: 'SELECT * FROM ${1:t} LIMIT ${2:10};' } },
      ]),
    );
    expect(within(dialog).getByRole('option', { name: /top/ })).toHaveAttribute('aria-selected', 'true');
    expect(setUserSnippets).toHaveBeenLastCalledWith([
      { prefix: 'top', label: 'Top rows', description: 'First rows of a table', body: 'SELECT * FROM ${1:t} LIMIT ${2:10};' },
    ]);
  });

  it('edits, shows a duplicate prefix error, and deletes after confirming', async () => {
    mockBackend({
      snippets_list: () => [snip('1', 'sel'), snip('2', 'ins')],
      snippets_update: ({ id, input }) => {
        const i = input as SnippetInput;
        if (i.prefix === 'ins') throw new Failure(appError('config', 'A snippet with the prefix "ins" already exists.', { field: 'prefix' }));
        return snip(id as string, i.prefix, i);
      },
      snippets_delete: () => null,
    });
    const user = userEvent.setup();
    const dialog = open();
    await user.click(await within(dialog).findByRole('option', { name: /sel/ }));
    const label = within(dialog).getByLabelText('Label');
    expect(label).toHaveValue('Label sel');
    await user.clear(label);
    await user.type(label, 'Select rows');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(callsOf('snippets_update')).toHaveLength(1));
    expect(callsOf('snippets_update')[0]).toMatchObject({ id: '1', input: { label: 'Select rows' } });
    expect(await within(dialog).findByText('Select rows')).toBeInTheDocument();

    const prefix = within(dialog).getByLabelText('Prefix');
    await user.clear(prefix);
    await user.type(prefix, 'ins');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByRole('alert')).toHaveTextContent('already exists');

    await user.click(within(dialog).getByRole('button', { name: 'Delete' }));
    const confirm = screen.getByRole('alertdialog', { name: 'Delete snippet' });
    await user.click(within(confirm).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(callsOf('snippets_delete')).toEqual([{ id: '1' }]));
    expect(within(dialog).getAllByRole('option').map((o) => o.textContent)).toEqual(['insLabel ins']);
  });
});
