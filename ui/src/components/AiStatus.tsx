import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { AiConsentDialog } from '../dialogs/AiConsentDialog';
import { AI_ERROR_AFTER, ensureAiLoaded, useAi } from '../store/ai';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';

type Look = 'off' | 'on' | 'busy' | 'paused' | 'error';

/**
 * The AI indicator of a query tab's status bar (AI editor completion spec §7): off, on, working,
 * paused or failing for the tab's connection. Its menu turns AI on (through the consent dialog)
 * or off for the connection, and opens Settings → AI.
 */
export function AiStatus({ connId }: { connId: string }) {
  const loaded = useAi((s) => s.loaded);
  const configured = useAi((s) => s.configured);
  const host = useAi((s) => s.host);
  const on = useAi((s) => s.active.has(connId));
  const busy = useAi((s) => s.busy > 0);
  const paused = useAi((s) => s.paused);
  const failing = useAi((s) => s.failures >= AI_ERROR_AFTER);
  const lastError = useAi((s) => s.lastError);
  const trigger = useAi((s) => s.trigger);
  const name = useConnections((s) => s.summaries.get(connId)?.config.name ?? '');
  const [menu, setMenu] = useState(false);
  const [consent, setConsent] = useState(false);
  const root = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    void ensureAiLoaded();
  }, []);

  useEffect(() => {
    if (!menu) return;
    const close = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setMenu(false);
    };
    document.addEventListener('mousedown', close);
    return () => document.removeEventListener('mousedown', close);
  }, [menu]);

  // Nothing until AI is set up in Settings → AI: no indicator for people who do not use it.
  if (!loaded || !configured) return null;

  const look: Look = !on ? 'off' : paused ? 'paused' : failing ? 'error' : busy ? 'busy' : 'on';
  const title = {
    off: 'AI is off for this connection',
    paused: `AI is paused: ${paused ?? ''}`,
    error: `AI requests are failing: ${lastError ?? ''}`,
    busy: 'AI is working on a suggestion',
    on:
      trigger === 'off'
        ? 'AI suggestions are off'
        : trigger === 'manual'
          ? 'AI is on: press Alt+\\ for a suggestion'
          : 'AI suggestions are on',
  }[look];

  const openSettings = () => {
    setMenu(false);
    useDialogs.getState().requestDialog({ kind: 'settings', tab: 'ai' });
  };
  const setOptIn = async (enabled: boolean) => {
    setMenu(false);
    setConsent(false);
    try {
      useAi.getState().setOptIns(await api.aiOptInSet(connId, enabled));
    } catch {
      // `api` already reported anything unexpected; the indicator keeps its state.
    }
  };

  return (
    <span className="status-ai-wrap" ref={root}>
      <button
        type="button"
        className={`status-btn status-ai ai-${look}`}
        data-testid="status-ai"
        aria-haspopup="menu"
        aria-expanded={menu}
        title={title}
        aria-label={title}
        onClick={() => setMenu((m) => !m)}
      >
        {look === 'off' ? 'AI off' : 'AI'}
      </button>
      {menu && (
        <div
          className="status-ai-menu"
          role="menu"
          aria-label="AI"
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.stopPropagation();
              setMenu(false);
            }
          }}
        >
          {look === 'off' && (
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                setMenu(false);
                setConsent(true);
              }}
            >
              Turn on AI for this connection…
            </button>
          )}
          {on && (
            <button type="button" role="menuitem" onClick={() => void setOptIn(false)}>
              Turn off AI for this connection
            </button>
          )}
          <button type="button" role="menuitem" onClick={openSettings}>
            AI settings…
          </button>
        </div>
      )}
      {consent && host && (
        <AiConsentDialog connectionName={name} host={host} onClose={() => setConsent(false)} onConfirm={() => void setOptIn(true)} />
      )}
    </span>
  );
}
