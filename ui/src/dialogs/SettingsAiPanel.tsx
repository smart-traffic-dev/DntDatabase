import { useEffect, useMemo, useState } from 'react';
import { api, toAppError } from '../api';
import type { AiConfig, AiSettings, AiTestResult, AiTrigger } from '../bindings';
import { familyOf } from '../engine';
import { aiErrorText, aiOnFor, useAi } from '../store/ai';
import { useConnections } from '../store/connections';
import { AiConsentDialog } from './AiConsentDialog';
import { Callout } from './parts';

const TRIGGERS: { id: AiTrigger; label: string }[] = [
  { id: 'automatic', label: 'Automatic' },
  { id: 'manual', label: 'Manual (Alt+\\)' },
  { id: 'off', label: 'Off' },
];

/** Text drafts of the config fields (numbers stay text until saved). */
interface Draft {
  baseUrl: string;
  model: string;
  trigger: AiTrigger;
  maxTokens: string;
  timeoutMs: string;
  maxLines: string;
}

function draftOf(c: AiConfig): Draft {
  return {
    baseUrl: c.base_url ?? '',
    model: c.model ?? '',
    trigger: c.trigger,
    maxTokens: String(c.max_tokens),
    timeoutMs: String(c.timeout_ms),
    maxLines: String(c.max_lines),
  };
}

function testMessage(r: AiTestResult): { tone: 'success' | 'warning'; text: string } {
  switch (r.verdict) {
    case 'ok':
      return { tone: 'success', text: `The endpoint works (${r.elapsed_ms} ms). Suggested: ${r.sample ?? ''}` };
    case 'ignores_suffix':
      return {
        tone: 'warning',
        text: 'The model may be ignoring the suffix. Use a code model with fill-in-the-middle support.',
      };
    case 'empty':
      return { tone: 'warning', text: 'The endpoint answered but suggested nothing.' };
  }
}

/**
 * Settings → AI (AI editor completion spec §6.1): the OpenAI-compatible endpoint, its key (stored in
 * the OS keychain, never shown again), editor suggestion options, Test, consent (once, to the
 * endpoint's host) and the connections AI is on for.
 */
export function SettingsAiPanel() {
  const [settings, setSettings] = useState<AiSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [key, setKey] = useState('');
  const [models, setModels] = useState<string[]>([]);
  const [asking, setAsking] = useState(false);
  const [test, setTest] = useState<AiTestResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const summaries = useConnections((s) => s.summaries);
  const choices = useAi((s) => s.choices);

  useEffect(() => {
    let live = true;
    Promise.all([api.aiSettingsGet(), api.aiConnections()])
      .then(([s, list]) => {
        if (!live) return;
        setSettings(s);
        setDraft(draftOf(s.config));
        useAi.getState().setChoices(list);
      })
      .catch((e: unknown) => live && setError(toAppError(e).message));
    return () => {
      live = false;
    };
  }, []);

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await action();
    } catch (e) {
      setError(aiErrorText(toAppError(e)));
    } finally {
      setBusy(false);
    }
  };

  /**
   * Stores the draft; Test and Load models use the stored settings, so they save first.
   * Returns the new settings, or `null` when nothing changed.
   */
  const persist = async (): Promise<AiSettings | null> => {
    if (!draft || !settings || JSON.stringify(draftOf(settings.config)) === JSON.stringify(draft)) return null;
    const s = await api.aiSettingsSet({
        base_url: draft.baseUrl.trim() || null,
        model: draft.model.trim() || null,
        trigger: draft.trigger,
        max_tokens: Number(draft.maxTokens),
        timeout_ms: Number(draft.timeoutMs),
        max_lines: Number(draft.maxLines),
      });
    setSettings(s);
    setDraft(draftOf(s.config));
    useAi.getState().setSettings(s);
    setTest(null);
    return s;
  };

  const save = () =>
    run(async () => {
      const before = settings?.origin ?? null;
      const s = await persist();
      setNotice('Saved.');
      // A new endpoint the user has not agreed to: ask now rather than leave AI quietly off.
      if (s?.origin && s.origin !== before && !s.consented && s.config.model) setAsking(true);
    });

  const saveKey = () =>
    run(async () => {
      const s = await api.aiKeySet(key);
      setSettings(s);
      useAi.getState().setSettings(s);
      setKey('');
      setNotice('The API key is saved.');
    });

  const clearKey = () =>
    run(async () => {
      const s = await api.aiKeyClear();
      setSettings(s);
      useAi.getState().setSettings(s);
      setNotice('The API key is removed.');
    });

  const loadModels = () =>
    run(async () => {
      await persist();
      setModels(await api.aiModels());
    });

  const runTest = () =>
    run(async () => {
      setTest(null);
      await persist();
      setTest(await api.aiTest());
    });

  const setConsent = (granted: boolean) =>
    run(async () => {
      setAsking(false);
      const s = await api.aiConsentSet(granted);
      setSettings(s);
      useAi.getState().setSettings(s);
    });

  const setChoice = (id: string, enabled: boolean) =>
    run(async () => {
      useAi.getState().setChoices(await api.aiConnectionSet(id, enabled));
    });

  const sqlConnections = useMemo(
    () =>
      [...summaries.values()]
        .filter((c) => familyOf(c.config.kind) === 'sql')
        .sort((a, b) => a.config.name.localeCompare(b.config.name)),
    [summaries],
  );

  if (!settings || !draft) {
    return error ? (
      <p role="alert" className="field-error">
        {error}
      </p>
    ) : null;
  }

  const dirty =
    JSON.stringify(draftOf(settings.config)) !== JSON.stringify(draft);
  const field = (k: keyof Draft) => (e: { target: { value: string } }) => setDraft({ ...draft, [k]: e.target.value });
  const result = test ? testMessage(test) : null;

  return (
    <>
      <div className="field">
        <label className="field-label" htmlFor="settings-ai-base-url">
          Base URL
        </label>
        <input
          id="settings-ai-base-url"
          type="url"
          spellCheck={false}
          placeholder="https://api.example.com/v1"
          value={draft.baseUrl}
          onChange={field('baseUrl')}
        />
        <p className="hint">An OpenAI-compatible endpoint. Requests go to its /completions and /models.</p>
      </div>
      <div className="field">
        <label className="field-label" htmlFor="settings-ai-key">
          API key
        </label>
        <div className="settings-path-row">
          <input
            id="settings-ai-key"
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder={settings.has_key ? 'A key is saved' : 'No key'}
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
          <button type="button" className="btn" disabled={busy || !key.trim()} onClick={() => void saveKey()}>
            Save key
          </button>
          <button type="button" className="btn" disabled={busy || !settings.has_key} onClick={() => void clearKey()}>
            Clear
          </button>
        </div>
        <p className="hint">Stored in Windows Credential Manager. Leave it empty if the endpoint needs no key.</p>
      </div>
      <div className="field">
        <label className="field-label" htmlFor="settings-ai-model">
          Model
        </label>
        <div className="settings-path-row">
          <input
            id="settings-ai-model"
            type="text"
            spellCheck={false}
            list="settings-ai-models"
            value={draft.model}
            onChange={field('model')}
          />
          <datalist id="settings-ai-models">
            {models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          <button type="button" className="btn" disabled={busy || !draft.baseUrl.trim()} onClick={() => void loadModels()}>
            Load models
          </button>
        </div>
        <p className="hint">
          Requires a code model with fill-in-the-middle support (for example Qwen2.5-Coder, DeepSeek-Coder). Chat models
          are not supported.
        </p>
      </div>
      <div className="field">
        <span className="field-label">Editor suggestions</span>
        <div className="theme-options" role="radiogroup" aria-label="Editor suggestions">
          {TRIGGERS.map((t) => (
            <button
              key={t.id}
              type="button"
              role="radio"
              aria-checked={draft.trigger === t.id}
              className="theme-option"
              onClick={() => setDraft({ ...draft, trigger: t.id })}
            >
              {t.label}
            </button>
          ))}
        </div>
      </div>
      <div className="field settings-ai-numbers">
        <label>
          <span className="field-label">Max tokens</span>
          <input type="number" min={16} max={512} value={draft.maxTokens} onChange={field('maxTokens')} />
        </label>
        <label>
          <span className="field-label">Timeout (ms)</span>
          <input type="number" min={1000} max={30000} step={500} value={draft.timeoutMs} onChange={field('timeoutMs')} />
        </label>
        <label>
          <span className="field-label">Max lines</span>
          <input type="number" min={1} max={20} value={draft.maxLines} onChange={field('maxLines')} />
        </label>
      </div>
      <div className="field settings-actions">
        <button type="button" className="btn primary" disabled={busy || !dirty} onClick={() => void save()}>
          Save
        </button>
        <button
          type="button"
          className="btn"
          disabled={busy || !draft.baseUrl.trim() || !draft.model.trim()}
          title={dirty ? 'Saves the settings, then tests them' : undefined}
          onClick={() => void runTest()}
        >
          Test
        </button>
      </div>
      {result && (
        <Callout tone={result.tone} role="status">
          {result.text}
        </Callout>
      )}
      {notice && (
        <p role="status" className="hint">
          {notice}
        </p>
      )}
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}

      <section className="settings-section field" aria-labelledby="settings-ai-connections">
        <h3 id="settings-ai-connections" className="field-label">
          Sharing
        </h3>
        {!settings.origin ? (
          <p className="hint">Save a base URL first.</p>
        ) : settings.consented ? (
          <div className="settings-ai-consent" data-testid="ai-consent-on">
            <p className="hint">Editor text and schema names of the connections ticked below go to {settings.host}.</p>
            <button type="button" className="btn" disabled={busy} onClick={() => void setConsent(false)}>
              Turn off AI
            </button>
          </div>
        ) : (
          <div className="settings-ai-consent" data-testid="ai-consent-off">
            <p className="hint">AI is off: nothing is sent to {settings.host} until you turn it on.</p>
            <button type="button" className="btn primary" disabled={busy} onClick={() => setAsking(true)}>
              Turn on AI…
            </button>
          </div>
        )}
        {sqlConnections.length > 0 && (
          <ul className="settings-ai-list" aria-label="Connections with AI">
            {sqlConnections.map((c) => (
              <li key={c.config.id}>
                <label className="check">
                  <input
                    type="checkbox"
                    checked={aiOnFor(choices, c.config)}
                    disabled={busy}
                    onChange={(e) => void setChoice(c.config.id, e.target.checked)}
                  />
                  <span>{c.config.name}</span>
                </label>
                {c.config.env_tag === 'production' && <span className="hint">Production</span>}
              </li>
            ))}
          </ul>
        )}
        <p className="hint">Connections tagged Production stay off until you tick them.</p>
      </section>

      {asking && settings.host && (
        <AiConsentDialog host={settings.host} onClose={() => setAsking(false)} onConfirm={() => void setConsent(true)} />
      )}
    </>
  );
}
