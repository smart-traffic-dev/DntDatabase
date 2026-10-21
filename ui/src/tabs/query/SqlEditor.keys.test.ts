import { describe, expect, it, vi } from 'vitest';

const run = vi.hoisted(() => ({ runFromEditor: vi.fn(), explainQuery: vi.fn(), stopQuery: vi.fn() }));
vi.mock('../../store/runQuery', async (importOriginal) => ({ ...(await importOriginal<object>()), ...run }));

import { bindRunKeys } from './SqlEditor';

// Monaco's KeyMod / KeyCode values (only the ones bindRunKeys uses).
const monaco = {
  KeyMod: { CtrlCmd: 2048, Shift: 1024 },
  KeyCode: { Enter: 3, KeyE: 35, Escape: 9 },
} as never;

interface Action {
  id: string;
  keybindings?: number[];
  precondition?: string;
  run(): void;
}

function fakeEditor() {
  const actions: Action[] = [];
  const disposers: (() => void)[] = [];
  const editor = {
    actions,
    addCommand: vi.fn(),
    addAction: vi.fn((a: Action) => {
      actions.push(a);
      return { dispose: vi.fn() };
    }),
    onDidDispose: (f: () => void) => void disposers.push(f),
    onDidChangeCursorSelection: () => ({ dispose() {} }),
    getPosition: () => null,
    getModel: () => null,
    getSelections: () => [],
    createContextKey: () => ({ set() {} }),
    dispose: () => disposers.forEach((f) => f()),
  };
  return editor;
}

const byKey = (e: ReturnType<typeof fakeEditor>, key: number) => e.actions.find((a) => a.keybindings?.includes(key));

describe('run keys of a query editor', () => {
  it('are bound per editor (addAction), never through the global addCommand', () => {
    const a = fakeEditor();
    const b = fakeEditor();
    bindRunKeys('tab-a', a as never, monaco);
    bindRunKeys('tab-b', b as never, monaco);
    expect(a.addCommand).not.toHaveBeenCalled();
    expect(b.addCommand).not.toHaveBeenCalled();

    // Ctrl+Enter in editor A runs tab A even though B was mounted later.
    byKey(a, 2048 | 3)!.run();
    expect(run.runFromEditor).toHaveBeenLastCalledWith('tab-a', 'cursor');
    byKey(a, 2048 | 1024 | 3)!.run();
    expect(run.runFromEditor).toHaveBeenLastCalledWith('tab-a', 'all');
    byKey(a, 2048 | 1024 | 35)!.run();
    expect(run.explainQuery).toHaveBeenLastCalledWith('tab-a', false);
    const esc = byKey(a, 9)!;
    expect(esc.precondition).toBe('dntRunning');
    esc.run();
    expect(run.stopQuery).toHaveBeenLastCalledWith('tab-a');
  });

  it('unbinds them when the editor goes away', () => {
    const a = fakeEditor();
    bindRunKeys('tab-a', a as never, monaco);
    const handles = a.addAction.mock.results.map((r) => r.value as { dispose: ReturnType<typeof vi.fn> });
    a.dispose();
    for (const h of handles.slice(0, 4)) expect(h.dispose).toHaveBeenCalled();
  });
});
