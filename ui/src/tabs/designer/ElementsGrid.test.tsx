import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { FkDef, IndexDef } from '../../bindings';
import { useCompletion } from '../../store/completion';
import { resetStores } from '../../test/backend';
import { def, draftOf, opened, setup } from './designerTestKit';

beforeEach(() => {
  resetStores();
  useCompletion.reset();
});

const index = (name: string, over: Partial<IndexDef> = {}): IndexDef => ({
  original: name,
  name,
  unique: false,
  parts: [{ column: 'total', expression: null, prefix: null, descending: false }],
  method: null,
  include: [],
  visible: true,
  comment: null,
  constraint: false,
  locked: false,
  ...over,
});

const fk = (name: string, over: Partial<FkDef> = {}): FkDef => ({
  original: name,
  name,
  columns: ['total'],
  ref_schema: null,
  ref_table: 'customers',
  ref_columns: ['id'],
  on_update: 'NO ACTION',
  on_delete: 'CASCADE',
  match_type: null,
  deferrable: false,
  initially_deferred: false,
  locked: false,
  ...over,
});

const withDef = (over: Partial<ReturnType<typeof def>>) => opened({ def: { ...def(), ...over } });
const elRow = (name: string) => document.querySelector<HTMLElement>(`[data-element="${name}"]`)!;
const goTo = async (user: ReturnType<typeof userEvent.setup>, name: RegExp) => {
  await screen.findByRole('table', { name: 'Columns' });
  await user.click(screen.getByRole('tab', { name }));
};

describe('element editors', () => {
  it('add_index_with_desc_part', async () => {
    const user = userEvent.setup();
    setup();
    await goTo(user, /Indexes/);
    await user.click(screen.getByRole('button', { name: 'Add index' }));
    const row = elRow('idx_1');
    expect(within(row).getByTitle('New')).toBeInTheDocument();
    await user.click(within(row).getByRole('checkbox', { name: 'DESC' }));
    expect(draftOf().indexes).toHaveLength(1);
    expect(draftOf().indexes[0]!.parts).toEqual([{ column: 'id', expression: null, prefix: null, descending: true }]);
    // MySQL: prefix length, no Postgres-only fields.
    expect(within(row).getByRole('spinbutton', { name: 'Prefix length' })).toBeInTheDocument();
    expect(within(row).queryByRole('combobox', { name: 'Method' })).toBeNull();
    expect(within(row).getByRole('checkbox', { name: 'Visible' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: /Indexes/ })).toHaveTextContent('1');
  });

  it('fk_actions_select', async () => {
    const user = userEvent.setup();
    setup(withDef({ foreign_keys: [fk('fk_orders_customer')] }));
    await goTo(user, /Foreign keys/);
    const row = elRow('fk_orders_customer');
    const onDelete = within(row).getByRole('combobox', { name: 'ON DELETE' });
    expect(within(onDelete).getAllByRole('option').map((o) => o.textContent)).toEqual(['NO ACTION', 'RESTRICT', 'CASCADE', 'SET NULL', 'SET DEFAULT']);
    await user.selectOptions(onDelete, 'SET NULL');
    expect(draftOf().foreign_keys[0]!.on_delete).toBe('SET NULL');
    expect(within(row).getByTitle('Modified')).toBeInTheDocument();
    // MySQL cannot change a foreign key in place.
    expect(screen.getByRole('note')).toHaveTextContent('Rename the foreign key to change it');
    await user.clear(within(row).getByRole('textbox', { name: 'Name' }));
    await user.type(within(row).getByRole('textbox', { name: 'Name' }), 'fk_renamed');
    expect(screen.queryByRole('note')).toBeNull();
  });

  it('checks_tab_disabled_on_57', async () => {
    setup(opened({}, 'MySql57'));
    await screen.findByRole('table', { name: 'Columns' });
    const tab = screen.getByRole('tab', { name: /Checks/ });
    expect(tab).toBeDisabled();
    expect(tab).toHaveAttribute('title', 'CHECK constraints need MySQL 8.0.16');
  });

  it('options_engine_change_marks_dirty', async () => {
    const user = userEvent.setup();
    setup();
    await goTo(user, /Options/);
    const engine = screen.getByRole('combobox', { name: 'Engine' });
    expect(engine).toHaveValue('InnoDB');
    expect(screen.queryByText('1 unsaved change')).toBeNull();
    await user.selectOptions(engine, 'MyISAM');
    expect(draftOf().options.engine).toBe('MyISAM');
    expect(screen.getByText('1 unsaved change')).toBeInTheDocument();
    expect(within(engine).queryByRole('option', { name: 'Aria' })).toBeNull();
  });

  it('rename_table_in_options', async () => {
    const user = userEvent.setup();
    setup();
    await goTo(user, /Options/);
    const name = screen.getByRole('textbox', { name: 'Table name' });
    await user.clear(name);
    await user.type(name, 'sales');
    expect(draftOf().name).toBe('sales');
    expect(screen.getByText('1 unsaved change')).toBeInTheDocument();
  });

  it('locked_index_is_read_only_with_reason', async () => {
    const user = userEvent.setup();
    setup(
      opened({
        def: { ...def(), indexes: [index('ft_total', { locked: true })] },
        locked: [{ element: { kind: 'index', name: 'ft_total' }, reason: 'Fulltext index', server_text: 'FULLTEXT KEY `ft_total` (`total`)' }],
      }),
    );
    await goTo(user, /Indexes/);
    const row = elRow('ft_total');
    expect(within(row).getByRole('textbox', { name: 'Name' })).toBeDisabled();
    expect(within(row).getByRole('button', { name: 'Remove index' })).toBeDisabled();
    expect(within(row).getByRole('img', { name: 'Locked' })).toHaveAttribute('title', 'Fulltext index\nFULLTEXT KEY `ft_total` (`total`)');
  });

  it('dropped_foreign_key_can_be_restored', async () => {
    const user = userEvent.setup();
    setup(withDef({ foreign_keys: [fk('fk_a')] }));
    await goTo(user, /Foreign keys/);
    await user.click(screen.getByRole('button', { name: 'Remove foreign key' }));
    expect(draftOf().foreign_keys).toEqual([]);
    const row = elRow('fk_a');
    expect(within(row).getByTitle('To drop')).toBeInTheDocument();
    await user.click(within(row).getByRole('button', { name: 'Undo' }));
    expect(draftOf().foreign_keys).toHaveLength(1);
  });

  it('read_only_disables_the_editors', async () => {
    const user = userEvent.setup();
    setup(opened({ read_only: true, def: { ...def(), indexes: [index('i1')] } }));
    await goTo(user, /Indexes/);
    expect(screen.getByRole('button', { name: 'Add index' })).toBeDisabled();
    await user.click(screen.getByRole('tab', { name: /Comment/ }));
    expect(screen.getByRole('textbox', { name: 'Table comment' })).toBeDisabled();
  });
});

describe('postgres editors', () => {
  it('index_method_include_and_constraint_badge', async () => {
    const user = userEvent.setup();
    setup(opened({ def: { ...def(), indexes: [index('orders_total_key', { unique: true, constraint: true, method: 'btree' })] } }, 'Postgres'));
    await goTo(user, /Indexes/);
    const row = elRow('orders_total_key');
    expect(within(row).getByText('Constraint')).toBeInTheDocument();
    expect(within(row).getByRole('combobox', { name: 'Method' })).toHaveValue('btree');
    expect(within(row).queryByRole('spinbutton', { name: 'Prefix length' })).toBeNull();
    expect(within(row).queryByRole('checkbox', { name: 'Visible' })).toBeNull();
    await user.type(within(row).getByRole('textbox', { name: 'Include' }), 'id, ');
    expect(draftOf().indexes[0]!.include).toEqual(['id']);
  });

  it('fk_deferral_fields_and_options_name_only', async () => {
    const user = userEvent.setup();
    setup(opened({ def: { ...def(), foreign_keys: [fk('fk_a')] } }, 'Postgres'));
    await goTo(user, /Foreign keys/);
    const row = elRow('fk_a');
    expect(within(row).getByRole('checkbox', { name: 'Initially deferred' })).toBeDisabled();
    await user.click(within(row).getByRole('checkbox', { name: 'Deferrable' }));
    expect(draftOf().foreign_keys[0]!.deferrable).toBe(true);
    await user.click(within(row).getByRole('checkbox', { name: 'Initially deferred' }));
    expect(draftOf().foreign_keys[0]!.initially_deferred).toBe(true);
    // PostgreSQL has no in-place change restriction.
    expect(screen.queryByRole('note')).toBeNull();
    await user.click(screen.getByRole('tab', { name: /Options/ }));
    expect(screen.getByRole('textbox', { name: 'Table name' })).toBeInTheDocument();
    expect(screen.queryByRole('combobox', { name: 'Engine' })).toBeNull();
  });
});
