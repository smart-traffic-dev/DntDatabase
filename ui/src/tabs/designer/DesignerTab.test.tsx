import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { DesignRender } from '../../bindings';
import { useQueryDocs } from '../../store/queryTabs';
import { useTableDesigns } from '../../store/tableDesigns';
import { useTabs } from '../../store/tabs';
import { appError, callsOf, Failure, mockBackend, resetStores } from '../../test/backend';
import { render } from '@testing-library/react';
import { DesignerTab } from './DesignerTab';
import { clean, column, def, known, nameInput, opened, rowOf, rows, setup, tab } from './designerTestKit';

beforeEach(() => resetStores());

describe('DesignerTab', () => {
  it('renders_columns_and_preview', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    expect(rows().map((r) => r.getAttribute('data-column'))).toEqual(['id', 'total']);
    expect(screen.getByRole('tab', { name: /Columns/ })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tab', { name: /Columns/ })).toHaveTextContent('2');

    await user.clear(nameInput('total'));
    await user.type(nameInput(''), 'amount');
    const preview = screen.getByRole('region', { name: 'SQL preview' });
    await waitFor(() => expect(preview).toHaveTextContent('CHANGE COLUMN `total` `amount`'));
    expect(within(preview).getByText('1')).toHaveClass('dz-ln');
    expect(screen.getByText('1 unsaved change')).toBeInTheDocument();
    expect(within(rowOf('amount')).getByTitle('Modified')).toBeInTheDocument();

    await user.click(within(preview).getByRole('button', { name: 'Open in query' }));
    const docs = [...useQueryDocs.getState().docs.values()];
    expect(docs.some((d) => d.sql.includes('CHANGE COLUMN `total` `amount` int NULL;'))).toBe(true);
  });

  it('type_change_shows_precision_fields', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    await user.click(within(rowOf('total')).getByRole('cell', { name: 'State' }));
    const inspector = screen.getByRole('complementary', { name: 'Column inspector' });
    expect(within(inspector).queryByRole('spinbutton', { name: 'Precision' })).toBeNull();
    expect(within(inspector).queryByRole('textbox', { name: 'Collation' })).toBeNull();
    expect(within(inspector).getByRole('checkbox', { name: 'Unsigned' })).toBeInTheDocument();

    const type = within(inspector).getByRole('combobox', { name: 'Type' });
    await user.clear(type);
    await user.type(type, 'decimal');
    expect(within(inspector).getByRole('spinbutton', { name: 'Precision' })).toBeInTheDocument();
    expect(within(inspector).getByRole('spinbutton', { name: 'Scale' })).toBeInTheDocument();
    expect(within(inspector).queryByRole('spinbutton', { name: 'Length' })).toBeNull();
    expect(within(inspector).getByText('Before: int')).toBeInTheDocument();

    await user.type(within(inspector).getByRole('spinbutton', { name: 'Precision' }), '14');
    const draft = useTableDesigns.getState().byTab[tab.id]!.draft!;
    expect(draft.columns[1]!.data_type).toEqual(known('decimal', { precision: 14 }));

    await user.clear(type);
    await user.type(type, 'varchar');
    expect(within(inspector).getByRole('spinbutton', { name: 'Length' })).toBeInTheDocument();
    expect(within(inspector).getByRole('textbox', { name: 'Collation' })).toBeInTheDocument();
    expect(within(inspector).queryByRole('checkbox', { name: 'Unsigned' })).toBeNull();
  });

  it('locked_column_is_disabled_with_reason', async () => {
    const locked = column('g', { locked: true, generated: 'id * 2' });
    setup(
      opened({
        def: def([column('id'), locked]),
        locked: [{ element: { kind: 'column', name: 'g' }, reason: 'Generated columns are not supported.', server_text: '`g` int GENERATED ALWAYS AS (`id` * 2)' }],
      }),
    );
    await screen.findByRole('table', { name: 'Columns' });
    expect(nameInput('g')).toBeDisabled();
    expect(within(rowOf('g')).getByRole('checkbox', { name: 'Null' })).toBeDisabled();
    const lock = within(rowOf('g')).getByLabelText('Locked');
    expect(lock.getAttribute('title')).toContain('Generated columns are not supported.');
    expect(lock.getAttribute('title')).toContain('GENERATED ALWAYS AS');
    expect(nameInput('id')).toBeEnabled();
  });

  it('drag_handle_hidden_on_postgres', async () => {
    const view = setup(opened({}, 'Postgres'));
    await screen.findByRole('table', { name: 'Columns' });
    expect(screen.queryAllByLabelText('Drag to reorder')).toHaveLength(0);
    view.unmount();
    resetStores();
    setup(opened({}, 'MySql8'));
    await screen.findByRole('table', { name: 'Columns' });
    expect(screen.getAllByLabelText('Drag to reorder')).toHaveLength(2);
  });

  it('review_disabled_while_errors', async () => {
    const user = userEvent.setup();
    let result: DesignRender = { ...clean, errors: [{ element: { kind: 'column', name: 'amount' }, field: 'name', message: 'Bad name.' }] };
    setup(opened(), () => result);
    await screen.findByRole('table', { name: 'Columns' });
    const review = screen.getByRole('button', { name: 'Review & apply…' });
    expect(review).toBeDisabled();
    await user.clear(nameInput('total'));
    await user.type(nameInput(''), 'amount');
    await waitFor(() => expect(useTableDesigns.getState().byTab[tab.id]!.render).not.toBeNull());
    expect(review).toBeDisabled();
    result = clean;
    await user.type(nameInput('amount'), 'x');
    await waitFor(() => expect(review).toBeEnabled());
    result = { ...clean, error: 'Cannot render this change.' };
    await user.type(nameInput('amountx'), 'y');
    await waitFor(() => expect(review).toBeDisabled());
    expect(screen.getByRole('alert')).toHaveTextContent('Cannot render this change.');
  });

  it('draft_error_shown_on_cell', async () => {
    const user = userEvent.setup();
    setup(opened(), () => ({ ...clean, errors: [{ element: { kind: 'column', name: 'id2' }, field: 'name', message: 'Column "id2" is already taken.' }] }));
    await screen.findByRole('table', { name: 'Columns' });
    await user.type(nameInput('total'), '_x');
    await user.clear(nameInput('total_x'));
    await user.type(nameInput(''), 'id2');
    await waitFor(() => expect(nameInput('id2')).toHaveAttribute('aria-invalid', 'true'));
    expect(nameInput('id2')).toHaveAccessibleDescription('Column "id2" is already taken.');
    expect(nameInput('id')).not.toHaveAttribute('aria-invalid', 'true');
  });

  it('undo_shortcut', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    await user.type(nameInput('total'), '_new');
    expect(nameInput('total_new')).toBeInTheDocument();
    // Typing in one field is one undo step.
    await user.keyboard('{Control>}z{/Control}');
    expect(nameInput('total')).toBeInTheDocument();
    await user.keyboard('{Control>}{Shift>}z{/Shift}{/Control}');
    expect(nameInput('total_new')).toBeInTheDocument();

    // Delete on a row marks the column for drop; its Undo link brings it back.
    await user.click(within(rowOf('id')).getByRole('cell', { name: 'State' }));
    await user.keyboard('{Delete}');
    expect(useTableDesigns.getState().byTab[tab.id]!.draft!.columns.map((c) => c.name)).toEqual(['total_new']);
    expect(within(rowOf('id')).getByTitle('To drop')).toBeInTheDocument();
    await user.click(within(rowOf('id')).getByRole('button', { name: 'Undo' }));
    expect(useTableDesigns.getState().byTab[tab.id]!.draft!.columns.map((c) => c.name)).toEqual(['id', 'total_new']);

    // Ctrl+Enter adds a column, Ctrl+Down moves the selected one (MySQL).
    await user.click(within(rowOf('id')).getByRole('cell', { name: 'State' }));
    await user.keyboard('{Control>}{ArrowDown}{/Control}');
    expect(useTableDesigns.getState().byTab[tab.id]!.draft!.columns.map((c) => c.name)).toEqual(['total_new', 'id']);
    await user.keyboard('{Control>}{Enter}{/Control}');
    expect(useTableDesigns.getState().byTab[tab.id]!.draft!.columns).toHaveLength(3);
    expect(rows()[2]).toHaveTextContent('+');
  });

  it('a lost session offers Reopen, then Restore my changes', async () => {
    const user = userEvent.setup();
    let n = 0;
    mockBackend({
      table_design_open: () => opened({ design_id: `d${++n}` }),
      table_design_render: () => clean,
      table_design_close: () => null,
    });
    useTabs.getState().open(tab);
    render(<DesignerTab tab={tab} />);
    await screen.findByRole('table', { name: 'Columns' });
    await user.type(nameInput('total'), '_x');
    act(() => useTableDesigns.getState().markLost(tab.id));
    expect(screen.getByText('The connection to this table was lost.')).toBeInTheDocument();
    expect(nameInput('total_x')).toBeDisabled();
    expect(screen.queryByRole('button', { name: 'Restore my changes' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Discard' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Reopen' }));
    await waitFor(() => expect(callsOf('table_design_open')).toHaveLength(2));
    const restore = await screen.findByRole('button', { name: 'Restore my changes' });
    expect(screen.getByRole('button', { name: 'Discard' })).toBeEnabled();
    await user.click(restore);
    expect(screen.queryByText('The connection to this table was lost.')).toBeNull();
    expect(nameInput('total_x')).toBeEnabled();
  });

  it('a read-only table shows the reason and disables editing', async () => {
    setup(opened({ def: { ...def(), read_only_reason: 'the table is partitioned.' } }));
    await screen.findByRole('table', { name: 'Columns' });
    expect(screen.getByText('This table is shown read-only: the table is partitioned.')).toBeInTheDocument();
    expect(nameInput('id')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Add column' })).toBeDisabled();
  });

  it('a failed open shows the error', async () => {
    mockBackend({ table_design_open: () => { throw new Failure(appError('internal', 'no such table')); }, table_design_close: () => null });
    useTabs.getState().open(tab);
    render(<DesignerTab tab={tab} />);
    expect(await screen.findByRole('alert')).toHaveTextContent('no such table');
  });
});
