import { useEffect, useRef } from 'react';
import type * as Monaco from 'monaco-editor';
import { useTheme } from '../theme/ThemeProvider';

type MonacoApi = typeof Monaco;

export const monacoThemeName = (dark: boolean) => (dark ? 'dnt-dark' : 'dnt-light');

const hex2 = (n: number) => Math.round(Math.min(255, Math.max(0, n))).toString(16).padStart(2, '0');

/**
 * A computed CSS colour as Monaco's theme format: `#rrggbb`, or `#rrggbbaa` when translucent. Monaco parses
 * theme colours with `Color.fromHex`, which turns anything else (`rgba(…)`) into red. `null` when unparseable.
 */
export function toMonacoColor(css: string): string | null {
  const v = css.trim().toLowerCase();
  if (/^#([0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/.test(v)) return v;
  const m = /^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)(?:[\s,/]+([\d.]+%?))?\s*\)$/.exec(v);
  if (!m) return null;
  const rgb = `#${hex2(Number(m[1]))}${hex2(Number(m[2]))}${hex2(Number(m[3]))}`;
  if (m[4] === undefined) return rgb;
  const alpha = m[4].endsWith('%') ? Number(m[4].slice(0, -1)) / 100 : Number(m[4]);
  return alpha >= 1 ? rgb : `${rgb}${hex2(alpha * 255)}`;
}

/** A CSS token resolved to a colour Monaco accepts (see toMonacoColor); `fallback` when it cannot be resolved. */
function token(name: string, fallback: string): string {
  try {
    const probe = document.createElement('span');
    probe.style.color = `var(${name}, ${fallback})`;
    probe.style.display = 'none';
    document.body.appendChild(probe);
    const resolved = getComputedStyle(probe).color;
    probe.remove();
    return toMonacoColor(resolved) ?? fallback;
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
