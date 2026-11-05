import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { AiConsentDialog } from '../dialogs/AiConsentDialog';
import { AI_ERROR_AFTER, aiAllowed, ensureAiLoaded, useAi, useAiOn } from '../store/ai';
import { useDialogs } from '../store/dialogs';

type Look = 'off' | 'on' | 'busy' | 'paused' | 'error';

/**
 * The AI indicator of a query tab's status bar (AI editor completion spec §7): off, on, working,
 * paused or failing for the tab's connection. Its menu turns AI on or off for the connection
 * (asking for consent first when the user has not agreed to the endpoint yet) and opens
 * Settings → AI.
 */
export function AiStatus({ connId }: { connId: string }) {
  const loaded = useAi((s) => s.loaded);
  const configured = useAi((s) => s.configured);
  const host = useAi((s) => s.host);
  const consented = useAi((s) => s.consented);
  const on = useAiOn(connId);
  const busy = useAi((s) => s.busy > 0);
  const paused = useAi((s) => s.paused);
  const failing = useAi((s) => s.failures >= AI_ERROR_AFTER);
  const lastError = useAi((s) => s.lastError);
  const trigger = useAi((s) => s.trigger);
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
    off: consented ? 'AI is off for this connection' : 'AI is off: nothing is sent until you turn it on',
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
  /** Turns AI on or off for this connection; turning it on agrees to the endpoint first if needed. */
  const setOn = async (enabled: boolean, agree = false) => {
    setMenu(false);
    setConsent(false);
    try {
      if (agree) useAi.getState().setSettings(await api.aiConsentSet(true));
      // Its own choice only when the default does not already give what was asked.
      if (aiAllowed(connId) !== enabled) useAi.getState().setChoices(await api.aiConnectionSet(connId, enabled));
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
          {!on && !consented && (
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                setMenu(false);
                setConsent(true);
              }}
            >
              Turn on AI…
            </button>
          )}
          {!on && consented && (
            <button type="button" role="menuitem" onClick={() => void setOn(true)}>
              Turn on AI for this connection
            </button>
          )}
          {on && (
            <button type="button" role="menuitem" onClick={() => void setOn(false)}>
              Turn off AI for this connection
            </button>
          )}
          <button type="button" role="menuitem" onClick={openSettings}>
            AI settings…
          </button>
        </div>
      )}
      {consent && host && (
        <AiConsentDialog host={host} onClose={() => setConsent(false)} onConfirm={() => void setOn(true, true)} />
      )}
    </span>
  );
}
