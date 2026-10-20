import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  EDITOR_MIN_PX,
  EDITOR_RATIO_DEFAULT,
  FONT_SCALE_MAX,
  FONT_SCALE_MIN,
  RESULT_MIN_PX,
  SIDEBAR_DEFAULT,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  editorPixels,
  reloadLayout,
  useLayout,
} from './layout';

beforeEach(() => {
  localStorage.clear();
  useLayout.getState().reset();
  localStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('layout store', () => {
  it('starts with the defaults', () => {
    const s = useLayout.getState();
    expect(s.sidebarWidth).toBe(SIDEBAR_DEFAULT);
    expect(s.sidebarVisible).toBe(true);
    expect(s.editorRatio).toBe(EDITOR_RATIO_DEFAULT);
  });

  it('clamps the sidebar width', () => {
    useLayout.getState().setSidebarWidth(10);
    expect(useLayout.getState().sidebarWidth).toBe(SIDEBAR_MIN);
    useLayout.getState().setSidebarWidth(5000);
    expect(useLayout.getState().sidebarWidth).toBe(SIDEBAR_MAX);
    useLayout.getState().setSidebarWidth(321.4);
    expect(useLayout.getState().sidebarWidth).toBe(321);
    useLayout.getState().setSidebarWidth(Number.NaN);
    expect(useLayout.getState().sidebarWidth).toBe(SIDEBAR_DEFAULT);
  });

  it('clamps the editor ratio to a sane fraction', () => {
    useLayout.getState().setEditorRatio(-1);
    expect(useLayout.getState().editorRatio).toBeGreaterThan(0);
    useLayout.getState().setEditorRatio(7);
    expect(useLayout.getState().editorRatio).toBeLessThan(1);
    useLayout.getState().setEditorRatio(0.4);
    expect(useLayout.getState().editorRatio).toBe(0.4);
  });

  it('toggles the sidebar and resets everything', () => {
    const s = useLayout.getState();
    s.toggleSidebar();
    s.setSidebarWidth(400);
    s.setEditorRatio(0.3);
    expect(useLayout.getState().sidebarVisible).toBe(false);
    useLayout.getState().reset();
    const r = useLayout.getState();
    expect([r.sidebarWidth, r.sidebarVisible, r.editorRatio]).toEqual([SIDEBAR_DEFAULT, true, EDITOR_RATIO_DEFAULT]);
  });

  it('persists to localStorage and reads it back', () => {
    const s = useLayout.getState();
    s.setSidebarWidth(350);
    s.setEditorRatio(0.25);
    s.toggleSidebar();
    expect(JSON.parse(localStorage.getItem('dnt.layout') ?? '{}')).toEqual({
      sidebarWidth: 350,
      sidebarVisible: false,
      editorRatio: 0.25,
      resultsVisible: true,
      fontScale: 1,
    });
    useLayout.setState({ sidebarWidth: 1, sidebarVisible: true, editorRatio: 0.9 });
    reloadLayout();
    const r = useLayout.getState();
    expect([r.sidebarWidth, r.sidebarVisible, r.editorRatio]).toEqual([350, false, 0.25]);
  });

  it('ignores a corrupt or out-of-range stored value', () => {
    localStorage.setItem('dnt.layout', 'not json');
    reloadLayout();
    expect(useLayout.getState().sidebarWidth).toBe(SIDEBAR_DEFAULT);
    localStorage.setItem('dnt.layout', JSON.stringify({ sidebarWidth: 99999, sidebarVisible: 'yes', editorRatio: 'x' }));
    reloadLayout();
    const r = useLayout.getState();
    expect([r.sidebarWidth, r.sidebarVisible, r.editorRatio]).toEqual([SIDEBAR_MAX, true, EDITOR_RATIO_DEFAULT]);
  });

  it('survives a throwing localStorage (reads and writes)', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('denied');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('denied');
    });
    expect(() => reloadLayout()).not.toThrow();
    expect(useLayout.getState().sidebarWidth).toBe(SIDEBAR_DEFAULT);
    expect(() => useLayout.getState().setSidebarWidth(300)).not.toThrow();
    expect(useLayout.getState().sidebarWidth).toBe(300);
  });
});

describe('result panel flag', () => {
  it('toggles, persists and survives a reset as visible', () => {
    expect(useLayout.getState().resultsVisible).toBe(true);
    useLayout.getState().toggleResults();
    expect(useLayout.getState().resultsVisible).toBe(false);
    expect(JSON.parse(localStorage.getItem('dnt.layout') ?? '{}').resultsVisible).toBe(false);
    useLayout.setState({ resultsVisible: true });
    reloadLayout();
    expect(useLayout.getState().resultsVisible).toBe(false);
    useLayout.getState().reset();
    expect(useLayout.getState().resultsVisible).toBe(true);
  });
});

describe('font scale', () => {
  afterEach(() => useLayout.getState().setFontScale(1));

  it('clamps to 0.8 to 1.6 in 0.1 steps without float noise', () => {
    const { setFontScale, zoomFont } = useLayout.getState();
    setFontScale(5);
    expect(useLayout.getState().fontScale).toBe(FONT_SCALE_MAX);
    setFontScale(0.1);
    expect(useLayout.getState().fontScale).toBe(FONT_SCALE_MIN);
    setFontScale(Number.NaN);
    expect(useLayout.getState().fontScale).toBe(1);
    setFontScale(1);
    for (let i = 0; i < 3; i++) zoomFont(1);
    expect(useLayout.getState().fontScale).toBe(1.3);
    for (let i = 0; i < 20; i++) zoomFont(1);
    expect(useLayout.getState().fontScale).toBe(1.6);
    for (let i = 0; i < 20; i++) zoomFont(-1);
    expect(useLayout.getState().fontScale).toBe(0.8);
  });

  it('persists, is mirrored to --font-scale, and a reset keeps it', () => {
    useLayout.getState().setFontScale(1.2);
    expect(JSON.parse(localStorage.getItem('dnt.layout') ?? '{}').fontScale).toBe(1.2);
    expect(document.documentElement.style.getPropertyValue('--font-scale')).toBe('1.2');
    useLayout.getState().reset();
    expect(useLayout.getState().fontScale).toBe(1.2);
    useLayout.setState({ fontScale: 1 });
    reloadLayout();
    expect(useLayout.getState().fontScale).toBe(1.2);
    localStorage.setItem('dnt.layout', JSON.stringify({ fontScale: 9 }));
    reloadLayout();
    expect(useLayout.getState().fontScale).toBe(1.6);
  });
});

describe('editorPixels', () => {
  it('applies the ratio', () => {
    expect(editorPixels(0.6, 1000)).toBe(600);
  });
  it('keeps the editor at least its minimum', () => {
    expect(editorPixels(0.05, 1000)).toBe(EDITOR_MIN_PX);
  });
  it('leaves the result panel its minimum', () => {
    expect(editorPixels(0.95, 1000)).toBe(1000 - RESULT_MIN_PX);
  });
  it('prefers the editor minimum in a tiny body', () => {
    expect(editorPixels(0.6, 150)).toBe(EDITOR_MIN_PX);
  });
});
