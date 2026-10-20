// The code font of every monospace surface drawn outside CSS (Monaco, the Glide grid canvas),
// read from the design tokens so they match the CSS surfaces (`var(--font-mono)`) exactly.

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

/** Code text sizes on the type scale: editor and grid cells (callout), grid header (footnote). */
export function codeFontSizes() {
  return {
    /** Editor text, suggest rows and grid cells. */
    body: cssPx('--text-callout', 13),
    bodyLineHeight: cssPx('--text-callout--line-height', 18),
    /** Grid header. */
    header: cssPx('--text-footnote', 12),
    /** Grid row markers. */
    marker: cssPx('--text-caption', 11),
  };
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
