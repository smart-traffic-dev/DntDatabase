import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from './dialogs';
import { MAX_TOASTS, useToasts } from './toasts';

const s = () => useToasts.getState();

beforeEach(() => {
  s().reset();
  useDialogs.getState().dismissInternal();
});

describe('toast room next to the internal-error toast', () => {
  it('the internal error takes a slot: the oldest toast goes instead of waiting hidden with no timer', () => {
    const gone = vi.fn();
    s().show({ kind: 'info', title: 'one', onDismiss: gone });
    s().show({ kind: 'info', title: 'two' });
    s().show({ kind: 'info', title: 'three' });
    useDialogs.getState().reportInternal({ kind: 'internal', message: 'boom' } as never);
    expect(s().toasts.map((t) => t.title)).toEqual(['two', 'three']);
    expect(gone).toHaveBeenCalledOnce();
    // While it shows, new toasts keep within the room left.
    s().show({ kind: 'info', title: 'four' });
    expect(s().toasts).toHaveLength(MAX_TOASTS - 1);
    // Dismissed: the full room is back.
    useDialogs.getState().dismissInternal();
    s().show({ kind: 'info', title: 'five' });
    expect(s().toasts).toHaveLength(MAX_TOASTS);
  });
});
