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
    'editor.background': token('--dnt-bg-app', '#ffffff'),
    'editor.foreground': token('--dnt-fg', '#131315'),
    'editorGutter.background': token('--dnt-bg-app', '#ffffff'),
    'editorLineNumber.foreground': token('--dnt-fg-subtle', '#707076'),
    'editorLineNumber.activeForeground': token('--dnt-fg-muted', '#5a5a60'),
    'editor.selectionBackground': token('--dnt-bg-selected', '#dce7fd'),
    'editor.inactiveSelectionBackground': token('--dnt-bg-selected-muted', '#ebebed'),
    'editor.lineHighlightBackground': token('--dnt-bg-sunken', '#f4f4f5'),
    'editorCursor.foreground': token('--dnt-fg', '#131315'),
    'editorWidget.background': token('--dnt-bg-raised', '#ffffff'),
    'editorWidget.border': token('--dnt-border', '#dedee1'),
    'editorHoverWidget.background': token('--dnt-bg-raised', '#ffffff'),
    'editorHoverWidget.border': token('--dnt-border', '#dedee1'),
    'editorSuggestWidget.background': token('--dnt-bg-raised', '#ffffff'),
    'editorSuggestWidget.border': token('--dnt-border', '#dedee1'),
    'editorSuggestWidget.selectedBackground': token('--dnt-bg-selected', '#dce7fd'),
    'scrollbarSlider.background': token('--dnt-border-strong', '#c6c6ca'),
  });
  monaco.editor.defineTheme(monacoThemeName(dark), { base: dark ? 'vs-dark' : 'vs', inherit: true, rules: [], colors: colors() });
}

/**
 * The app theme for a Monaco editor: pass the returned theme and beforeMount to <Editor>. The theme is
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
