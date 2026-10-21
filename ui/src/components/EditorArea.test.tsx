import { act, createEvent, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

// The tabs themselves are tested on their own; here only which one a group shows matters.
vi.mock('../tabs/ObjectTab', () => ({ ObjectTab: ({ tab }: { tab: { id: string } }) => <div data-testid="object-tab">{tab.id}</div> }));
vi.mock('../tabs/query/QueryTab', () => ({ QueryTab: ({ tab }: { tab: { id: string } }) => <div data-testid="query-tab">{tab.id}</div> }));
import { useTabs, type ObjectTabRef } from '../store/tabs';
import { resetStores } from '../test/backend';
import { dropZone, EditorArea } from './EditorArea';
import { setDraggedTab } from './tabDrag';

const s = () => useTabs.getState();
const obj = (id: string): ObjectTabRef => ({ id, kind: 'object', connectionId: 'c1', title: id, objectRef: { database: 'd', schema: null, name: id, kind: 'Table' } });

beforeEach(resetStores);

describe('dropZone', () => {
  const rect = { left: 0, top: 0, width: 400, height: 300 };
  it('edges split, the middle moves', () => {
    expect(dropZone(rect, 380, 150)).toBe('right');
    expect(dropZone(rect, 200, 290)).toBe('down');
    expect(dropZone(rect, 200, 150)).toBe('center');
    expect(dropZone(rect, 10, 150)).toBe('center');
    expect(dropZone({ left: 0, top: 0, width: 0, height: 0 }, 0, 0)).toBe('center');
  });
});

describe('EditorArea', () => {
  it('renders one pane per group in the layout', () => {
    const { container } = render(<EditorArea />);
    expect(screen.getAllByTestId('editor-group')).toHaveLength(1);
    expect(screen.getByTestId('workspace-empty')).toBeInTheDocument();
    act(() => s().setLayout('grid'));
    expect(screen.getAllByTestId('editor-group')).toHaveLength(4);
    expect(container.querySelector('.editor-area')).toHaveClass('layout-grid');
    // Empty extra groups say how to fill them.
    expect(screen.getAllByText('Drop a tab here or open one')).toHaveLength(3);
  });

  it('a pointer down inside a group focuses it', () => {
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
    });
    render(<EditorArea />);
    const [first] = screen.getAllByTestId('editor-group');
    fireEvent.mouseDown(first!);
    expect(s().focusedGroupId).toBe(s().groups[0]!.id);
    expect(first).toHaveClass('focused');
  });

  it('the splitter between two columns resets to equal sizes on double-click', () => {
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
      s().setRatios({ x: 0.3 });
    });
    render(<EditorArea />);
    fireEvent.doubleClick(screen.getByRole('separator', { name: 'Resize editor groups' }));
    expect(s().ratios.x).toBe(0.5);
  });

  it('an edge drop only previews (and does) a split when one can happen', () => {
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({ left: 0, top: 0, width: 400, height: 300, right: 400, bottom: 300, x: 0, y: 0, toJSON() {} } as DOMRect);
    try {
      act(() => {
        for (const id of ['a', 'b', 'c', 'd']) s().open(obj(id));
        s().splitTab('d', 'right');
      });
      render(<EditorArea />);
      const types = ['text/x-dnt-tab'];
      const over = (pane: Element, id: string) => {
        setDraggedTab(id);
        // jsdom has no DragEvent: put the pointer position on the event by hand.
        const e = createEvent.dragOver(pane, { dataTransfer: { types, dropEffect: '' } });
        Object.defineProperties(e, { clientX: { value: 390 }, clientY: { value: 150 } });
        fireEvent(pane, e);
        return pane.querySelector('.drop-zone')?.className;
      };
      const [left, right] = screen.getAllByTestId('group-content');
      // d is the only tab of the right group: splitting it off its own group would change nothing.
      expect(over(right!, 'd')).toBe('drop-zone zone-center');
      // a from the left group onto the right edge of the right group: a real split.
      expect(over(right!, 'a')).toBe('drop-zone zone-right');
      // With four groups there is no room for another: the edge just moves the tab.
      act(() => s().setLayout('grid'));
      expect(over(screen.getAllByTestId('group-content')[0]!, 'b')).toBe('drop-zone zone-center');
      void left;
    } finally {
      setDraggedTab(null);
      vi.restoreAllMocks();
    }
  });

  it('dropping a tab in the middle of another group moves it there', () => {
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
    });
    render(<EditorArea />);
    const data = new Map([['text/x-dnt-tab', 'a']]);
    const dataTransfer = { getData: (k: string) => data.get(k) ?? '', types: ['text/x-dnt-tab'], dropEffect: '' };
    const second = screen.getAllByTestId('group-content')[1]!;
    fireEvent.dragOver(second, { dataTransfer });
    fireEvent.drop(second, { dataTransfer });
    expect(s().groups).toHaveLength(1);
    expect(s().tabs.map((t) => t.id)).toEqual(['b', 'a']);
  });
});
