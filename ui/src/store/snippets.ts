import { create } from 'zustand';
import { api, toAppError } from '../api';
import { commands, type AppError, type Snippet, type SnippetInput } from '../bindings';
import * as bridge from './snippetsBridge';

/** Case-insensitive prefix order, as the backend lists them. */
const byPrefix = (a: Snippet, b: Snippet) => a.prefix.localeCompare(b.prefix, undefined, { sensitivity: 'base' });

interface SnippetsState {
  snippets: Snippet[];
  loaded: boolean;
  /** Why the last load failed (shown by the manager). */
  error: AppError | null;
  /** Loads every snippet and hands them to the editor's completion. Never throws. */
  load(): Promise<void>;
  /** The create / update / delete calls reject with the backend error (the dialog shows it). */
  create(input: SnippetInput): Promise<Snippet>;
  update(id: string, input: SnippetInput): Promise<Snippet>;
  remove(id: string): Promise<void>;
  reset(): void;
}

/** Pushes the list to the completion bridge. */
function publish(list: Snippet[]) {
  bridge.setUserSnippets(list.map(({ prefix, label, body, description }) => ({ prefix, label, body, description })));
}

export const useSnippets = create<SnippetsState>()((set, get) => {
  const replace = (list: Snippet[]) => {
    const sorted = [...list].sort(byPrefix);
    set({ snippets: sorted, loaded: true, error: null });
    publish(sorted);
  };
  return {
    snippets: [],
    loaded: false,
    error: null,
    load: async () => {
      try {
        // Straight through the bindings: a failed startup load is shown by the manager,
        // not as a global internal-error toast.
        const result = await commands.snippetsList();
        if (result.status === 'error') throw result.error;
        replace(Array.isArray(result.data) ? result.data : []);
      } catch (e) {
        set({ error: toAppError(e) });
      }
    },
    create: async (input) => {
      const created = await api.snippetsCreate(input);
      replace([...get().snippets, created]);
      return created;
    },
    update: async (id, input) => {
      const updated = await api.snippetsUpdate(id, input);
      replace(get().snippets.map((s) => (s.id === id ? updated : s)));
      return updated;
    },
    remove: async (id) => {
      await api.snippetsDelete(id);
      replace(get().snippets.filter((s) => s.id !== id));
    },
    reset: () => set({ snippets: [], loaded: false, error: null }),
  };
});
