import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { resetStores } from '../test/backend';
import { Toolbar } from './Toolbar';

beforeEach(resetStores);

describe('Toolbar', () => {
  it('leaves the app name to the title bar', () => {
    render(<Toolbar />);
    expect(screen.queryByText('DntDatabase')).not.toBeInTheDocument();
  });

  it('the connection menu opens Settings', async () => {
    const user = userEvent.setup();
    render(<Toolbar />);
    await user.click(screen.getByRole('button', { name: 'Connection menu' }));
    await user.click(screen.getByRole('menuitem', { name: 'Settings…' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'settings' });
  });
});
