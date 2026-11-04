import { useEffect, useMemo, useState } from 'react';
import { api, toAppError } from '../api';
import type { AiConfig, AiConnectionView, AiSettings, AiTestResult, AiTrigger } from '../bindings';
import { useAi } from '../store/ai';
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
 * the OS keychain, never shown again), editor suggestion options, Test, and the connections AI is
 * on for. Turning AI on for a connection goes through the consent dialog.
 */
export function SettingsAiPanel() {
  const [settings, setSettings] = useState<AiSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [key, setKey] = useState('');
  const [models, setModels] = useState<string[]>([]);
  const [optIns, setOptIns] = useState<AiConnectionView[]>([]);
  const [pick, setPick] = useState('');
  const [consentFor, setConsentFor] = useState<string | null>(null);
  const [test, setTest] = useState<AiTestResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const summaries = useConnections((s) => s.summaries);

  useEffect(() => {
    let live = true;
    Promise.all([api.aiSettingsGet(), api.aiOptInList()])
      .then(([s, list]) => {
        if (!live) return;
        setSettings(s);
        setDraft(draftOf(s.config));
        setOptIns(list);
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
      setError(toAppError(e).message);
    } finally {
      setBusy(false);
    }
  };

  /** Stores the draft; Test and Load models use the stored settings, so they save first. */
  const persist = async () => {
    if (!draft || !settings || JSON.stringify(draftOf(settings.config)) === JSON.stringify(draft)) return;
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
    const list = await api.aiOptInList();
    setOptIns(list);
    useAi.getState().setOptIns(list);
    setTest(null);
  };

  const save = () =>
    run(async () => {
      await persist();
      setNotice('Saved.');
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

  const setOptIn = (id: string, enabled: boolean) =>
    run(async () => {
      const list = await api.aiOptInSet(id, enabled);
      setOptIns(list);
      useAi.getState().setOptIns(list);
      setConsentFor(null);
      setPick('');
    });

  const active = useMemo(() => new Set(optIns.filter((o) => o.active).map((o) => o.connection_id)), [optIns]);
  const candidates = useMemo(
    () =>
      [...summaries.values()]
        .filter((c) => !active.has(c.config.id))
        .sort((a, b) => a.config.name.localeCompare(b.config.name)),
    [summaries, active],
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
  const consentName = consentFor ? (summaries.get(consentFor)?.config.name ?? '') : '';
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
          Connections with AI
        </h3>
        {optIns.length === 0 ? (
          <p className="hint">AI is off for every connection.</p>
        ) : (
          <ul className="settings-ai-list">
            {optIns.map((o) => (
              <li key={o.connection_id}>
                <span>{o.name}</span>
                {!o.active && <span className="hint"> Consent needed again for the new endpoint</span>}
                <button type="button" className="btn" disabled={busy} onClick={() => void setOptIn(o.connection_id, false)}>
                  Turn off
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="settings-path-row">
          <select aria-label="Connection to turn AI on for" value={pick} onChange={(e) => setPick(e.target.value)} disabled={!settings.origin}>
            <option value="">Choose a connection…</option>
            {candidates.map((c) => (
              <option key={c.config.id} value={c.config.id}>
                {c.config.name}
              </option>
            ))}
          </select>
          <button type="button" className="btn" disabled={busy || !pick || !settings.origin} onClick={() => setConsentFor(pick)}>
            Turn on…
          </button>
        </div>
        {!settings.origin && <p className="hint">Save a base URL first.</p>}
      </section>

      {consentFor && settings.host && (
        <AiConsentDialog
          connectionName={consentName}
          host={settings.host}
          onClose={() => setConsentFor(null)}
          onConfirm={() => void setOptIn(consentFor, true)}
        />
      )}
    </>
  );
}
