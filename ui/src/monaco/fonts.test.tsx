import { act, render } from '@testing-library/react';
import { useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ThemeProvider } from '../theme/ThemeProvider';
import { SqlEditor } from '../tabs/query/SqlEditor';
import { MONO_FALLBACK, codeFontSizes, editorFontOptions, monoFontFamily } from './fonts';

type EditorProps = { value?: string; options?: Record<string, unknown>; onChange?: (v: string | undefined) => void };
const renders: EditorProps[] = [];

vi.mock('@monaco-editor/react', async () => {
  const React = await import('react');
  return {
    default: (props: EditorProps) => {
      renders.push(props);
      return React.createElement('div', { 'data-testid': 'monaco-mock' });
    },
    loader: { config: () => {} },
  };
});

afterEach(() => {
  document.documentElement.style.removeProperty('--font-mono');
  document.documentElement.style.removeProperty('--text-callout');
  renders.length = 0;
});

describe('code fonts', () => {
  it('mono_font_comes_from_the_token', () => {
    expect(monoFontFamily()).toBe(MONO_FALLBACK);
    document.documentElement.style.setProperty('--font-mono', "'Iosevka', monospace");
    expect(monoFontFamily()).toBe("'Iosevka', monospace");
  });

  it('a_ligature_font_gets_its_ligature_free_twin_first', () => {
    document.documentElement.style.setProperty('--font-mono', "ui-monospace, 'Cascadia Code', Consolas, monospace");
    expect(monoFontFamily()).toBe("ui-monospace, 'Cascadia Mono', 'Cascadia Code', Consolas, monospace");
  });

  it('sizes_follow_the_type_scale', () => {
    expect(codeFontSizes()).toEqual({ body: 13, bodyLineHeight: 18, header: 12, marker: 11 });
    document.documentElement.style.setProperty('--text-callout', '14px');
    expect(editorFontOptions()).toMatchObject({ fontSize: 14, suggestFontSize: 14, fontLigatures: false });
  });
});

describe('SqlEditor options', () => {
  const doc = { id: 'q-fonts', sql: 'select 1', connectionId: null, database: null } as unknown as Parameters<
    typeof SqlEditor
  >[0]['doc'];

  it('uses_the_token_font_and_no_word_based_suggestions', () => {
    render(<ThemeProvider><SqlEditor doc={doc} engine="mysql" /></ThemeProvider>);
    const options = renders.at(-1)?.options ?? {};
    expect(options).toMatchObject({
      fontFamily: monoFontFamily(),
      fontLigatures: false,
      wordBasedSuggestions: 'off',
      suggestFontSize: options.fontSize,
      suggestLineHeight: options.lineHeight,
    });
  });

  it('typing_does_not_re_render_the_editor', () => {
    // The parent re-renders with the store's new document on every keystroke (QueryTab).
    let setSql: (sql: string) => void = () => {};
    function Host() {
      const [sql, set] = useState(doc.sql);
      setSql = set;
      return <SqlEditor doc={{ ...doc, sql }} engine="mysql" />;
    }
    render(<ThemeProvider><Host /></ThemeProvider>);
    const first = renders.at(-1);
    const count = renders.length;
    act(() => {
      first?.onChange?.('select 12');
      // The store echoes the editor's own text back: no new render.
      setSql('select 12');
    });
    expect(renders).toHaveLength(count);
    // A change from elsewhere (e.g. a file opened) does reach the editor, same options object.
    act(() => setSql('select 99'));
    expect(renders.at(-1)?.value).toBe('select 99');
    expect(renders.at(-1)?.options).toBe(first?.options);
  });
});
