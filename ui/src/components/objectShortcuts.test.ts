import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('../dialogs/resultExport', () => ({ openExportDialog: vi.fn() }));

import { openExportDialog } from '../dialogs/resultExport';
import { onObjectShortcut } from './objectShortcuts';

function key(init: KeyboardEventInit, altGraph = false) {
  const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
  Object.defineProperty(e, 'getModifierState', { value: (m: string) => m === 'AltGraph' && altGraph });
  return e;
}

describe('onObjectShortcut', () => {
  afterEach(() => vi.mocked(openExportDialog).mockClear());

  it('Ctrl+Alt+E opens the export dialog', () => {
    const e = key({ key: 'e', code: 'KeyE', ctrlKey: true, altKey: true });
    expect(onObjectShortcut(e)).toBe(true);
    expect(e.defaultPrevented).toBe(true);
    expect(openExportDialog).toHaveBeenCalledOnce();
  });

  it('AltGr+E (reported as Ctrl+Alt on Windows) types its character instead', () => {
    const e = key({ key: 'ę', code: 'KeyE', ctrlKey: true, altKey: true }, true);
    expect(onObjectShortcut(e)).toBe(false);
    expect(e.defaultPrevented).toBe(false);
    expect(openExportDialog).not.toHaveBeenCalled();
  });
});
