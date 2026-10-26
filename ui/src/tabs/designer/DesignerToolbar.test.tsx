import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useDialogs } from '../../store/dialogs';
import { resetStores } from '../../test/backend';
import { catalog, column, def, draftOf, known, opened, rowOf, rows, selectRow, setup } from './designerTestKit';
import { filterTypes, groupOf, signatureOf } from './TypePicker';
import { defaultFromText, looksLikeExpression, withLengthText } from './columnModel';

beforeEach(() => resetStores());

const toolbar = () => screen.getByRole('toolbar', { name: 'Table design' });
const tool = (name: string) => within(toolbar()).getByRole('button', { name });

/** A new table as the backend opens it: a starter `id` column that is not on the server. */
const newTable = () => {
  const id = column('id', { original: null, nullable: false, auto_increment: true, data_type: known('int', { unsigned: true }) });
  return opened({ def: { ...def([id]), original_name: null, name: 'new_table', primary_key: { original: null, name: null, columns: ['id'], locked: false } } });
};

describe('new table', () => {
  it('shows its starter column as new (never "to drop"), names the table in the header, Ctrl+S reviews', async () => {
    const user = userEvent.setup();
    setup(newTable());
    await screen.findByRole('table', { name: 'Columns' });
    expect(rows()).toHaveLength(1);
    expect(screen.queryByText(/To drop/)).toBeNull();
    const name = screen.getByRole('textbox', { name: 'Table name' });
    await waitFor(() => expect(name).toHaveFocus());
    await user.keyboard('customers');
    expect(draftOf().name).toBe('customers');
    await waitFor(() => expect(screen.getByRole('button', { name: /Review & apply/ })).toBeEnabled());
    await user.keyboard('{Control>}s{/Control}');
    expect(useDialogs.getState().request).toMatchObject({ kind: 'tableDesign' });
  });

  it('can be reviewed before any edit', async () => {
    setup(newTable());
    await waitFor(() => expect(screen.getByRole('button', { name: /Review & apply/ })).toBeEnabled());
  });
});

describe('toolbar', () => {
  it('inserts before, deletes, moves and keys the selected column; undo and redo', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    expect(tool('Insert column')).toBeDisabled();
    await selectRow(user, 'total');
    await user.click(tool('Insert column'));
    expect(draftOf().columns.map((c) => c.name)).toEqual(['id', 'column_3', 'total']);
    await user.click(tool('Undo'));
    expect(draftOf().columns.map((c) => c.name)).toEqual(['id', 'total']);
    await user.click(tool('Redo'));
    expect(draftOf().columns).toHaveLength(3);
    await user.click(tool('Undo'));

    await selectRow(user, 'total');
    await user.click(tool('Move up'));
    expect(draftOf().columns.map((c) => c.name)).toEqual(['total', 'id']);
    expect(tool('Move up')).toBeDisabled();

    expect(tool('Primary key')).toHaveAttribute('aria-pressed', 'false');
    await user.click(tool('Primary key'));
    expect(draftOf().primary_key?.columns).toEqual(['id', 'total']);
    expect(tool('Primary key')).toHaveAttribute('aria-pressed', 'true');

    await user.click(tool('Delete column'));
    expect(draftOf().columns.map((c) => c.name)).toEqual(['id']);
    expect(within(rowOf('total')).getByText(/To drop/)).toBeInTheDocument();
  });

  it('explains why PostgreSQL cannot move columns', async () => {
    const user = userEvent.setup();
    setup(opened({}, 'Postgres'));
    await screen.findByRole('table', { name: 'Columns' });
    await selectRow(user, 'total');
    expect(tool('Move up')).toBeDisabled();
    expect(tool('Move up')).toHaveAttribute('title', 'PostgreSQL cannot reorder columns.');
  });
});

describe('type picker and Length / Decimals', () => {
  it('lists the types grouped, filters as one types and picks with Enter', async () => {
    const user = userEvent.setup();
    setup();
    await screen.findByRole('table', { name: 'Columns' });
    const type = within(rowOf('total')).getByRole('combobox', { name: 'Type' });
    await user.click(type);
    const list = screen.getByRole('listbox', { name: 'Type options' });
    expect(within(list).getAllByRole('option').map((o) => o.textContent)).toEqual(['int', 'decimal[(p,s)]', 'varchar(n)', 'datetime[(p)]']);
    expect(list).toHaveTextContent('Numeric');
    await user.clear(type);
    await user.type(type, 'va');
    expect(within(screen.getByRole('listbox', { name: 'Type options' })).getAllByRole('option')).toHaveLength(1);
    await user.keyboard('{Enter}');
    expect(screen.queryByRole('listbox')).toBeNull();
    expect(draftOf().columns[1]!.data_type).toMatchObject({ kind: 'known', base: 'varchar' });
  });

  it('edits precision and scale in separate cells', async () => {
    const user = userEvent.setup();
    setup(opened({ def: def([column('id', { nullable: false }), column('total', { data_type: known('decimal', { precision: 12, scale: 2 }) })]) }));
    await screen.findByRole('table', { name: 'Columns' });
    const length = within(rowOf('total')).getByRole('textbox', { name: 'Length' });
    const decimals = within(rowOf('total')).getByRole('textbox', { name: 'Decimals' });
    expect(length).toHaveValue('12');
    expect(decimals).toHaveValue('2');
    await user.clear(decimals);
    await user.type(decimals, '4');
    expect(draftOf().columns[1]!.data_type).toMatchObject({ precision: 12, scale: 4 });
    // An int takes neither.
    expect(within(rowOf('id')).getByRole('textbox', { name: 'Decimals' })).toBeDisabled();
  });

  it('groups, signatures and filtering', () => {
    expect(groupOf('varchar')).toBe('String');
    expect(groupOf('timestamptz')).toBe('Date & time');
    expect(groupOf('my_domain')).toBe('Other');
    expect(signatureOf(catalog[2]!)).toBe('(n)');
    expect(filterTypes(catalog, 'ar').map((t) => t.base)).toEqual(['varchar']);
    expect(filterTypes(catalog, 'd').map((t) => t.base)).toEqual(['decimal', 'datetime']);
  });
});

describe('defaults typed in the grid', () => {
  it('takes CURRENT_TIMESTAMP and calls as expressions, NULL as no default, the rest as values', async () => {
    const user = userEvent.setup();
    setup(opened({ def: def([column('id', { nullable: false }), column('total', { data_type: known('datetime') })]) }));
    await screen.findByRole('table', { name: 'Columns' });
    const cell = within(rowOf('total')).getByRole('textbox', { name: 'Default' });
    await user.type(cell, 'CURRENT_TIMESTAMP');
    expect(draftOf().columns[1]!.default).toEqual({ kind: 'expression', text: 'CURRENT_TIMESTAMP' });
    await user.clear(cell);
    await user.type(cell, 'now()');
    expect(draftOf().columns[1]!.default).toEqual({ kind: 'expression', text: 'now()' });
    await user.clear(cell);
    await user.type(cell, 'NULL');
    expect(draftOf().columns[1]!.default).toBeNull();
    expect(cell).toHaveValue('NULL');
    await user.clear(cell);
    await user.type(cell, '2024-01-01');
    expect(draftOf().columns[1]!.default).toEqual({ kind: 'value', text: '2024-01-01' });
  });

  it('recognises expression shapes', () => {
    for (const e of ['CURRENT_TIMESTAMP', 'current_timestamp(3)', 'CURRENT_DATE', 'uuid()', 'gen_random_uuid()', "nextval('s')", '(price * 2)']) expect(looksLikeExpression(e), e).toBe(true);
    for (const v of ['abc', '0.00', 'now', "'x'", '2024-01-01', 'Untitled (1)']) expect(looksLikeExpression(v), v).toBe(false);
  });

  it('keeps an expression default an expression, whatever its shape', () => {
    expect(defaultFromText("ARRAY['a']::text[]", 'expression')).toEqual({ kind: 'expression', text: "ARRAY['a']::text[]" });
    expect(defaultFromText("ARRAY['a']::text[]")).toEqual({ kind: 'value', text: "ARRAY['a']::text[]" });
    expect(defaultFromText('now()', 'value')).toEqual({ kind: 'expression', text: 'now()' });
    expect(defaultFromText(' null ', 'expression')).toBeNull();
  });

  it('reads a full precision,scale typed in Length and drops the scale with the precision', () => {
    const info = catalog.find((t) => t.precision !== 'None' && t.scale !== 'None')!;
    const spec = { kind: 'known' as const, base: info.base, length: null, precision: 10, scale: 2, unsigned: false, values: [] };
    expect(withLengthText(spec, info, '14,3')).toMatchObject({ precision: 14, scale: 3 });
    expect(withLengthText(spec, info, '12')).toMatchObject({ precision: 12, scale: 2 });
    expect(withLengthText(spec, info, '')).toMatchObject({ precision: null, scale: null });
  });
});