import { act, createEvent, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { emptyRun, startRun, useQueryRun } from '../store/queryRun';
import { groupTabs, useTabs, type ObjectTabRef, type QueryTabRef } from '../store/tabs';
import { resetStores } from '../test/backend';
import { installRunMarks, useRunMarks } from '../store/runMarks';
import { TabBar } from './TabBar';
import { setDraggedTab, useDrag } from './tabDrag';

const s = () => useTabs.getState();
const obj = (id: string): ObjectTabRef => ({ id, kind: 'object', connectionId: 'c1', title: id, objectRef: { database: 'd', schema: null, name: id, kind: 'Table' } });
const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });
const PLAN = {
  plan_id: 'p1',
  stmts: [{ index: 0, range: [0, 8] as [number, number], kind: 'read' as const, verdict: 'allow' as const, preview: 'S0' }],
  dialect: { engine: 'mysql' as const },
};

beforeEach(resetStores);
afterEach(() => {
  useDrag.setState({ item: null });
  vi.restoreAllMocks();
});

/** Each tab is 100px wide, side by side from x = 0. */
function layoutTabs() {
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
    const tabs = [...(this.parentElement?.querySelectorAll('.tab') ?? [])];
    const i = tabs.indexOf(this);
    const left = i >= 0 ? i * 100 : 0;
    return { left, right: left + 100, width: 100, top: 0, bottom: 34, height: 34, x: left, y: 0, toJSON() {} } as DOMRect;
  });
}

function dragAt(type: 'dragOver' | 'drop', el: Element, x: number) {
  const e = createEvent[type](el, { dataTransfer: { types: ['text/x-dnt-tab'], dropEffect: '', getData: () => '' } });
  Object.defineProperties(e, { clientX: { value: x }, clientY: { value: 10 } });
  fireEvent(el, e);
}

describe('TabBar (editor v2)', () => {
  it('shows an insertion bar where a dragged tab lands and reorders on drop', () => {
    layoutTabs();
    act(() => ['a', 'b', 'c'].forEach((id) => s().open(obj(id))));
    const { container } = render(<TabBar />);
    act(() => setDraggedTab('c'));
    const strip = container.querySelector('.tabstrip')!;
    dragAt('dragOver', strip, 40); // left half of a
    const list = container.querySelector('.tabbar')!;
    expect(list.firstElementChild).toHaveClass('tab-insert');
    dragAt('drop', strip, 40);
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['c', 'a', 'b']);
    expect(container.querySelector('.tab-insert')).toBeNull();
  });

  it('dropping after the last tab appends; dragging rightwards accounts for the tab itself', () => {
    layoutTabs();
    act(() => ['a', 'b', 'c'].forEach((id) => s().open(obj(id))));
    const { container } = render(<TabBar />);
    act(() => setDraggedTab('a'));
    const strip = container.querySelector('.tabstrip')!;
    dragAt('drop', strip, 260); // right half of c
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['b', 'c', 'a']);
    act(() => setDraggedTab('b'));
    dragAt('drop', strip, 160); // left half of a (index 2): b lands between c and a
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['c', 'b', 'a']);
  });

  it('a preview tab has an italic title; double-click keeps it, double-click on a kept tab maximizes the group', () => {
    act(() => {
      s().open(obj('a'));
      s().splitTab('a', 'right');
      s().openPreview(obj('p'));
    });
    const { container } = render(<TabBar />);
    const tab = () => container.querySelector('[data-tab-id="p"]')!;
    expect(tab()).toHaveClass('preview');
    fireEvent.doubleClick(screen.getByRole('tab', { name: /p/ }));
    expect(tab()).not.toHaveClass('preview');
    expect(s().maximizedGroupId).toBeUndefined();
    fireEvent.doubleClick(screen.getByRole('tab', { name: /p/ }));
    expect(s().maximizedGroupId).toBe(s().focusedGroupId);
  });

  it('a maximized group says so in its strip, with a restore button', () => {
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
    });
    render(<TabBar />);
    expect(screen.queryByRole('button', { name: /Restore editor layout/ })).toBeNull();
    act(() => s().toggleMaximize());
    fireEvent.click(screen.getByRole('button', { name: 'Restore editor layout (Ctrl+K Ctrl+M)' }));
    expect(s().maximizedGroupId).toBeUndefined();
  });

  it('shows a spinner while a tab runs, and a result mark when a run ends out of sight', () => {
    const stop = installRunMarks();
    try {
      act(() => {
        s().open(q('r'));
        s().open(q('other'));
      });
      render(<TabBar />);
      act(() => useQueryRun.setState({ runs: new Map([['r', startRun(emptyRun, PLAN as never, 0)]]) }));
      expect(screen.getByRole('img', { name: 'Running' })).toBeInTheDocument();
      // The run ends while "other" is shown: r gets a mark until it is looked at.
      act(() => useQueryRun.setState({ runs: new Map([['r', { ...emptyRun, lastRun: { rows: 1, elapsedMs: 5 } }]]) }));
      expect(screen.queryByRole('img', { name: 'Running' })).toBeNull();
      expect(screen.getByRole('img', { name: 'Finished' })).toBeInTheDocument();
      act(() => s().activate('r'));
      expect(screen.queryByRole('img', { name: 'Finished' })).toBeNull();
      expect(useRunMarks.getState().marks.size).toBe(0);
    } finally {
      stop();
    }
  });

  it('a failed run out of sight gets a Failed mark', () => {
    const stop = installRunMarks();
    try {
      act(() => {
        s().open(q('r'));
        s().open(q('other'));
      });
      render(<TabBar />);
      act(() => useQueryRun.setState({ runs: new Map([['r', startRun(emptyRun, PLAN as never, 0)]]) }));
      act(() => useQueryRun.setState({ runs: new Map([['r', { ...emptyRun, lastRun: { rows: 0, elapsedMs: 5, errors: 1 } }]]) }));
      expect(screen.getByRole('img', { name: 'Failed' })).toBeInTheDocument();
    } finally {
      stop();
    }
  });
});
