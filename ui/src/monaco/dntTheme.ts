import { useEffect, useRef } from 'react';
import type * as Monaco from 'monaco-editor';
import { useTheme } from '../theme/ThemeProvider';

type MonacoApi = typeof Monaco;

export const monacoThemeName = (dark: boolean) => (dark ? 'dnt-dark' : 'dnt-light');

/** A CSS token resolved to a color Monaco accepts (hex / rgba); `fallback` when it cannot be resolved. */
function token(name: string, fallback: string): string {
  try {
    const probe = document.createElement('span');
    probe.style.color = `var(${name}, ${fallback})`;
    probe.style.display = 'none';
    document.body.appendChild(probe);
    const resolved = getComputedStyle(probe).color;
    probe.remove();
    const ctx = document.createElement('canvas').getContext('2d');
    if (!ctx || !resolved) return fallback;
    ctx.fillStyle = '#000000';
    ctx.fillStyle = resolved;
    return ctx.fillStyle;
  } catch {
    return fallback;
  }
}

/**
 * Defines the theme of the current appearance from the design tokens, so the editor matches the app palette
 * (Monaco's own `vs-dark` is #1e1e1e, the app is #212121). Call again after the theme class changed.
 */
export function defineDntTheme(monaco: MonacoApi, dark: boolean): void {
  const colors = () => ({
    'editor.background': token('--studio-bg', '#ffffff'),
    'editor.foreground': token('--studio-fg', '#1d1d1f'),
    'editorGutter.background': token('--studio-bg', '#ffffff'),
    'editorLineNumber.foreground': token('--studio-fg-tertiary', '#8e8e93'),
    'editorLineNumber.activeForeground': token('--studio-fg-secondary', '#636366'),
    'editor.selectionBackground': token('--studio-selected', '#cfe3ff'),
    'editor.inactiveSelectionBackground': token('--studio-hover', '#e8e8ed'),
    'editor.lineHighlightBackground': token('--studio-subtle', '#f5f5f7'),
    'editorCursor.foreground': token('--studio-fg', '#1d1d1f'),
    'editorWidget.background': token('--studio-popover', '#ffffff'),
    'editorWidget.border': token('--studio-hairline', '#d2d2d7'),
    'editorHoverWidget.background': token('--studio-popover', '#ffffff'),
    'editorHoverWidget.border': token('--studio-hairline', '#d2d2d7'),
    'editorSuggestWidget.background': token('--studio-popover', '#ffffff'),
    'editorSuggestWidget.border': token('--studio-hairline', '#d2d2d7'),
    'editorSuggestWidget.selectedBackground': token('--studio-selected', '#cfe3ff'),
    'scrollbarSlider.background': token('--studio-hairline', '#d2d2d7'),
  });
  monaco.editor.defineTheme(monacoThemeName(dark), { base: dark ? 'vs-dark' : 'vs', inherit: true, rules: [], colors: colors() });
}

/**
 * The app theme for a Monaco editor: pass 	heme and eforeMount to <Editor>. The theme is
 * redefined from the tokens when the appearance changes (ThemeProvider's layout effect, which
 * sets the class, runs before this effect).
 */
export function useDntMonacoTheme() {
  const { dark } = useTheme();
  const monacoRef = useRef<MonacoApi | null>(null);
  useEffect(() => {
    const monaco = monacoRef.current;
    if (!monaco) return;
    defineDntTheme(monaco, dark);
    monaco.editor.setTheme(monacoThemeName(dark));
  }, [dark]);
  return {
    theme: monacoThemeName(dark),
    beforeMount: (monaco: MonacoApi) => {
      monacoRef.current = monaco;
      defineDntTheme(monaco, dark);
    },
  };
}