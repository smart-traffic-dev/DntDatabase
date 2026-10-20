import { useVirtualizer } from '@tanstack/react-virtual';
import { useEffect, useRef } from 'react';
import { needsReconnect } from '../../api';
import type { TabRun } from '../../store/queryRun';
import { reopenConnection } from '../../store/reopen';
import { useQueryDocs } from '../../store/queryTabs';
import { rerunStatement } from './rerun';

/** Minimum row height (one line); wrapped lines are measured and grow past it. */
const ROW_MIN_HEIGHT = 26;
/** Within this many px of the bottom the panel counts as "following" the run. */
const FOLLOW_SLACK = 24;

/**
 * The Messages panel: one line per statement, plus the final line of the run.
 * Virtualized: only the visible window of lines is mounted, so a 10k-statement
 * script does not re-render 10k rows on every run event.
 */
export function MessagesView({ tabId, run }: { tabId: string; run: TabRun }) {
  const connectionId = useQueryDocs((s) => s.docs.get(tabId)?.connectionId ?? null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  const count = run.messages.length;
  const running = run.phase.kind === 'running' || run.phase.kind === 'stopping';

  const virtualizer = useVirtualizer({
    count,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_MIN_HEIGHT,
    // Wrapped long errors are taller than one line; never measure below the minimum
    // (also keeps layout-less environments such as jsdom at the estimate).
    measureElement: (el) => Math.max(el.getBoundingClientRect().height, ROW_MIN_HEIGHT),
    overscan: 8,
  });

  // Follow the newest line while a run is in progress, unless the user scrolled up.
  useEffect(() => {
    if (running && following.current && count > 0) virtualizer.scrollToIndex(count - 1, { align: 'end' });
  }, [count, running, virtualizer]);

  if (count === 0) return <div className="qr-empty">No messages yet.</div>;
  return (
    <div
      className="qr-messages"
      data-testid="messages"
      role="log"
      aria-label="Messages"
      ref={scrollRef}
      onScroll={(e) => {
        const el = e.currentTarget;
        following.current = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_SLACK;
      }}
    >
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {virtualizer.getVirtualItems().map((item) => {
          const m = run.messages[item.index];
          if (!m) return null;
          const detail = [
            m.rows !== null ? `${m.rows} rows` : null,
            m.affected !== null ? `${m.affected} rows affected` : null,
            m.elapsedMs !== null && m.index !== null ? `${m.elapsedMs} ms` : null,
          ].filter(Boolean);
          const reconnect = m.error !== null && needsReconnect(m.error) && connectionId !== null;
          return (
            <div
              key={item.key}
              ref={virtualizer.measureElement}
              data-index={item.index}
              className={`qr-msg ${m.tone}`}
              data-testid="message-line"
              aria-posinset={item.index + 1}
              aria-setsize={count}
              style={{ position: 'absolute', top: 0, left: 0, right: 0, minHeight: ROW_MIN_HEIGHT, transform: `translateY(${item.start}px)` }}
            >
              {m.index !== null && <span className="qr-msg-n">#{m.index + 1}</span>}
              <span className="qr-msg-text">{m.text}</span>
              {detail.length > 0 && <span className="qr-msg-meta">{detail.join(' · ')}</span>}
              {m.retryRange !== null && m.index !== null && (
                <button type="button" className="btn small" onClick={() => void rerunStatement(tabId, m.index!)}>
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
