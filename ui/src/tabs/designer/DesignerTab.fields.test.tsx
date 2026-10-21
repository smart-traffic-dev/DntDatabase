import { act, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { TableDef } from '../../bindings';
import { useQueryDocs } from '../../store/queryTabs';
import { useTableDesigns } from '../../store/tableDesigns';
import { resetStores } from '../../test/backend';
import { clean, column, def, draftOf, inspector, known, nameInput, opened, pgCatalog, retype, rowOf, rows, selectRow, setup, tab } from './designerTestKit';

beforeEach(() => resetStores());

describe('DesignerTab column fields and preview', () => {
  it('auto_increment_and_on_update_on_mysql', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    await selectRow(user, 'id');
    const ai = within(inspector()).getByRole('checkbox', { name: 'Auto increment' });
    expect(within(inspector()).queryByRole('combobox', { name: 'Identity' })).toBeNull();
    await user.click(ai);
    expect(draftOf().columns[0]!.auto_increment).toBe(true);
    expect(within(rowOf('id')).getByText('AUTO_INCREMENT')).toBeInTheDocument();
    // A type that takes no AUTO_INCREMENT clears it.
    await retype(user, 'varchar');
    expect(within(inspector()).queryByRole('checkbox', { name: 'Auto increment' })).toBeNull();
    expect(draftOf().columns[0]!.auto_increment).toBe(false);

    await selectRow(user, 'total');
    expect(within(inspector()).queryByRole('checkbox', { name: /On update/ })).toBeNull();
    await retype(user, 'datetime');
    await user.click(within(inspector()).getByRole('checkbox', { name: 'On update CURRENT_TIMESTAMP' }));
    expect(draftOf().columns[1]!.on_update).toBe('CURRENT_TIMESTAMP');
    await user.type(within(inspector()).getByRole('spinbutton', { name: 'Precision' }), '3');
    expect(draftOf().columns[1]!.on_update).toBe('CURRENT_TIMESTAMP(3)');
    expect(within(inspector()).getByRole('checkbox', { name: 'On update CURRENT_TIMESTAMP(3)' })).toBeChecked();
    await retype(user, 'int');
    expect(draftOf().columns[1]!.on_update).toBeNull();
  });

  it('identity_select_on_postgres', async () => {
    const user = userEvent.setup();
    setup(opened({ catalog: pgCatalog, def: def([column('id', { data_type: known('integer') }), column('note', { data_type: known('text') })]) }, 'Postgres'));
    await screen.findByRole('table', { name: 'Columns' });
    await selectRow(user, 'id');
    expect(within(inspector()).queryByRole('checkbox', { name: 'Auto increment' })).toBeNull();
    await user.selectOptions(within(inspector()).getByRole('combobox', { name: 'Identity' }), 'Always');
    expect(draftOf().columns[0]!.identity).toBe('always');
    expect(within(rowOf('id')).getByText('IDENTITY ALWAYS')).toBeInTheDocument();
    await retype(user, 'text');
    expect(draftOf().columns[0]!.identity).toBeNull();
    expect(within(inspector()).queryByRole('combobox', { name: 'Identity' })).toBeNull();
    await selectRow(user, 'note');
    expect(within(inspector()).queryByRole('combobox', { name: 'Identity' })).toBeNull();
  });

  it('type_change_clears_hidden_fields', async () => {
    const user = userEvent.setup();
    const name = column('name', { data_type: known('varchar', { length: 20 }), collation: 'utf8mb4_bin', charset: 'utf8mb4' });
    setup(opened({ def: def([column('id'), name]) }));
    await screen.findByRole('table', { name: 'Columns' });
    await selectRow(user, 'name');
    expect(within(inspector()).getByRole('textbox', { name: 'Collation' })).toHaveValue('utf8mb4_bin');
    await retype(user, 'int');
    expect(draftOf().columns[1]).toMatchObject({ data_type: known('int'), collation: null, charset: null, using: null });
  });

  it('postgres_using_clears_when_type_returns', async () => {
    const user = userEvent.setup();
    setup(opened({ catalog: pgCatalog, def: def([column('id', { data_type: known('integer') })]) }, 'Postgres'));
    await screen.findByRole('table', { name: 'Columns' });
    await selectRow(user, 'id');
    expect(within(inspector()).queryByRole('textbox', { name: 'USING' })).toBeNull();
    await retype(user, 'bigint');
    await user.type(within(inspector()).getByRole('textbox', { name: 'USING' }), 'id::bigint');
    expect(draftOf().columns[0]!.using).toBe('id::bigint');
    await retype(user, 'integer');
    expect(draftOf().columns[0]!.using).toBeNull();
  });

  it('hidden_field_errors_are_listed', async () => {
    const user = userEvent.setup();
    setup(opened(), () => ({ ...clean, errors: [{ element: { kind: 'column', name: 'total' }, field: 'charset', message: 'Bad character set.' }] }));
    await screen.findByRole('table', { name: 'Columns' });
    await user.type(nameInput('total'), 'x');
    await user.clear(nameInput('totalx'));
    await user.type(nameInput(''), 'total');
    await selectRow(user, 'total');
    const list = await within(inspector()).findByRole('list', { name: 'Other problems' });
    expect(list).toHaveTextContent('Bad character set.');
    expect(within(rowOf('total')).getByRole('combobox', { name: 'Type' })).toHaveAttribute('aria-invalid', 'true');
  });

  it('sql_preview_is_verbatim_and_pg_scripts_say_how_they_run', async () => {
    const user = userEvent.setup();
    const stmts = ['ALTER TABLE "public"."orders" DROP COLUMN "note"', 'COMMENT ON TABLE "public"."orders" IS \'x\''];
    setup(opened({ catalog: pgCatalog, def: def([column('id', { data_type: known('integer') })]) }, 'Postgres'), () => ({ ...clean, statements: stmts }));
    await screen.findByRole('table', { name: 'Columns' });
    await user.type(nameInput('id'), '2');
    const sql = await screen.findByRole('group', { name: 'SQL statements' });
    expect(sql).toHaveTextContent(stmts[0]!);
    expect(sql.textContent).not.toContain(';');
    const preview = screen.getByRole('region', { name: 'SQL preview' });
    await user.click(within(preview).getByRole('button', { name: 'Copy' }));
    const copied = await navigator.clipboard.readText();
    expect(copied).toContain('-- The table designer applies these statements in one transaction');
    expect(copied).toContain(`${stmts[0]};\n${stmts[1]};`);
    expect(copied).not.toContain('BEGIN;');
    await user.click(within(preview).getByRole('button', { name: 'Open in query' }));
    const doc = [...useQueryDocs.getState().docs.values()].at(-1)!;
    expect(doc.sql).toMatch(/^-- The table designer[\s\S]*\nBEGIN;\nALTER TABLE[\s\S]*;\nCOMMIT;$/);
  });

  it('locked_row_is_inert', async () => {
    const user = userEvent.setup();
    setup(
      opened({
        def: def([column('id'), column('g', { locked: true }), column('z')]),
        locked: [{ element: { kind: 'column', name: 'g' }, reason: 'Locked for a test.', server_text: '`g` int' }],
      }),
    );
    await screen.findByRole('table', { name: 'Columns' });
    await selectRow(user, 'g');
    const before = draftOf();
    await user.keyboard('{Delete}');
    await user.keyboard('{Control>}{ArrowDown}{/Control}');
    await user.keyboard('{Control>}{ArrowUp}{/Control}');
    expect(draftOf()).toBe(before);
    expect(within(rowOf('g')).getByLabelText('Drag to reorder')).toHaveAttribute('draggable', 'false');
    expect(within(rowOf('g')).getByRole('button', { name: 'Primary key' })).toBeDisabled();
    for (const field of within(inspector()).getAllByRole('textbox')) expect(field).toBeDisabled();
    for (const box of within(inspector()).getAllByRole('checkbox')) expect(box).toBeDisabled();
    expect(within(inspector()).getByText('Locked for a test.')).toBeInTheDocument();
  });

  it('read_only_connection_disables_editing', async () => {
    setup(opened({ read_only: true }));
    await screen.findByRole('table', { name: 'Columns' });
    expect(screen.getByText('This table is shown read-only: the connection is read-only.')).toBeInTheDocument();
    expect(nameInput('id')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Add column' })).toBeDisabled();
  });

  it('chip_without_a_count_and_default_mode_follows_undo', async () => {
    const user = userEvent.setup();
    setup(opened({ def: def([column('id'), column('total', { default: { kind: 'value', text: '0' } })]) }));
    await screen.findByRole('table', { name: 'Columns' });
    // A draft that differs in nothing `changeCount` counts: dirty, with no number to show.
    act(() => useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, extra: 1 }) as TableDef));
    expect(screen.getByText('Unsaved changes')).toBeInTheDocument();
    act(() => useTableDesigns.getState().discard(tab.id));

    await selectRow(user, 'total');
    await user.click(within(inspector()).getByRole('button', { name: 'Expression' }));
    expect(draftOf().columns[1]!.default).toEqual({ kind: 'expression', text: '0' });
    expect(within(inspector()).getByRole('button', { name: 'Expression' })).toHaveAttribute('aria-pressed', 'true');
    await user.keyboard('{Control>}z{/Control}');
    expect(within(inspector()).getByRole('button', { name: 'Value' })).toHaveAttribute('aria-pressed', 'true');
  });

  it('a new column keeps its row through a reorder', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    await user.click(screen.getByRole('button', { name: 'Add column' }));
    const added = rows()[2]!;
    await selectRow(user, added.getAttribute('data-column')!);
    await user.keyboard('{Control>}{ArrowUp}{/Control}');
    expect(rows()[1]).toBe(added);
    expect(added).toHaveFocus();
  });
});
