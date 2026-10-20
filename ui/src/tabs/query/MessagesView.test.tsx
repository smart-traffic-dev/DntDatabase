import { fireEvent, render, screen } from '@testing-library/react';
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

    it('has_valid_list_semantics', () => {
      render(<MessagesView tabId="t" run={runWith(lines(5))} />);
      const log = screen.getByTestId('messages');
      expect(log).toHaveAttribute('role', 'log');
      expect(log).toHaveAttribute('aria-live', 'off');
      const list = screen.getByRole('list');
      expect(log).toContainElement(list);
      const items = screen.getAllByRole('listitem');
      expect(items[0]).toHaveAttribute('aria-posinset', '1');
      expect(items[0]).toHaveAttribute('aria-setsize', '5');
      expect(items[0]).toBe(screen.getAllByTestId('message-line')[0]);
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

    /** jsdom has no layout: back the scroll metrics with plain numbers. */
    const metrics = (box: HTMLElement, scrollHeight = 13_000) => {
      let top = 0;
      Object.defineProperty(box, 'scrollHeight', { configurable: true, value: scrollHeight });
      Object.defineProperty(box, 'clientHeight', { configurable: true, value: 600 });
      Object.defineProperty(box, 'scrollTop', { configurable: true, get: () => top, set: (v: number) => (top = v) });
      return (v: number) => {
        top = v;
        fireEvent.scroll(box);
      };
    };

    it('autoscrolls_to_newest_unless_scrolled_up_and_resumes_at_bottom', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      const { rerender } = render(<MessagesView tabId="t" run={runWith(lines(500), true)} />);
      const box = screen.getByTestId('messages');
      const scrollTopTo = metrics(box);
      scrollTopTo(12_400); // at the bottom
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(501), true)} />);
      expect(scrollTo).toHaveBeenCalledTimes(1);
      expect((scrollTo.mock.calls[0]![0] as ScrollToOptions).top).toBeGreaterThan(12_300);

      scrollTopTo(5000); // user scrolls up
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(502), true)} />);
      expect(scrollTo).not.toHaveBeenCalled();

      scrollTopTo(12_900); // back to the bottom
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(503), true)} />);
      expect(scrollTo).toHaveBeenCalledTimes(1);
      expect((scrollTo.mock.calls[0]![0] as ScrollToOptions).top).toBeGreaterThanOrEqual(12_400);
      scrollTo.mockRestore();
    });

    it('content_growth_without_upward_scroll_keeps_following', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      const { rerender } = render(<MessagesView tabId="t" run={runWith(lines(500), true)} />);
      const box = screen.getByTestId('messages');
      const scrollTopTo = metrics(box, 13_000);
      scrollTopTo(12_400);
      // Content grows (scrollHeight jumps) and a scroll event fires without moving up.
      Object.defineProperty(box, 'scrollHeight', { configurable: true, value: 30_000 });
      fireEvent.scroll(box);
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(501), true)} />);
      expect(scrollTo).toHaveBeenCalled();
      scrollTo.mockRestore();
    });

    it('follows_the_final_done_line_after_the_run_ends', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      const { rerender } = render(<MessagesView tabId="t" run={runWith(lines(500), true)} />);
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={runWith(lines(501))} />);
      expect(scrollTo).toHaveBeenCalled();
      scrollTo.mockRestore();
    });

    it('does_not_scroll_on_first_mount_of_a_finished_run', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      render(<MessagesView tabId="t" run={runWith(lines(500))} />);
      // The virtualizer itself restores offset 0 on mount; nothing may jump to the end.
      expect(scrollTo.mock.calls.every(([o]) => ((o as ScrollToOptions).top ?? 0) === 0)).toBe(true);
      scrollTo.mockRestore();
    });

    it('new_run_resumes_following', () => {
      const scrollTo = vi.spyOn(Element.prototype, 'scrollTo');
      const { rerender } = render(<MessagesView tabId="t" run={{ ...runWith(lines(500), true), runId: 'r1' }} />);
      const scrollTopTo = metrics(screen.getByTestId('messages'));
      scrollTopTo(5000);
      scrollTo.mockClear();
      rerender(<MessagesView tabId="t" run={{ ...runWith(lines(3), true), runId: 'r2' }} />);
      rerender(<MessagesView tabId="t" run={{ ...runWith(lines(4), true), runId: 'r2' }} />);
      expect(scrollTo).toHaveBeenCalled();
      scrollTo.mockRestore();
    });
  });
});
