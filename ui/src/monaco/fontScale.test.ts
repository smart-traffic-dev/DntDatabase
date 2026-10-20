import { afterEach, describe, expect, it, vi } from 'vitest';
import { useLayout } from '../store/layout';
import { bindEditorFontScale, codeFontSizes, editorFontOptions } from './fonts';

afterEach(() => useLayout.getState().setFontScale(1));

describe('code font scale', () => {
  it('multiplies the sizes of the type scale', () => {
    expect(codeFontSizes(1)).toEqual({ body: 13, bodyLineHeight: 18, header: 12, marker: 11 });
    expect(codeFontSizes(1.5)).toEqual({ body: 19.5, bodyLineHeight: 27, header: 18, marker: 16.5 });
    expect(codeFontSizes(0.8).body).toBe(10.4);
  });

  it('reads the layout store when no scale is given (grid theme, editor options)', () => {
    useLayout.getState().setFontScale(1.2);
    expect(codeFontSizes().body).toBe(15.6);
    expect(editorFontOptions().fontSize).toBe(15.6);
    expect(editorFontOptions().lineHeight).toBe(22);
  });

  it('keeps an editor at the current scale until it is disposed', () => {
    let dispose: () => void = () => {};
    const editor = {
      updateOptions: vi.fn(),
      onDidDispose: vi.fn((cb: () => void) => {
        dispose = cb;
      }),
    };
    bindEditorFontScale(editor);
    expect(editor.updateOptions).toHaveBeenLastCalledWith({ fontSize: 13, lineHeight: 18, suggestFontSize: 13, suggestLineHeight: 18 });
    useLayout.getState().zoomFont(2);
    expect(editor.updateOptions).toHaveBeenLastCalledWith({ fontSize: 15.6, lineHeight: 22, suggestFontSize: 15.6, suggestLineHeight: 22 });
    // Other layout changes do not touch the editor.
    const calls = editor.updateOptions.mock.calls.length;
    useLayout.getState().toggleSidebar();
    useLayout.getState().toggleSidebar();
    expect(editor.updateOptions.mock.calls.length).toBe(calls);
    dispose();
    useLayout.getState().zoomFont(1);
    expect(editor.updateOptions.mock.calls.length).toBe(calls);
  });
});
