import { create } from 'zustand';
import { commands, type AiConnectionChoice, type AiSettings, type AiTrigger, type ConnectionConfig, type DbKind } from '../bindings';
import { aiConsentNeeded, hasAiSuggestions } from '../engine';
import { useConnections } from './connections';

/**
 * Whether AI is on for connection `cfg` once the user agreed: its own choice, else on unless it
 * is tagged Production. (The backend has the same rule, `ai_on_by_default`.)
 */
export function aiOnFor(choices: ReadonlyMap<string, boolean>, cfg: Pick<ConnectionConfig, 'id' | 'env_tag'>): boolean {
  return choices.get(cfg.id) ?? aiOnByDefault(cfg.env_tag);
}

/** AI is on for a connection without a choice of its own unless it is tagged Production. */
export function aiOnByDefault(envTag: ConnectionConfig['env_tag']): boolean {
  return envTag !== 'production';
}

/**
 * The user's consent covers connections of `kind`: any consent for MySQL and PostgreSQL; for
 * MongoDB, one given to the consent text that names it (an earlier consent does not).
 */
export function aiConsentCovers(kind: DbKind, consent: Pick<AiState, 'consented' | 'consentedMongo'>): boolean {
  switch (aiConsentNeeded(kind)) {
    case 'any':
      return consent.consented;
    case 'mongo':
      return consent.consented && consent.consentedMongo;
    case 'never':
      return false;
  }
}

/**
 * AI suggestions may be asked for in a query tab of connection `connId` (current state): its kind
 * has them (SQL, MongoDB; not Redis), the user's consent covers it, and AI is on for it.
 */
export function aiAllowed(connId: string): boolean {
  const ai = useAi.getState();
  const cfg = useConnections.getState().summaries.get(connId)?.config;
  return !!cfg && hasAiSuggestions(cfg.kind) && aiConsentCovers(cfg.kind, ai) && aiOnFor(ai.choices, cfg);
}

/** Reactive {@link aiAllowed}, for components. */
export function useAiOn(connId: string): boolean {
  const agreed = useAiAgreed(connId);
  const choices = useAi((s) => s.choices);
  const cfg = useConnections((s) => s.summaries.get(connId)?.config);
  return agreed && !!cfg && hasAiSuggestions(cfg.kind) && aiOnFor(choices, cfg);
}

/** Reactive: the user's consent covers connection `connId` ({@link aiConsentCovers}). */
export function useAiAgreed(connId: string): boolean {
  const consented = useAi((s) => s.consented);
  const consentedMongo = useAi((s) => s.consentedMongo);
  const kind = useConnections((s) => s.summaries.get(connId)?.config.kind);
  return !!kind && aiConsentCovers(kind, { consented, consentedMongo });
}

/**
 * The text of a failed AI command: the backend's `ai` and `config` errors carry the endpoint's
 * or the setting's own reason as their message.
 */
export function aiErrorText(e: unknown, fallback = 'The AI request failed.'): string {
  const o = (typeof e === 'object' && e !== null ? e : {}) as { message?: unknown };
  return typeof o.message === 'string' && o.message ? o.message : fallback;
}

/** Consecutive failed suggestion requests before the status bar shows an error. */
export const AI_ERROR_AFTER = 3;

interface AiState {
  /** Settings were read from the backend at least once. */
  loaded: boolean;
  /** A base URL and a model are set. */
  configured: boolean;
  /** Host of the endpoint, for the consent text. */
  host: string | null;
  trigger: AiTrigger;
  /** The user agreed to send editor text and schema names to the current endpoint. */
  consented: boolean;
  /** That consent names MongoDB too (given to the current consent text, not an earlier one). */
  consentedMongo: boolean;
  /** Connections' own on / off choices (the others follow {@link aiOnFor}'s default). */
  choices: ReadonlyMap<string, boolean>;
  /** Why suggestions are paused (spec §7), as the backend said. */
  paused: string | null;
  /** Consecutive failed requests (timeouts, network, 5xx) and the last error. */
  failures: number;
  lastError: string | null;
  /** Requests in flight. */
  busy: number;
  /** Reads settings and consent from the backend (quietly: errors leave AI off). */
  load(): Promise<void>;
  /** Takes what Settings → AI or the consent flow got back from the backend. */
  setSettings(settings: AiSettings): void;
  setChoices(list: AiConnectionChoice[]): void;
  begin(): void;
  /** A request ended: `ok` (suggestion or nothing), `paused`, or `failed`. */
  end(outcome: { kind: 'ok' } | { kind: 'paused'; message: string } | { kind: 'failed'; message: string }): void;
  reset(): void;
}

const initial = {
  loaded: false,
  configured: false,
  host: null,
  trigger: 'automatic' as AiTrigger,
  consented: false,
  consentedMongo: false,
  choices: new Map<string, boolean>() as ReadonlyMap<string, boolean>,
  paused: null,
  failures: 0,
  lastError: null,
  busy: 0,
};

const fromSettings = (s: AiSettings) => ({
  configured: Boolean(s.config.base_url && s.config.model),
  host: s.host,
  trigger: s.config.trigger,
  consented: s.consented,
  consentedMongo: s.consent_covers_mongo,
  // New settings or key: the backend resumes, so does the indicator.
  paused: null,
  failures: 0,
  lastError: null,
});

const choicesOf = (list: AiConnectionChoice[]): ReadonlyMap<string, boolean> =>
  new Map(list.map((c) => [c.connection_id, c.enabled]));

/** AI editor suggestions as the editor and the status bar need them. */
export const useAi = create<AiState>()((set) => ({
  ...initial,
  load: async () => {
    const [settings, choices] = await Promise.all([commands.aiSettingsGet(), commands.aiConnections()]);
    if (settings.status !== 'ok' || choices.status !== 'ok') {
      set({ loaded: true });
      return;
    }
    set({ loaded: true, ...fromSettings(settings.data), choices: choicesOf(choices.data) });
  },
  setSettings: (s) => set({ loaded: true, ...fromSettings(s) }),
  setChoices: (list) => set({ choices: choicesOf(list) }),
  begin: () => set((s) => ({ busy: s.busy + 1 })),
  end: (outcome) =>
    set((s) => {
      const busy = Math.max(0, s.busy - 1);
      switch (outcome.kind) {
        case 'ok':
          return { busy, paused: null, failures: 0, lastError: null };
        case 'paused':
          return { busy, paused: outcome.message };
        case 'failed':
          return { busy, failures: s.failures + 1, lastError: outcome.message };
      }
    }),
  reset: () => set({ ...initial, choices: new Map() }),
}));

let loading: Promise<void> | null = null;

/** Loads the AI state once (later calls share the first load). */
export function ensureAiLoaded(): Promise<void> {
  loading ??= useAi
    .getState()
    .load()
    .catch(() => {});
  return loading;
}

/** Forgets the shared load (tests). */
export function resetAiLoad(): void {
  loading = null;
}
