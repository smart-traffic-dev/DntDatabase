import { act, fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppError } from '../../bindings';
import type { MessageLine } from '../../store/queryRun';
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

  describe('virtualized', () => {
    const lines = (n: number, over: Partial<MessageLine> = {}): MessageLine[] =>
      Array.from({ length: n }, (_, i) => ({
        index: i,
        text: `statement ${i}`,
        rows: null,
        affected: null,
        elapsedMs: 1,
        error: null,
        tone: 'info' as const,
        retryRange: null,
        ...over,
      }));
    const runWith = (messages: MessageLine[], running = false): TabRun => ({
      ...emptyRun,
      messages,
      phase: running ? { kind: 'running', index: 0, total: messages.length, startedAt: 0 } : emptyRun.phase,
    });

    it('mounts_only_a_window_of_10k_rows', () => {
      render(<MessagesView tabId="t" run={runWith(lines(10_000))} />);
      const mounted = screen.getAllByTestId('message-line').length;
      expect(mounted).toBeGreaterThan(0);
      expect(mounted).toBeLessThan(100);
      expect(screen.getByText('statement 0')).toBeInTheDocument();
      expect(screen.queryByText('statement 9999')).toBeNull();
    });

    it('keeps_list_semantics', () => {
      render(<MessagesView tabId="t" run={runWith(lines(5))} />);
      expect(screen.getByTestId('messages')).toHaveAttribute('role', 'log');
      expect(screen.getAllByTestId('message-line')[0]).toHaveAttribute('aria-posinset', '1');
      expect(screen.getAllByTestId('message-line')[0]).toHaveAttribute('aria-setsize', '5');
    });

    it('retry_and_reconnect_work_on_a_virtualized_row', async () => {
      const err = appError('connection_lost', 'Lost.', { outcome_unknown: true, retryable: true });
      const id = useQueryDocs.getState().newQuery(ids.prod, null);
      const msgs = lines(5000);
      msgs[3] = { ...msgs[3]!, tone: 'error', error: err, retryRange: [0, 5] };
      render(<MessagesView tabId={id} run={runWith(msgs)} />);
      const u = userEvent.setup();
      await u.click(screen.getByRole('button', { name: 'Retry statement' }));
      expect(rerunStatement).toHaveBeenCalledWith(id, 3);
      await u.click(screen.getByRole('button', { name: 'Reconnect' }));
      expect(reopenConnection).toHaveBeenCalledWith(ids.prod);
    });

    it('autoscrolls_to_newest_while_running_unless_scrolled_up', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      const { rerender } = render(<MessagesView tabId="t" run={runWith(lines(500), true)} />);
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(501), true)} />);
      expect(scrollTo).toHaveBeenCalled();

      const box = screen.getByTestId('messages');
      Object.defineProperty(box, 'scrollHeight', { configurable: true, value: 13_000 });
      box.scrollTop = 0;
      fireEvent.scroll(box);
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(502), true)} />);
      expect(scrollTo).not.toHaveBeenCalled();

      // Back at the bottom: following resumes.
      box.scrollTop = 13_000 - 600;
      act(() => {
        fireEvent.scroll(box);
      });
      rerender(<MessagesView tabId="t" run={runWith(lines(503), true)} />);
      expect(scrollTo).toHaveBeenCalled();
      scrollTo.mockRestore();
    });

    it('does_not_autoscroll_when_not_running', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      const { rerender } = render(<MessagesView tabId="t" run={runWith(lines(500))} />);
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(501))} />);
      expect(scrollTo).not.toHaveBeenCalled();
      scrollTo.mockRestore();
    });
  });});
