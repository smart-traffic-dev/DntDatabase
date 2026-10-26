import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, expect, it, vi } from 'vitest';
import type { ActionPreview, NamespaceOptions, ObjectAction } from '../bindings';
import { appError, callsOf, config, Failure, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { nameProblem } from './CreateNamespaceDialog';
import { ObjectActionDialog } from './ObjectActionDialog';

const mysqlOptions: NamespaceOptions = {
  charsets: [{ name: 'latin1', default_collation: 'latin1_swedish_ci' }, { name: 'utf8mb4', default_collation: 'utf8mb4_0900_ai_ci' }],
  collations: [{ name: 'latin1_bin', charset: 'latin1' }, { name: 'utf8mb4_0900_ai_ci', charset: 'utf8mb4' }, { name: 'utf8mb4_bin', charset: 'utf8mb4' }],
  default_charset: 'utf8mb4', default_collation: 'utf8mb4_0900_ai_ci', roles: [], current_role: null,
};
const create: ObjectAction = { kind: 'create_namespace', database: '', name: '' };
const plan = (action: ObjectAction, confirm: string | null = null): ActionPreview =>
  ({ plan_id: 'plan', connection_id: ids.prod, action, sql: 'CREATE DATABASE `x`', confirm_name: confirm, warnings: [], expires_in_seconds: 300 });
/** Renders the SQL the way the backend would, enough to check what the form sends. */
const render_ = ({ action }: Record<string, unknown>) => {
  const a = action as Extract<ObjectAction, { kind: 'create_namespace' }>;
  if (a.owner !== undefined && a.owner !== null) return `CREATE SCHEMA "${a.name}" AUTHORIZATION "${a.owner}"`;
  return `CREATE DATABASE \`${a.name}\`${a.charset ? ` CHARACTER SET ${a.charset}` : ''}${a.collation ? ` COLLATE ${a.collation}` : ''}`;
};

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'localhost')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  vi.spyOn(useTree.getState(), 'refresh').mockResolvedValue();
});

it('validates names like the backend', () => {
  expect(nameProblem('mysql', '', [])).toBe('Enter a name.');
  expect(nameProblem('mysql', 'a.b', [])).toMatch(/cannot contain/);
  expect(nameProblem('mysql', 'a/b', [])).toMatch(/cannot contain/);
  expect(nameProblem('mysql', 'x'.repeat(65), [])).toMatch(/64/);
  expect(nameProblem('mysql', 'ok ', [])).toMatch(/space/);
  expect(nameProblem('mysql', 'x😀', [])).toMatch(/supplementary/);
  expect(nameProblem('postgres', 'ệ'.repeat(22), [])).toMatch(/63 bytes/);
  expect(nameProblem('mysql', 'shop', ['shop'])).toBe('A database named shop already exists.');
  expect(nameProblem('postgres', 'shop-dev', [])).toBeNull();
});

it('shows live SQL with charset and collation, and creates in one step', async () => {
  const user = userEvent.setup();
  mockBackend({ namespace_options: () => mysqlOptions, object_action_render: render_, object_action_preview: ({ action }) => plan(action as ObjectAction), object_action_apply: () => ({ outcome: 'applied', error: null, warnings: [] }) });
  const close = vi.fn();
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: create }} onClose={close} />);
  expect(screen.getByRole('heading', { name: 'Create database' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Create database' })).toBeDisabled();
  await user.type(screen.getByLabelText('Name'), 'shop_dev');
  await waitFor(() => expect(screen.getByLabelText('SQL preview')).toHaveTextContent('CREATE DATABASE `shop_dev`'));
  await waitFor(() => expect(screen.getByLabelText('Character set')).toBeEnabled());
  await user.selectOptions(screen.getByLabelText('Character set'), 'latin1');
  // Collations follow the chosen character set.
  expect([...screen.getByLabelText<HTMLSelectElement>('Collation').options].map((o) => o.value)).toEqual(['', 'latin1_bin']);
  await user.selectOptions(screen.getByLabelText('Collation'), 'latin1_bin');
  await waitFor(() => expect(screen.getByLabelText('SQL preview')).toHaveTextContent('CREATE DATABASE `shop_dev` CHARACTER SET latin1 COLLATE latin1_bin'));
  await user.type(screen.getByLabelText('Name'), '{Enter}');
  await waitFor(() => expect(close).toHaveBeenCalled());
  expect(callsOf('object_action_preview')[0]?.action).toMatchObject({ name: 'shop_dev', charset: 'latin1', collation: 'latin1_bin', owner: null });
  expect(callsOf('object_action_apply')).toHaveLength(1);
  expect(useTree.getState().selectedId).toBe(`${connNodeId(ids.prod)}/db:shop_dev`);
  expect(useDialogs.getState().notice).toBe('Database shop_dev created.');
});

it('flags a name the tree already lists before any request', async () => {
  const user = userEvent.setup();
  mockBackend({ namespace_options: () => mysqlOptions, object_action_render: render_ });
  const conn = connNodeId(ids.prod);
  const db = `${conn}/db:shop`;
  useTree.setState((s) => ({ childIds: new Map(s.childIds).set(conn, [db]), nodes: new Map(s.nodes).set(db, { id: db, parentId: conn, kind: 'database', label: 'shop' }) }));
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: create }} onClose={() => {}} />);
  await user.type(screen.getByLabelText('Name'), 'shop');
  expect(screen.getByRole('alert')).toHaveTextContent('A database named shop already exists.');
  expect(screen.getByRole('button', { name: 'Create database' })).toBeDisabled();
});

it('shows the backend error when the server already has the name', async () => {
  const user = userEvent.setup();
  mockBackend({ namespace_options: () => mysqlOptions, object_action_render: render_, object_action_preview: () => { throw new Failure(appError('config', 'A database named mysql already exists.')); } });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: create }} onClose={() => {}} />);
  await user.type(screen.getByLabelText('Name'), 'mysql{Enter}');
  expect(await screen.findByRole('alert')).toHaveTextContent('A database named mysql already exists.');
  expect(callsOf('object_action_apply')).toHaveLength(0);
});

it('asks production connections for the typed name before creating', async () => {
  const user = userEvent.setup();
  useConnections.getState().setStored([], [config(ids.prod, 'prod', { env_tag: 'production' })]);
  mockBackend({ namespace_options: () => mysqlOptions, object_action_render: render_, object_action_preview: ({ action }) => plan(action as ObjectAction, 'shop'), object_action_apply: () => ({ outcome: 'applied', error: null, warnings: [] }) });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: create }} onClose={() => {}} />);
  await user.type(screen.getByLabelText('Name'), 'shop{Enter}');
  await screen.findByLabelText('Confirmation name');
  expect(callsOf('object_action_apply')).toHaveLength(0);
  expect(screen.getByRole('button', { name: 'Create database' })).toBeDisabled();
  await user.type(screen.getByLabelText('Confirmation name'), 'shop');
  await user.click(screen.getByRole('button', { name: 'Create database' }));
  await waitFor(() => expect(callsOf('object_action_apply')[0]).toMatchObject({ name: 'shop' }));
});

it('offers PostgreSQL schema owners and explains read-only connections', async () => {
  const user = userEvent.setup();
  useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres', read_only: true })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
  mockBackend({ namespace_options: () => ({ ...mysqlOptions, charsets: [], collations: [], roles: ['app', 'postgres'], current_role: 'postgres' }), object_action_render: render_ });
  render(<ObjectActionDialog request={{ connectionId: ids.prod, action: { ...create, database: 'shop' } }} onClose={() => {}} />);
  expect(screen.getByRole('heading', { name: 'Create schema' })).toBeInTheDocument();
  expect(screen.queryByLabelText('Character set')).not.toBeInTheDocument();
  await waitFor(() => expect(screen.getByLabelText('Owner')).toBeEnabled());
  expect([...screen.getByLabelText<HTMLSelectElement>('Owner').options].map((o) => o.text)).toEqual(['Current user (postgres)', 'app']);
  await user.type(screen.getByLabelText('Name'), 'reporting');
  await user.selectOptions(screen.getByLabelText('Owner'), 'app');
  await waitFor(() => expect(screen.getByLabelText('SQL preview')).toHaveTextContent('CREATE SCHEMA "reporting" AUTHORIZATION "app"'));
  expect(screen.getByRole('note')).toHaveTextContent('read-only');
  expect(screen.getByRole('button', { name: 'Create schema' })).toBeDisabled();
});
