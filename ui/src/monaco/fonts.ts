// The code font of every monospace surface drawn outside CSS (Monaco, the Glide grid canvas),
// read from the design tokens so they match the CSS surfaces (`var(--font-mono)`) exactly.
import { useLayout } from '../store/layout';

/** Used when the token cannot be read (tests, before the CSS loaded). Ligature-free first. */
export const MONO_FALLBACK = "'Cascadia Mono', Consolas, 'SF Mono', Menlo, 'DejaVu Sans Mono', monospace";

/** The value of a custom property on the root element, or `fallback`. */
export function cssVar(name: string, fallback: string): string {
  try {
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v || fallback;
  } catch {
    return fallback;
  }
}

/** A `px` token of the type scale as a number (`--text-callout` → 13). */
export function cssPx(name: string, fallback: number): number {
  const n = parseFloat(cssVar(name, ''));
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

/**
 * `--font-mono`, ligature-free: a canvas cannot switch ligatures off, so 'Cascadia Code'
 * (whose ligatures turn `>=` into one glyph) gets its ligature-free twin 'Cascadia Mono'
 * in front of it.
 */
export function monoFontFamily(): string {
  const stack = cssVar('--font-mono', MONO_FALLBACK);
  if (/cascadia code/i.test(stack) && !/cascadia mono/i.test(stack)) {
    return stack.replace(/(['"]?)cascadia code\1/i, "'Cascadia Mono', 'Cascadia Code'");
  }
  return stack;
}

const scaled = (px: number, scale: number) => Math.round(px * scale * 10) / 10;

/**
 * Code text sizes on the type scale: editor and grid cells (callout), grid header (footnote),
 * times the user's font scale (Ctrl+= / Ctrl+-, `useLayout().fontScale`).
 */
export function codeFontSizes(scale: number = useLayout.getState().fontScale) {
  return {
    /** Editor text, suggest rows and grid cells. */
    body: scaled(cssPx('--text-callout', 13), scale),
    bodyLineHeight: Math.round(cssPx('--text-callout--line-height', 18) * scale),
    /** Grid header. */
    header: scaled(cssPx('--text-footnote', 12), scale),
    /** Grid row markers. */
    marker: scaled(cssPx('--text-caption', 11), scale),
  };
}

/** The part of a Monaco editor the font scale needs. */
interface ScalableEditor {
  updateOptions(options: Record<string, unknown>): void;
  onDidDispose(listener: () => void): unknown;
}

/**
 * Keeps an editor's font at the current font scale (applied now, then on every change of
 * the layout store's `fontScale`); stops when the editor is disposed.
 */
export function bindEditorFontScale(editor: ScalableEditor): void {
  const apply = () => {
    const { body, bodyLineHeight } = codeFontSizes();
    editor.updateOptions({ fontSize: body, lineHeight: bodyLineHeight, suggestFontSize: body, suggestLineHeight: bodyLineHeight });
  };
  apply();
  const off = useLayout.subscribe((s, prev) => {
    if (s.fontScale !== prev.fontScale) apply();
  });
  editor.onDidDispose(off);
}

/** Font options shared by every Monaco editor (query editor, DDL viewer). */
export function editorFontOptions() {
  const { body, bodyLineHeight } = codeFontSizes();
  return {
    fontFamily: monoFontFamily(),
    fontSize: body,
    lineHeight: bodyLineHeight,
    fontLigatures: false,
    // The suggest widget renders its rows in the editor font; same size and row height.
    suggestFontSize: body,
    suggestLineHeight: bodyLineHeight,
  } as const;
}
