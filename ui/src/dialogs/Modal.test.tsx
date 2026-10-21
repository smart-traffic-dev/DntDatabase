import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { Modal } from './Modal';

const sheet = import.meta.glob('./dialogs.css', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;
const css = Object.values(sheet)[0] ?? '';
const rule = (selector: string) => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const plain = css.replace(/\/\*[\s\S]*?\*\//g, '');
  return new RegExp(`(^|\\})\\s*${escaped}\\s*\\{([^}]*)\\}`).exec(plain)?.[2] ?? '';
};

describe('Modal frame (spec §6)', () => {
  it('has a header with the title and a close button, and the body gets the first focus', async () => {
    const onClose = vi.fn();
    render(
      <Modal title="Rename group" onClose={onClose}>
        <div className="modal-body">
          <input aria-label="Group name" />
        </div>
      </Modal>,
    );
    const dialog = screen.getByRole('dialog', { name: 'Rename group' });
    const close = screen.getByRole('button', { name: 'Close dialog' });
    expect(dialog.querySelector('.modal-head')).toContainElement(close);
    // Mouse only: Esc is the keyboard way out, Tab cycles the dialog's own controls.
    expect(close).toHaveAttribute('tabindex', '-1');
    // The × is not where the keyboard starts: the first field is.
    expect(screen.getByLabelText('Group name')).toHaveFocus();
    await userEvent.setup().click(close);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('closable={false} leaves the × out', () => {
    render(
      <Modal title="Working" onClose={vi.fn()} closable={false}>
        <p>busy</p>
      </Modal>,
    );
    expect(screen.queryByRole('button', { name: 'Close dialog' })).toBeNull();
  });

  it('the badge sits before the title', () => {
    render(
      <Modal title="Confirm run" badge={<span data-testid="badge">PRODUCTION</span>} onClose={vi.fn()}>
        <p>x</p>
      </Modal>,
    );
    const head = screen.getByRole('heading', { name: 'Confirm run' }).parentElement!;
    expect(head.firstElementChild).toBe(screen.getByTestId('badge'));
  });

  it('Enter outside a button triggers the safe default of a destructive dialog', async () => {
    const cancel = vi.fn();
    const run = vi.fn();
    render(
      <Modal title="Drop table" role="alertdialog" onClose={cancel} initialFocus="input">
        <div className="modal-body">
          <input aria-label="Type the name" />
        </div>
        <footer className="modal-foot">
          <button type="button" data-cancel onClick={cancel}>
            Cancel
          </button>
          <button type="button" onClick={run}>
            Drop
          </button>
        </footer>
      </Modal>,
    );
    const user = userEvent.setup();
    // Enter in a text box does nothing: no run, and no silent cancel of what was just typed.
    await user.type(screen.getByLabelText('Type the name'), 'orders{Enter}');
    expect(cancel).not.toHaveBeenCalled();
    expect(run).not.toHaveBeenCalled();
    // Enter elsewhere in the dialog is the safe default.
    screen.getByRole('alertdialog').focus();
    await user.keyboard('{Enter}');
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(run).not.toHaveBeenCalled();
    // Enter on a focused button still presses that button.
    screen.getByRole('button', { name: 'Drop' }).focus();
    await user.keyboard('{Enter}');
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('uses the large radius, shadow-3 and the scrim token', () => {
    expect(rule('.modal')).toMatch(/border-radius:\s*var\(--dnt-radius-lg\)/);
    expect(rule('.modal')).toMatch(/box-shadow:\s*var\(--dnt-shadow-3\)/);
    expect(rule('.modal')).toMatch(/background:\s*var\(--dnt-bg-raised\)/);
    expect(rule('.modal-backdrop')).toMatch(/background:\s*var\(--dnt-scrim\)/);
    // Primary action on the right: the footer packs its buttons to the end.
    expect(rule('.modal-foot')).toMatch(/justify-content:\s*flex-end/);
  });
});
