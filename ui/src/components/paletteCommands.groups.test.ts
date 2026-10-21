import { beforeEach, describe, expect, it } from 'vitest';
import { useTabs, type QueryTabRef } from '../store/tabs';
import { paletteCommands } from './paletteCommands';

const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });
const offered = () => paletteCommands().map((c) => c.id);

beforeEach(() => useTabs.getState().reset());

describe('tab commands of the palette with editor groups', () => {
  it('are offered only when they would close something in the focused group', () => {
    for (const id of ['a', 'b', 'c']) useTabs.getState().open(q(id));
    useTabs.getState().splitTab('c', 'right');
    // Focused group: [c] alone, the other group holds a and b.
    expect(offered()).not.toContain('tab.closeOthers');
    expect(offered()).not.toContain('tab.closeRight');

    useTabs.getState().activate('a');
    expect(offered()).toContain('tab.closeOthers');
    expect(offered()).toContain('tab.closeRight');
    useTabs.getState().activate('b');
    expect(offered()).not.toContain('tab.closeRight');
  });
});
