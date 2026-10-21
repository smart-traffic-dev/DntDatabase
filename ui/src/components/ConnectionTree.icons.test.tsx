import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import type { ObjectKind } from '../bindings';
import { NodeIcon } from './ConnectionTree';

const markup = (folderKind: ObjectKind) =>
  render(<NodeIcon node={{ id: `f:${folderKind}`, kind: 'folder', label: folderKind, folderKind }} />).container.innerHTML;

describe('tree icons', () => {
  it('each folder kind has its own icon', () => {
    const kinds: ObjectKind[] = ['Table', 'View', 'Function', 'Procedure', 'Trigger', 'Event'];
    const icons = kinds.map(markup);
    expect(new Set(icons).size).toBe(kinds.length);
  });

  it('an object uses the icon of its folder', () => {
    const object = (kind: ObjectKind) =>
      render(
        <NodeIcon node={{ id: `o:${kind}`, kind: 'object', label: 'x', objectRef: { database: 'd', schema: null, name: 'x', kind } }} />,
      ).container.innerHTML;
    for (const kind of ['Table', 'Function', 'Trigger'] as const) expect(object(kind)).toBe(markup(kind));
  });
});
