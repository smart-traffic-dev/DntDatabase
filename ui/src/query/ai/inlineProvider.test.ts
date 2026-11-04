import type * as Monaco from 'monaco-editor';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AiCompleteRequest, AiCompletion } from '../../bindings';
import { useAi } from '../../store/ai';
import { mockBackend, resetStores } from '../../test/backend';
import type { CompletionTarget } from '../completion/monacoProvider';
import { AI_DEBOUNCE_MS, createAiInlineProvider, type AiInlineDeps } from './inlineProvider';

const CONN = 'c1';

class Range {
  constructor(
    public startLineNumber: number,
    public startColumn: number,
    public endLineNumber: number,
    public endColumn: number,
  ) {}
}

/** One-line model with the cursor at `|`. */
function model(marked: string) {
  const at = marked.indexOf('|');
  const text = marked.replace('|', '');
  const m = {
    getValue: () => text,
    getOffsetAt: () => at,
    getLineContent: () => text,
  };
  return { model: m as unknown as Monaco.editor.ITextModel, position: { lineNumber: 1, column: at + 1 } as Monaco.Position };
}

interface FakeEditor {
  selections: { isEmpty(): boolean }[];
}

function fakeMonaco(editors: (FakeEditor & { model: unknown })[] = []) {
  return {
    Range,
    languages: { InlineCompletionTriggerKind: { Automatic: 0, Explicit: 1 } },
    editor: {
      getEditors: () =>
        editors.map((e) => ({
          getModel: () => e.model,
          hasTextFocus: () => true,
          getSelections: () => e.selections,
          getDomNode: () => null,
        })),
    },
  } as unknown as typeof Monaco;
}

function token() {
  const listeners: (() => void)[] = [];
  return {
    isCancellationRequested: false,
    onCancellationRequested(fn: () => void) {
      listeners.push(fn);
      return { dispose() {} };
    },
    cancel() {
      this.isCancellationRequested = true;
      listeners.forEach((l) => l());
    },
  };
}

const target: CompletionTarget = { tabId: 't1', connId: CONN, db: 'shop', engine: 'mysql' };

function deps(answer: AiCompletion | Error = { kind: 'suggestion', text: 'id = 1' }) {
  const calls: AiCompleteRequest[] = [];
  const d: AiInlineDeps & { calls: AiCompleteRequest[]; cancelled: string[]; slept: number[] } = {
    calls,
    cancelled: [],
    slept: [],
    ctxOf: () => target,
    complete: vi.fn(async (req: AiCompleteRequest) => {
      calls.push(req);
      if (answer instanceof Error) throw answer;
      return answer;
    }),
    cancel: (id) => d.cancelled.push(id),
    sleep: async (ms) => {
      d.slept.push(ms);
    },
  };
  return d;
}

const AUTO = { triggerKind: 0 } as Monaco.languages.InlineCompletionContext;
const EXPLICIT = { triggerKind: 1 } as Monaco.languages.InlineCompletionContext;

async function ask(
  provider: Monaco.languages.InlineCompletionsProvider,
  marked: string,
  ctx = AUTO,
  tok = token(),
) {
  const { model: m, position } = model(marked);
  const out = await provider.provideInlineCompletions(m, position as Monaco.Position, ctx, tok as unknown as Monaco.CancellationToken);
  return (out as { items: { insertText: string; range: Range }[] }).items;
}

beforeEach(() => {
  resetStores();
  // Already loaded: the provider must not need the backend for settings.
  mockBackend({ ai_settings_get: () => ({}), ai_opt_in_list: () => [] });
  useAi.setState({ loaded: true, configured: true, trigger: 'automatic', active: new Set([CONN]), host: 'api.example.com' });
});

describe('AI inline provider', () => {
  it('returns the suggestion at the cursor after the typing pause', async () => {
    const d = deps();
    const items = await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT * FROM t WHERE |');
    expect(items).toHaveLength(1);
    expect(items[0]!.insertText).toBe('id = 1');
    expect(items[0]!.range).toEqual(new Range(1, 23, 1, 23));
    expect(d.slept).toEqual([AI_DEBOUNCE_MS]);
    expect(d.calls).toEqual([
      { request_id: 'ai-t1', conn_id: CONN, database: 'shop', schema: null, text: 'SELECT * FROM t WHERE ', cursor: 22 },
    ]);
    expect(useAi.getState().busy).toBe(0);
  });

  it('asks at once on Alt+\\ and caches the answer', async () => {
    const d = deps();
    const p = createAiInlineProvider(fakeMonaco(), d);
    await ask(p, 'SELECT |', EXPLICIT);
    expect(d.slept).toEqual([]);
    const again = await ask(p, 'SELECT |');
    expect(again[0]!.insertText).toBe('id = 1');
    expect(d.calls).toHaveLength(1);
  });

  it('asks nothing when AI is off, manual, not set up or not allowed here', async () => {
    for (const state of [
      { trigger: 'off' as const },
      { trigger: 'manual' as const },
      { configured: false },
      { active: new Set<string>() },
      { loaded: false },
    ]) {
      useAi.setState({ loaded: true, configured: true, trigger: 'automatic', active: new Set([CONN]), ...state });
      const d = deps();
      expect(await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT |')).toEqual([]);
      expect(d.calls, JSON.stringify(state)).toEqual([]);
    }
    // Manual still answers Alt+\.
    useAi.setState({ loaded: true, configured: true, trigger: 'manual', active: new Set([CONN]) });
    const d = deps();
    expect(await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT |', EXPLICIT)).toHaveLength(1);
  });

  it('asks nothing inside a word, with a selection or with several cursors', async () => {
    const d = deps();
    expect(await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT na|me')).toEqual([]);
    const { model: m } = model('SELECT |');
    for (const selections of [[{ isEmpty: () => false }], [{ isEmpty: () => true }, { isEmpty: () => true }]]) {
      const monaco = fakeMonaco([{ model: m, selections }]);
      const p = createAiInlineProvider(monaco, d);
      const out = await p.provideInlineCompletions(m, { lineNumber: 1, column: 8 } as Monaco.Position, AUTO, token() as never);
      expect((out as { items: unknown[] }).items).toEqual([]);
    }
    expect(d.calls).toEqual([]);
  });

  it('drops the request when typing goes on during the pause', async () => {
    const d = deps();
    const tok = token();
    d.sleep = async () => tok.cancel();
    expect(await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT |', AUTO, tok)).toEqual([]);
    expect(d.calls).toEqual([]);
  });

  it('cancels the backend request when Monaco cancels', async () => {
    const d = deps();
    const tok = token();
    d.complete = async () => {
      tok.cancel();
      return { kind: 'suggestion', text: 'late' };
    };
    expect(await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT |', AUTO, tok)).toEqual([]);
    expect(d.cancelled).toEqual(['ai-t1']);
  });

  it('records a pause and counts failures for the status bar', async () => {
    await ask(createAiInlineProvider(fakeMonaco(), deps({ kind: 'paused', message: 'The API key was rejected (401).' })), 'SELECT |');
    expect(useAi.getState().paused).toBe('The API key was rejected (401).');

    const p = createAiInlineProvider(fakeMonaco(), deps(Object.assign(new Error('x'), { message: 'The request timed out.' })));
    for (const text of ['SELECT 1 |', 'SELECT 2 |', 'SELECT 3 |']) expect(await ask(p, text)).toEqual([]);
    expect(useAi.getState().failures).toBe(3);
    expect(useAi.getState().lastError).toBe('The request timed out.');

    await ask(createAiInlineProvider(fakeMonaco(), deps()), 'SELECT 4 |');
    expect(useAi.getState()).toMatchObject({ failures: 0, paused: null, busy: 0 });
  });
});
