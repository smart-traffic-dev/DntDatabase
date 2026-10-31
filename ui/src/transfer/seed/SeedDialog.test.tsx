import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { SeedRequest } from '../../bindings';
import { rerun } from '../../jobs/rerun';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useToasts } from '../../store/toasts';
import { callsOf, config, fail, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { seedPreview, seedRequest, seedSample, seedTable, seedTarget } from '../../test/fixtures/seed';
import { SeedDialog } from './SeedDialog';
import './rerunSeed';

vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));

function backend(over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    seed_target: () => seedTarget(),
    seed_preview: (args) => seedSample(args.request as SeedRequest),
    seed_prepare: () => seedPreview(),
    seed_start: () => 'job-7',
    job_list: () => [],
    ...over,
  });
}

const user = () => userEvent.setup();

function open(params?: SeedRequest) {
  const onClose = vi.fn();
  render(<SeedDialog connectionId={ids.prod} table={seedTable} params={params} onClose={onClose} />);
  return onClose;
}

const row = (name: string) => screen.getByTestId(`seed-col-${name}`);
const review = () => screen.getByRole('button', { name: 'Review…' });
const lastPreview = () => callsOf('seed_preview').at(-1)?.request as SeedRequest | undefined;

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});

describe('SeedDialog', () => {
  it('shows the table, the inferred generators and the preview', async () => {
    backend();
    open();
    expect(screen.getByRole('heading', { name: 'Generate data — shop.orders' })).toBeInTheDocument();
    await screen.findByTestId('seed-col-email');
    expect(screen.getByLabelText('Generator for email')).toHaveValue('fake');
    expect(screen.getByLabelText('Fake data for email')).toHaveValue('email');
    expect(screen.getByLabelText('Generator for customer_id')).toHaveValue('foreign_key');
    // The key count comes with the preview.
    await waitFor(() => expect(row('customer_id')).toHaveTextContent('customers.id · 3 keys'));
    const locked = screen.getByLabelText('Generator for total_with_tax');
    expect(locked).toBeDisabled();
    expect(locked).toHaveAttribute('title', 'Generated column: the server computes it.');
    const grid = await screen.findByTestId('seed-preview');
    expect(within(grid).getAllByRole('row')).toHaveLength(11);
    expect(grid).toHaveTextContent('user0@example.com');
    expect(screen.getByLabelText('Seed')).toHaveValue(48213);
    expect(callsOf('seed_target')).toEqual([{ connectionId: ids.prod, table: seedTable }]);
    await waitFor(() => expect(review()).toBeEnabled());
  });

  it('a changed generator shows its options and previews again', async () => {
    backend();
    const u = user();
    open();
    await screen.findByTestId('seed-col-email');
    await u.selectOptions(screen.getByLabelText('Generator for email'), 'pattern');
    const pattern = screen.getByLabelText('Pattern for email');
    expect(pattern).toHaveValue('???-#####');
    await u.clear(pattern);
    await u.type(pattern, 'U-####');
    await waitFor(() =>
      expect(lastPreview()?.columns.find((c) => c.name === 'email')?.generator).toEqual({ kind: 'pattern', pattern: 'U-####' }),
    );
  });

  it('a column issue shows under its row and blocks the review', async () => {
    backend({
      seed_preview: () => ({ ...seedSample(), rows: [], sql: null, issues: [{ column: 'total', message: 'Values must be below 1000.' }] }),
    });
    open();
    await waitFor(() => expect(row('total').nextElementSibling).toHaveTextContent('Values must be below 1000.'));
    expect(review()).toBeDisabled();
  });

  it('NULL % only on nullable columns; Unique only where values can repeat', async () => {
    backend();
    const u = user();
    open();
    await screen.findByTestId('seed-col-email');
    expect(screen.getByLabelText('NULL % for total')).toBeInTheDocument();
    expect(screen.queryByLabelText('NULL % for email')).toBeNull();
    expect(screen.getByLabelText('Unique email')).toBeChecked();
    await u.selectOptions(screen.getByLabelText('Generator for customer_id'), 'bool');
    expect(screen.queryByLabelText('Unique customer_id')).toBeNull();
    expect(screen.getByLabelText('True % for customer_id')).toHaveValue(50);
  });

  it('New seed changes the seed; Refresh previews again with the same seed', async () => {
    backend();
    const u = user();
    open();
    await screen.findByTestId('seed-preview');
    await waitFor(() => expect(callsOf('seed_preview').length).toBeGreaterThan(0));
    await u.click(screen.getByRole('button', { name: 'New seed' }));
    await waitFor(() => expect(screen.getByLabelText('Seed')).not.toHaveValue(48213));
    const seed = Number((screen.getByLabelText('Seed') as HTMLInputElement).value);
    await waitFor(() => expect(lastPreview()?.seed).toBe(seed));
    const before = callsOf('seed_preview').length;
    await u.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(callsOf('seed_preview').length).toBe(before + 1));
    expect(lastPreview()?.seed).toBe(seed);
  });

  it('Production: Generate stays disabled until the table name is typed, then starts the job', async () => {
    backend({ seed_prepare: () => seedPreview({ confirm: { kind: 'type_name', name: 'orders' } }) });
    const u = user();
    const onClose = open();
    await waitFor(() => expect(review()).toBeEnabled());
    await u.click(review());
    const step = await screen.findByTestId('seed-review');
    expect(step).toHaveTextContent('Insert 1,000 rows into shop.orders in 1 transaction (1 batch).');
    const generate = screen.getByRole('button', { name: 'Generate' });
    expect(generate).toBeDisabled();
    await u.type(screen.getByLabelText(/to confirm/), 'order');
    expect(generate).toBeDisabled();
    await u.type(screen.getByLabelText(/to confirm/), 's');
    expect(generate).toBeEnabled();
    await u.click(generate);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('seed_start')).toEqual([
      { planId: 'seed-plan-1', confirm: { confirmed: true, name: 'orders', irreversible_ack: false } },
    ]);
    expect(callsOf('seed_prepare')[0]?.request).toEqual(seedRequest(ids.prod));
    expect(useToasts.getState().toasts.at(-1)?.title).toBe('Generating data… — see Jobs');
  });

  it('Staging asks "I understand"; an irreversible pre-step asks its own acknowledgement', async () => {
    backend({
      seed_prepare: () =>
        seedPreview({
          confirm: { kind: 'click' },
          pre_step: { sql: 'TRUNCATE `shop`.`orders`', irreversible: true, warning: 'Existing rows are deleted even if the job fails later.' },
        }),
    });
    const u = user();
    open();
    await waitFor(() => expect(review()).toBeEnabled());
    await u.click(review());
    await screen.findByTestId('seed-review');
    expect(screen.getByText('Cannot be undone')).toBeInTheDocument();
    const generate = screen.getByRole('button', { name: 'Generate' });
    await u.click(screen.getByLabelText('I understand'));
    expect(generate).toBeDisabled();
    await u.click(screen.getByLabelText('I understand existing rows are deleted even if the job fails.'));
    expect(generate).toBeEnabled();
  });

  it('Delete first is sent with the request', async () => {
    backend();
    const u = user();
    open();
    await waitFor(() => expect(review()).toBeEnabled());
    await u.selectOptions(screen.getByLabelText('Existing rows'), 'delete');
    await waitFor(() => expect(review()).toBeEnabled());
    await u.click(review());
    await waitFor(() => expect(callsOf('seed_prepare')[0]?.request).toMatchObject({ existing: 'delete' }));
  });

  it('an expired review goes back to Configure', async () => {
    backend({
      seed_start: () => {
        throw fail('plan_expired');
      },
    });
    const u = user();
    const onClose = open();
    await waitFor(() => expect(review()).toBeEnabled());
    await u.click(review());
    await screen.findByTestId('seed-review');
    await u.click(screen.getByRole('button', { name: 'Generate' }));
    expect(await screen.findByText('The review expired; review again.')).toBeInTheDocument();
    expect(screen.queryByTestId('seed-review')).toBeNull();
    expect(onClose).not.toHaveBeenCalled();
  });

  it('read-only connections cannot generate', async () => {
    backend({ seed_target: () => seedTarget({ read_only: true }) });
    open();
    expect(await screen.findByText('This connection is read-only: generating data is blocked.')).toBeInTheDocument();
    await screen.findByTestId('seed-preview');
    expect(review()).toBeDisabled();
  });
});

describe('Run again', () => {
  it('reopens the dialog with the stored request', async () => {
    const request = seedRequest(ids.prod, { rows: 25, seed: 9 });
    expect(rerun({ kind: 'seed', request: request as never })).toBe(true);
    expect(useDialogs.getState().request).toMatchObject({ kind: 'seedData', connectionId: ids.prod, table: seedTable, params: request });
    backend();
    open(request);
    await screen.findByTestId('seed-col-email');
    expect(screen.getByLabelText('Rows')).toHaveValue(25);
    expect(screen.getByLabelText('Seed')).toHaveValue(9);
  });
});
