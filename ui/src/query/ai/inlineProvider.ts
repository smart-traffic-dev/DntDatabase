// AI ghost text in the query editor (AI editor completion spec §5): a Monaco
// `InlineCompletionsProvider` for both SQL languages. Tab accepts, Ctrl+→ accepts a word and Esc
// dismisses (Monaco's own inline-suggest keys); Alt+\ asks explicitly.
import type * as Monaco from 'monaco-editor';
import { commands, type AiCompleteRequest, type AiCompletion } from '../../bindings';
import { ensureAiLoaded, useAi } from '../../store/ai';
import { queryDocTarget, suggestShown, type CompletionTarget } from '../completion/monacoProvider';

/** Pause in typing before an automatic request. */
export const AI_DEBOUNCE_MS = 350;
/** Suggestions remembered per (connection, database, text, cursor). */
const CACHE_SIZE = 50;

const LANGUAGES = ['mysql', 'pgsql'];

/** What the provider needs from the app; tests pass fakes. */
export interface AiInlineDeps {
  ctxOf(model: unknown): CompletionTarget | null;
  complete(req: AiCompleteRequest): Promise<AiCompletion>;
  cancel(requestId: string): void;
  sleep(ms: number): Promise<void>;
}

const defaultDeps: AiInlineDeps = {
  ctxOf: queryDocTarget,
  complete: async (req) => {
    const r = await commands.aiComplete(req);
    if (r.status === 'error') throw r.error;
    return r.data;
  },
  cancel: (id) => void commands.aiCancel(id),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
};

/** FNV-1a: a cache key for the document text without keeping 50 copies of it. */
function hash(text: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(36) + ':' + text.length.toString(36);
}

const isWordChar = (c: string | undefined) => c !== undefined && /[\p{L}\p{N}_$]/u.test(c);

/** The focused editor showing `model`, if any. */
function editorOf(monaco: typeof Monaco, model: Monaco.editor.ITextModel): Monaco.editor.ICodeEditor | null {
  const editors = monaco.editor?.getEditors?.() ?? [];
  return editors.find((e) => e.getModel() === model && e.hasTextFocus?.()) ?? editors.find((e) => e.getModel() === model) ?? null;
}

/** Why no request is made here, or `null` to ask (spec §5.1). */
export function skipReason(
  monaco: typeof Monaco,
  model: Monaco.editor.ITextModel,
  position: Monaco.Position,
  explicit: boolean,
  target: CompletionTarget | null,
): string | null {
  if (!target?.connId) return 'no connection';
  const ai = useAi.getState();
  if (!ai.loaded) return 'not loaded';
  if (!ai.configured) return 'not set up';
  if (ai.trigger === 'off') return 'off';
  if (ai.trigger === 'manual' && !explicit) return 'manual';
  if (!ai.active.has(target.connId)) return 'not allowed';
  const editor = editorOf(monaco, model);
  if (editor) {
    const selections = editor.getSelections() ?? [];
    if (selections.length > 1) return 'several cursors';
    if (selections.some((s) => !s.isEmpty())) return 'selection';
    if (suggestShown(editor)) return 'suggest list open';
  }
  const line = model.getLineContent(position.lineNumber);
  if (isWordChar(line[position.column - 1])) return 'inside a word';
  return null;
}

export function createAiInlineProvider(monaco: typeof Monaco, deps: AiInlineDeps = defaultDeps): Monaco.languages.InlineCompletionsProvider {
  const cache = new Map<string, string | null>();
  const remember = (key: string, text: string | null) => {
    cache.delete(key);
    cache.set(key, text);
    if (cache.size > CACHE_SIZE) cache.delete(cache.keys().next().value as string);
  };
  const items = (text: string | null, position: Monaco.Position) =>
    text
      ? {
          items: [
            {
              insertText: text,
              range: new monaco.Range(position.lineNumber, position.column, position.lineNumber, position.column),
            },
          ],
        }
      : { items: [] };

  return {
    async provideInlineCompletions(model, position, context, token) {
      const explicit = context.triggerKind === monaco.languages.InlineCompletionTriggerKind?.Explicit;
      void ensureAiLoaded();
      const target = deps.ctxOf(model);
      if (skipReason(monaco, model, position, explicit, target) !== null || !target?.connId) return { items: [] };

      const text = model.getValue();
      const cursor = model.getOffsetAt(position);
      const key = `${target.connId}|${target.db ?? ''}|${cursor}|${hash(text)}`;
      if (cache.has(key)) return items(cache.get(key) ?? null, position);

      if (!explicit) {
        await deps.sleep(AI_DEBOUNCE_MS);
        if (token.isCancellationRequested) return { items: [] };
      }
      const requestId = `ai-${target.tabId}`;
      const sub = token.onCancellationRequested?.(() => deps.cancel(requestId));
      const ai = useAi.getState();
      ai.begin();
      try {
        const out = await deps.complete({
          request_id: requestId,
          conn_id: target.connId,
          database: target.db,
          schema: null,
          text,
          cursor,
        });
        if (out.kind === 'paused') {
          useAi.getState().end({ kind: 'paused', message: out.message });
          return { items: [] };
        }
        useAi.getState().end({ kind: 'ok' });
        const suggestion = out.kind === 'suggestion' ? out.text : null;
        if (!token.isCancellationRequested) remember(key, suggestion);
        return token.isCancellationRequested ? { items: [] } : items(suggestion, position);
      } catch (e) {
        const message = (e as { message?: string } | null)?.message ?? 'The AI request failed.';
        useAi.getState().end({ kind: 'failed', message });
        return { items: [] };
      } finally {
        sub?.dispose();
      }
    },
    freeInlineCompletions() {},
    disposeInlineCompletions() {},
  } as Monaco.languages.InlineCompletionsProvider;
}

type Handle = { dispose(): void };
const handles = new WeakMap<object, Handle>();

/** Registers the provider once per monaco instance (every query editor shares it). */
export function ensureAiInline(monaco: typeof Monaco): void {
  if (handles.has(monaco) || !monaco.languages?.registerInlineCompletionsProvider) return;
  const provider = createAiInlineProvider(monaco);
  const subs = LANGUAGES.map((lang) => monaco.languages.registerInlineCompletionsProvider(lang, provider));
  handles.set(monaco, { dispose: () => subs.forEach((s) => s.dispose()) });
}

/** Alt+\ asks for a suggestion in this editor (an action: scoped to the editor, like the run keys). */
export function bindAiKeys(editor: Monaco.editor.IStandaloneCodeEditor, monaco: typeof Monaco): void {
  const handle = editor.addAction({
    id: 'dnt.ai.suggest',
    label: 'Suggest with AI',
    keybindings: [monaco.KeyMod.Alt | monaco.KeyCode.Backslash],
    run: () => editor.trigger('dnt', 'editor.action.inlineSuggest.trigger', {}),
  });
  editor.onDidDispose(() => handle.dispose());
}
