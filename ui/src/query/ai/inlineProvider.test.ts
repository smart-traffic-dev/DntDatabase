import type * as Monaco from 'monaco-editor';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AiCompleteRequest, AiCompletion } from '../../bindings';
import { aiAllowed, useAi } from '../../store/ai';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { config, mockBackend, resetStores } from '../../test/backend';
import type { CompletionTarget } from '../completion/monacoProvider';
import { AI_DEBOUNCE_MS, aiDocTarget, createAiInlineProvider, ensureAiInline, type AiInlineDeps } from './inlineProvider';

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
  mockBackend({ ai_settings_get: () => ({}), ai_connections: () => [] });
  useConnections.getState().setStored([], [config(CONN, 'Shop')]);
  useAi.setState({ loaded: true, configured: true, trigger: 'automatic', consented: true, choices: new Map(), host: 'api.example.com' });
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
      { consented: false },
      { choices: new Map([[CONN, false]]) },
      { loaded: false },
    ]) {
      useAi.setState({ loaded: true, configured: true, trigger: 'automatic', consented: true, choices: new Map(), ...state });
      const d = deps();
      expect(await ask(createAiInlineProvider(fakeMonaco(), d), 'SELECT |')).toEqual([]);
      expect(d.calls, JSON.stringify(state)).toEqual([]);
    }
    // Manual still answers Alt+\.
    useAi.setState({ loaded: true, configured: true, trigger: 'manual', consented: true, choices: new Map() });
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

  it('typing what was suggested returns the rest without a new request', async () => {
    const d = deps({ kind: 'suggestion', text: 'status = 1' });
    const p = createAiInlineProvider(fakeMonaco(), d);
    await ask(p, 'SELECT * FROM t WHERE |');
    const rest = await ask(p, 'SELECT * FROM t WHERE sta|');
    expect(rest[0]!.insertText).toBe('tus = 1');
    expect(d.calls).toHaveLength(1);
    // Typing something else asks again.
    await ask(p, 'SELECT * FROM t WHERE x|');
    expect(d.calls).toHaveLength(2);
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

describe('AI inline provider in Command tabs', () => {
  const MONGO = '33333333-3333-4333-8333-333333333333';
  const REDIS = '44444444-4444-4444-8444-444444444444';

  beforeEach(() => {
    mockBackend({ ai_settings_get: () => ({}), ai_connections: () => [], tabs_save: () => null });
    useAi.setState({ consentedMongo: true });
    useConnections
      .getState()
      .setStored([], [config(CONN, 'Shop'), config(MONGO, 'docs', { kind: 'mongo', default_db: 'shop' }), config(REDIS, 'cache', { kind: 'redis' })]);
  });

  /** A one-line model of query tab `tabId` with the cursor at `|`. */
  function tabModel(tabId: string, marked: string) {
    const { model: m, position } = model(marked);
    return { model: Object.assign(m, { uri: { scheme: 'query', authority: tabId } }), position };
  }

  it('resolves the tab of a MongoDB or SQL model, not of a Redis one', () => {
    const mongoDoc = useQueryDocs.getState().newQuery(MONGO, 'logs');
    const defaultDoc = useQueryDocs.getState().newQuery(MONGO, null);
    const sqlDoc = useQueryDocs.getState().newQuery(CONN, 'shop');
    const redisDoc = useQueryDocs.getState().newQuery(REDIS, null);
    const of = (id: string) => aiDocTarget({ uri: { scheme: 'query', authority: id } });
    expect(of(mongoDoc)).toEqual({ tabId: mongoDoc, connId: MONGO, db: 'logs' });
    expect(of(defaultDoc)).toEqual({ tabId: defaultDoc, connId: MONGO, db: 'shop' });
    expect(of(sqlDoc)).toMatchObject({ tabId: sqlDoc, connId: CONN, db: 'shop' });
    expect(of(redisDoc)).toBeNull();
  });

  it('asks for a MongoDB tab with the same request shape', async () => {
    const doc = useQueryDocs.getState().newQuery(MONGO, 'logs');
    const d = { ...deps({ kind: 'suggestion', text: 'status: "paid" })' }), ctxOf: aiDocTarget };
    const p = createAiInlineProvider(fakeMonaco(), d);
    const { model: m, position } = tabModel(doc, 'db.orders.find({ |');
    const out = await p.provideInlineCompletions(m, position, AUTO, token() as never);
    expect((out as { items: { insertText: string }[] }).items[0]!.insertText).toBe('status: "paid" })');
    expect(d.calls).toEqual([
      { request_id: `ai-${doc}`, conn_id: MONGO, database: 'logs', schema: null, text: 'db.orders.find({ ', cursor: 17 },
    ]);
  });

  it('asks nothing for a Redis tab, even with AI turned on for it', async () => {
    useAi.setState({ choices: new Map([[REDIS, true]]) });
    expect(aiAllowed(REDIS)).toBe(false);
    expect(aiAllowed(MONGO)).toBe(true);
    const doc = useQueryDocs.getState().newQuery(REDIS, null);
    const d = { ...deps(), ctxOf: aiDocTarget };
    const { model: m, position } = tabModel(doc, 'GET |');
    const out = await createAiInlineProvider(fakeMonaco(), d).provideInlineCompletions(m, position, EXPLICIT, token() as never);
    expect((out as { items: unknown[] }).items).toEqual([]);
    expect(d.calls).toEqual([]);
  });

  it('a consent from before MongoDB was named covers SQL tabs only', () => {
    useAi.setState({ consentedMongo: false });
    expect(aiAllowed(MONGO)).toBe(false);
    expect(aiAllowed(CONN)).toBe(true);
    useAi.setState({ consentedMongo: true });
    expect(aiAllowed(MONGO)).toBe(true);
  });

  it('a MongoDB connection tagged Production follows the same default as SQL', () => {
    useConnections.getState().setStored([], [config(MONGO, 'docs', { kind: 'mongo', env_tag: 'production' })]);
    expect(aiAllowed(MONGO)).toBe(false);
    useAi.setState({ choices: new Map([[MONGO, true]]) });
    expect(aiAllowed(MONGO)).toBe(true);
  });

  it('registers for the SQL languages and mongo, not redis', () => {
    const langs: string[] = [];
    const monaco = {
      ...fakeMonaco(),
      languages: {
        InlineCompletionTriggerKind: { Automatic: 0, Explicit: 1 },
        registerInlineCompletionsProvider: (lang: string) => {
          langs.push(lang);
          return { dispose() {} };
        },
      },
    } as unknown as typeof Monaco;
    ensureAiInline(monaco);
    expect(langs).toEqual(['mysql', 'pgsql', 'mongo']);
  });
});
