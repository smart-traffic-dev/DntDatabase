import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { groupTabs, useTabs, type QueryTabRef } from '../store/tabs';
import { resetStores } from '../test/backend';
import { TabBar } from './TabBar';

const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });
const s = () => useTabs.getState();
const groupIds = () => s().groups.map((g) => g.id);
const titles = (groupId: string) => groupTabs(s(), groupId).map((t) => t.title);

beforeEach(resetStores);

function twoGroups() {
  act(() => {
    for (const id of ['a', 'b', 'c']) s().open({ ...q(id), kind: 'object', connectionId: 'c1', objectRef: { database: 'd', schema: null, name: id, kind: 'Table' } });
    s().splitTab('c', 'right');
  });
  return groupIds() as [string, string];
}

describe('TabBar of an editor group', () => {
  it('shows only the tabs of its group; an unfocused group dims its strip', () => {
    const [g1, g2] = twoGroups();
    const { container } = render(
      <>
        <TabBar groupId={g1} />
        <TabBar groupId={g2} />
      </>,
    );
    const [strip1, strip2] = [...container.querySelectorAll('.tabstrip')] as HTMLElement[];
    expect(within(strip1!).getAllByRole('tab').map((t) => t.textContent)).toEqual([expect.stringContaining('a'), expect.stringContaining('b')]);
    expect(within(strip2!).getAllByRole('tab')).toHaveLength(1);
    expect(strip1).toHaveClass('unfocused');
    expect(strip2).not.toHaveClass('unfocused');
  });

  it('the tab menu splits right / down and moves to another group', async () => {
    const [g1, g2] = twoGroups();
    const user = userEvent.setup();
    render(<TabBar groupId={g1} />);
    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /a/ }) });
    await user.click(screen.getByRole('menuitem', { name: 'Move to group 2' }));
    expect(titles(g2)).toEqual(['c', 'a']);

    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /b/ }) });
    expect(screen.getByRole('menuitem', { name: /^Split right/ })).toBeInTheDocument();
    await user.click(screen.getByRole('menuitem', { name: /^Split down/ }));
    // b was alone in group 1: an empty group opened beside it.
    expect(s().groups).toHaveLength(3);
  });

  it('Close others stays inside the group', async () => {
    const [g1, g2] = twoGroups();
    const user = userEvent.setup();
    render(<TabBar groupId={g1} />);
    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /b/ }) });
    await user.click(screen.getByRole('menuitem', { name: 'Close others' }));
    await waitFor(() => expect(titles(g1)).toEqual(['b']));
    expect(titles(g2)).toEqual(['c']);
  });

  it('dropping a tab on another group strip moves it there', () => {
    const [g1, g2] = twoGroups();
    const { container } = render(
      <>
        <TabBar groupId={g1} />
        <TabBar groupId={g2} />
      </>,
    );
    const data = new Map<string, string>();
    const dataTransfer = { setData: (k: string, v: string) => data.set(k, v), getData: (k: string) => data.get(k) ?? '', types: ['text/x-dnt-tab'], effectAllowed: '', dropEffect: '' };
    const source = screen.getAllByRole('tab', { name: /a/ })[0]!.closest('.tab')!;
    fireEvent.dragStart(source, { dataTransfer });
    const strip2 = container.querySelectorAll('.tabstrip')[1]!;
    fireEvent.dragOver(strip2, { dataTransfer });
    fireEvent.drop(strip2, { dataTransfer });
    expect(titles(g2)).toEqual(['c', 'a']);
    expect(s().focusedGroupId).toBe(g2);
  });

  it('clicking a strip focuses its group', async () => {
    const [g1] = twoGroups();
    render(<TabBar groupId={g1} />);
    await userEvent.setup().click(screen.getByRole('tab', { name: /b/ }));
    expect(s().focusedGroupId).toBe(g1);
    expect(s().activeId).toContain('b');
  });
});
