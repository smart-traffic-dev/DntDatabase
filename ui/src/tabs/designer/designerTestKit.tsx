import { render, screen, within } from '@testing-library/react';
import type userEvent from '@testing-library/user-event';
import type { ColumnDef, DesignOpen, DesignRender, ServerVariant, TableDef, TypeInfo } from '../../bindings';
import { useTableDesigns } from '../../store/tableDesigns';
import { designTabId, useTabs, type DesignTabRef } from '../../store/tabs';
import { ids, mockBackend, serverInfo } from '../../test/backend';
import { DesignerTab } from './DesignerTab';

/* Fixtures and queries shared by the designer tab tests. */
export const scope = { database: 'shop', schema: null };
export const tab: DesignTabRef = { id: designTabId(ids.prod, scope, 'orders'), kind: 'design', connectionId: ids.prod, scope, table: 'orders', title: 'orders' };

export const known = (base: string, extra: Partial<Extract<ColumnDef['data_type'], { kind: 'known' }>> = {}): ColumnDef['data_type'] => ({
  kind: 'known',
  base,
  length: null,
  precision: null,
  scale: null,
  unsigned: false,
  values: [],
  ...extra,
});

export const column = (name: string, over: Partial<ColumnDef> = {}): ColumnDef => ({
  original: name,
  name,
  data_type: known('int'),
  nullable: true,
  default: null,
  auto_increment: false,
  identity: null,
  on_update: null,
  charset: null,
  collation: null,
  comment: null,
  using: null,
  generated: null,
  locked: false,
  ...over,
});

export const def = (columns: ColumnDef[] = [column('id', { nullable: false }), column('total')]): TableDef => ({
  original_name: 'orders',
  name: 'orders',
  columns,
  primary_key: { original: 'PRIMARY', name: 'PRIMARY', columns: ['id'], locked: false },
  indexes: [],
  foreign_keys: [],
  checks: [],
  options: { engine: 'InnoDB', charset: null, collation: null, row_format: null, locked: false },
  comment: null,
  read_only_reason: null,
});

export const info = (base: string, over: Partial<TypeInfo> = {}): TypeInfo => ({
  base,
  length: 'None',
  precision: 'None',
  scale: 'None',
  unsigned: false,
  values: false,
  collation: false,
  ...over,
});
export const catalog: TypeInfo[] = [
  info('int', { unsigned: true }),
  info('decimal', { precision: 'Optional', scale: 'Optional', unsigned: true }),
  info('varchar', { length: 'Required', collation: true }),
  info('datetime', { precision: 'Optional' }),
];
export const pgCatalog: TypeInfo[] = [info('integer'), info('bigint'), info('text', { collation: true }), info('numeric', { precision: 'Optional', scale: 'Optional' })];

export const opened = (over: Partial<DesignOpen> = {}, variant: ServerVariant = 'MySql8'): DesignOpen => ({
  design_id: 'd1',
  def: def(),
  locked: [],
  catalog,
  server: serverInfo(variant),
  env: null,
  read_only: false,
  ...over,
});

export const clean: DesignRender = { statements: ['ALTER TABLE `shop`.`orders` CHANGE COLUMN `total` `amount` int NULL'], impacts: [], errors: [], error: null };

export function setup(open: DesignOpen = opened(), renderResult: () => DesignRender = () => clean) {
  mockBackend({ table_design_open: () => open, table_design_render: renderResult, table_design_close: () => null });
  useTabs.getState().open(tab);
  return render(<DesignerTab tab={tab} />);
}

export const rows = () => screen.getAllByRole('row').filter((r) => r.getAttribute('data-column') !== null);
export const rowOf = (name: string) => {
  const row = rows().find((r) => r.getAttribute('data-column') === name);
  if (!row) throw new Error(`no row ${name}`);
  return row;
};
export const nameInput = (name: string) => within(rowOf(name)).getByRole('textbox', { name: 'Name' });

export const draftOf = () => useTableDesigns.getState().byTab[tab.id]!.draft!;
export const inspector = () => screen.getByRole('complementary', { name: 'Column inspector' });
export async function selectRow(user: ReturnType<typeof userEvent.setup>, name: string) {
  await user.click(within(rowOf(name)).getByRole('cell', { name: 'State' }));
}
export async function retype(user: ReturnType<typeof userEvent.setup>, text: string) {
  const type = within(inspector()).getByRole('combobox', { name: 'Type' });
  await user.clear(type);
  await user.type(type, text);
}
