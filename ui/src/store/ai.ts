import { create } from 'zustand';
import { commands, type AiConnectionView, type AiSettings, type AiTrigger } from '../bindings';

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
  /** Connections with consent for the current endpoint. */
  active: Set<string>;
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
  setOptIns(list: AiConnectionView[]): void;
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
  active: new Set<string>(),
  paused: null,
  failures: 0,
  lastError: null,
  busy: 0,
};

const fromSettings = (s: AiSettings) => ({
  configured: Boolean(s.config.base_url && s.config.model),
  host: s.host,
  trigger: s.config.trigger,
  // New settings or key: the backend resumes, so does the indicator.
  paused: null,
  failures: 0,
  lastError: null,
});

const activeOf = (list: AiConnectionView[]) => new Set(list.filter((o) => o.active).map((o) => o.connection_id));

/** AI editor suggestions as the editor and the status bar need them. */
export const useAi = create<AiState>()((set) => ({
  ...initial,
  load: async () => {
    const [settings, optIns] = await Promise.all([commands.aiSettingsGet(), commands.aiOptInList()]);
    if (settings.status !== 'ok' || optIns.status !== 'ok') {
      set({ loaded: true });
      return;
    }
    set({ loaded: true, ...fromSettings(settings.data), active: activeOf(optIns.data) });
  },
  setSettings: (s) => set({ loaded: true, ...fromSettings(s) }),
  setOptIns: (list) => set({ active: activeOf(list) }),
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
  reset: () => set({ ...initial, active: new Set() }),
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
