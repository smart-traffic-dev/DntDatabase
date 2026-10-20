import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppError } from '../../bindings';
import { emptyRun, reduceRun, startRun, type TabRun } from '../../store/queryRun';
import { reopenConnection } from '../../store/reopen';
import { useQueryDocs } from '../../store/queryTabs';
import { appError, ids, resetStores } from '../../test/backend';
import { MessagesView } from './MessagesView';
import { rerunStatement } from './rerun';

vi.mock('./rerun', () => ({ rerunStatement: vi.fn(async () => {}) }));
vi.mock('../../store/reopen', () => ({ reopenConnection: vi.fn(async () => null) }));

const PLAN = {
  plan_id: 'p1',
  stmts: [{ index: 0, range: [0, 8] as [number, number], kind: 'read' as const, verdict: 'allow' as const, preview: 'SELECT 1' }],
  dialect: { engine: 'postgres' as const },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle' as const,
};

const withError = (error: AppError): TabRun =>
  reduceRun(startRun(emptyRun, PLAN, 0), { t: 'statement_error', index: 0, error }, 1);

describe('MessagesView', () => {
  beforeEach(() => {
    resetStores();
    vi.mocked(rerunStatement).mockClear();
  });

  it('retry_button_only_when_retryable', async () => {
    const { unmount } = render(<MessagesView tabId="t" run={withError(appError('query', 'boom', { retryable: true }))} />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Retry statement' }));
    expect(rerunStatement).toHaveBeenCalledWith('t', 0);
    unmount();
    render(<MessagesView tabId="t" run={withError(appError('query', 'syntax', { retryable: false }))} />);
    expect(screen.queryByRole('button', { name: 'Retry statement' })).toBeNull();
    expect(screen.getByTestId('message-line')).toHaveClass('error');
  });

  it('may_have_been_applied_warning_yellow', () => {
    const run = withError(appError('timeout', 'Timed out.', { outcome_unknown: true, retryable: true }));
    render(<MessagesView tabId="t" run={run} />);
    const row = screen.getByTestId('message-line');
    expect(row).toHaveClass('warning');
    expect(row).not.toHaveClass('error');
    expect(row).toHaveTextContent('may have been applied');
    expect(screen.queryByRole('button', { name: 'Retry statement' })).toBeNull();
  });

  it('cancelled_by_user_is_not_red', () => {
    render(<MessagesView tabId="t" run={withError(appError('cancelled', 'Cancelled.'))} />);
    const row = screen.getByTestId('message-line');
    expect(row).toHaveClass('info');
    expect(row).not.toHaveClass('error');
    expect(row).not.toHaveClass('warning');
  });

  it('connection_lost_offers_reconnect', async () => {
    const run = withError(appError('connection_lost', 'Lost.', { outcome_unknown: true }));
    const id = useQueryDocs.getState().newQuery(ids.prod, null);
    render(<MessagesView tabId={id} run={run} />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Reconnect' }));
    expect(reopenConnection).toHaveBeenCalledWith(ids.prod);
  });

  it('info_line_shows_rows_and_time', () => {
    const run = reduceRun(
      reduceRun(startRun(emptyRun, PLAN, 0), { t: 'result_start', result_id: 1, index: 0, columns: [] }, 1),
      { t: 'result_end', result_id: 1, row_count: 0, affected: 3, truncated: false, elapsed_ms: 12 },
      2,
    );
    render(<MessagesView tabId="t" run={run} />);
    const row = screen.getByTestId('message-line');
    expect(row).toHaveClass('info');
    expect(row).toHaveTextContent('3 rows affected');
    expect(row).toHaveTextContent('12 ms');
  });
});
