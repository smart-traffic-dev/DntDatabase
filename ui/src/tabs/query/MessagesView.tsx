import { useVirtualizer } from '@tanstack/react-virtual';
import { useEffect, useLayoutEffect, useMemo, useRef, type KeyboardEvent, type MouseEvent } from 'react';
import { needsReconnect } from '../../api';
import { stmtsByIndex } from '../../query/plan';
import { planAlive, type MessageLine, type TabRun } from '../../store/queryRun';
import { reopenConnection } from '../../store/reopen';
import { revealInEditor } from '../../store/runQuery';
import { useQueryDocs } from '../../store/queryTabs';
import { PLAN_GONE, rerunStatement } from './rerun';

/** Minimum row height (one line); wrapped lines are measured and grow past it. */
const ROW_MIN_HEIGHT = 26;
/** Within this many px of the bottom the panel counts as "following" the run. */
const FOLLOW_SLACK = 80;

/**
 * The Messages panel: one line per statement, plus the final line of the run.
 * Virtualized: only the visible window of lines is mounted, so a 10k-statement
 * script does not re-render 10k rows on every run event.
 */
export function MessagesView({ tabId, run }: { tabId: string; run: TabRun }) {
  const doc = useQueryDocs((s) => s.docs.get(tabId));
  const connectionId = doc?.connectionId ?? null;
  const alive = planAlive(run, doc);
  const ranges = useMemo(() => (run.plan ? stmtsByIndex(run.plan) : null), [run.plan]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  const lastTop = useRef(0);
  const prevCount = useRef(run.messages.length);
  const prevRunId = useRef(run.runId);
  const count = run.messages.length;

  const virtualizer = useVirtualizer({
    count,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_MIN_HEIGHT,
    // Wrapped long errors are taller than one line; never measure below the minimum
    // (also keeps layout-less environments such as jsdom at the estimate).
    measureElement: (el) => Math.max(el.getBoundingClientRect().height, ROW_MIN_HEIGHT),
    overscan: 8,
  });

  // A new run starts at the top of a fresh list: drop cached row heights and follow again.
  useLayoutEffect(() => {
    if (prevRunId.current === run.runId) return;
    prevRunId.current = run.runId;
    following.current = true;
    lastTop.current = 0;
    virtualizer.measure();
  }, [run.runId, virtualizer]);

  // Follow the newest line (including the final Done line) when lines are appended,
  // unless the user scrolled up. A finished run opens at the top, so no scroll on mount.
  useEffect(() => {
    const grew = count > prevCount.current;
    prevCount.current = count;
    if (grew && following.current) virtualizer.scrollToIndex(count - 1, { align: 'end' });
  }, [count, virtualizer]);

  /** Where clicking the line jumps to: the error position, else its statement; none for the final line. */
  const targetOf = (m: MessageLine): { from: number; to: number } | null => {
    if (m.index === null) return null;
    if (m.error?.position != null && m.tone !== 'info') return { from: m.error.position, to: m.error.position + 1 };
    const r = ranges?.get(m.index)?.range;
    return r ? { from: r[0], to: r[1] } : null;
  };
  const jump = (target: { from: number; to: number }) => revealInEditor(tabId, target, { focus: true });

  if (count === 0) {
    const text = run.phase.kind === 'idle' ? 'No messages yet.' : run.mode === 'explain' ? 'Explainingâ€¦' : 'Runningâ€¦';
    return <div className="qr-empty">{text}</div>;
  }
  return (
    <div
      className="qr-messages"
      data-testid="messages"
      role="log"
      aria-live="off"
      aria-label="Messages"
      ref={scrollRef}
      onScroll={(e) => {
        // Only an upward move can stop following (content growth or our own scrolls cannot).
        const el = e.currentTarget;
        const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_SLACK;
        if (el.scrollTop < lastTop.current) following.current = nearBottom;
        else if (nearBottom) following.current = true;
        lastTop.current = el.scrollTop;
      }}
    >
      <div role="list" style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {virtualizer.getVirtualItems().map((item) => {
          const m = run.messages[item.index];
          if (!m) return null;
          const detail = [
            m.rows !== null ? `${m.rows} rows` : null,
            m.affected !== null ? `${m.affected} rows affected` : null,
            m.elapsedMs !== null && m.index !== null ? `${m.elapsedMs} ms` : null,
          ].filter(Boolean);
          const reconnect = m.error !== null && needsReconnect(m.error) && connectionId !== null;
          const target = targetOf(m);
          const twoLines = m.statement != null && m.statement !== '' && m.statement !== m.text;
          return (
            <div
              key={item.key}
              ref={virtualizer.measureElement}
              data-index={item.index}
              className={`qr-msg ${m.tone}${target ? ' jump' : ''}`}
              role="listitem"
              data-testid="message-line"
              tabIndex={target ? 0 : undefined}
              title={target ? 'Show in the editor' : undefined}
              onClick={target ? (e: MouseEvent) => !(e.target as HTMLElement).closest('button') && jump(target) : undefined}
              onKeyDown={
                target
                  ? (e: KeyboardEvent) => {
                      if (e.target === e.currentTarget && (e.key === 'Enter' || e.key === ' ')) {
                        e.preventDefault();
                        jump(target);
                      }
                    }
                  : undefined
              }
              aria-posinset={item.index + 1}
              aria-setsize={count}
              style={{ position: 'absolute', top: 0, left: 0, right: 0, minHeight: ROW_MIN_HEIGHT, transform: `translateY(${item.start}px)` }}
            >
              {m.index !== null && <span className="qr-msg-n">#{m.index + 1}</span>}
              {twoLines ? (
                <span className="qr-msg-text">
                  <span className="qr-msg-stmt" data-testid="message-statement">
                    {m.statement}
                  </span>
                  <span className="qr-msg-detail" data-testid="message-detail">
                    {m.text}
                  </span>
                </span>
              ) : (
                <span className="qr-msg-text">{m.text}</span>
              )}
              {detail.length > 0 && <span className="qr-msg-meta">{detail.join(' Ã‚Â· ')}</span>}
              {m.retryRange !== null && m.index !== null && (
                <button
                  type="button"
                  className="btn small"
                  disabled={!alive}
                  title={alive ? undefined : PLAN_GONE}
                  onClick={() => void rerunStatement(tabId, m.index!)}
                >
                  Retry statement
                </button>
              )}
              {reconnect && (
                <button type="button" className="btn small primary" onClick={() => void reopenConnection(connectionId)}>
                  Reconnect
                </button>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
