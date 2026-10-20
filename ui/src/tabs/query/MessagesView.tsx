import { needsReconnect } from '../../api';
import type { TabRun } from '../../store/queryRun';
import { reopenConnection } from '../../store/reopen';
import { useQueryDocs } from '../../store/queryTabs';
import { rerunStatement } from './rerun';

/** The Messages panel: one line per statement, plus the final line of the run. */
export function MessagesView({ tabId, run }: { tabId: string; run: TabRun }) {
  const connectionId = useQueryDocs((s) => s.docs.get(tabId)?.connectionId ?? null);
  if (run.messages.length === 0) return <div className="qr-empty">No messages yet.</div>;
  return (
    <div className="qr-messages" data-testid="messages">
      {run.messages.map((m, i) => {
        const detail = [
          m.rows !== null ? `${m.rows} rows` : null,
          m.affected !== null ? `${m.affected} rows affected` : null,
          m.elapsedMs !== null && m.index !== null ? `${m.elapsedMs} ms` : null,
        ].filter(Boolean);
        const reconnect = m.error !== null && needsReconnect(m.error) && connectionId !== null;
        return (
          <div key={i} className={`qr-msg ${m.tone}`} data-testid="message-line">
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
  );
}
