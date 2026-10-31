import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { resetStores } from '../test/backend';
import { Sidebar } from './Sidebar';

beforeEach(() => {
  resetStores();
  localStorage.removeItem('dnt.sidebar.view');
});

describe('Sidebar', () => {
  it('the Objects filter row + is New connection (testid new-connection)', async () => {
    render(<Sidebar />);
    const plus = screen.getByTestId('new-connection');
    expect(plus).toHaveAccessibleName('New connection');
    await userEvent.setup().click(plus);
    expect(useDialogs.getState().request).toEqual({ kind: 'newConnection' });
  });

  it('the header holds only the view tabs; the Objects actions leave with the Objects view', async () => {
    const user = userEvent.setup();
    const { container } = render(<Sidebar />);
    const bar = container.querySelector('.sidebar-bar') as HTMLElement;
    expect(within(bar).queryAllByRole('button')).toHaveLength(0);
    expect(screen.getByRole('button', { name: 'Objects options' })).toBeInTheDocument();

    await user.click(screen.getByRole('tab', { name: 'Favorites' }));
    expect(screen.queryByTestId('new-connection')).toBeNull();
    expect(screen.queryByRole('button', { name: 'Objects options' })).toBeNull();
  });

  it('switches between Objects and Favorites and remembers the view', async () => {
    const user = userEvent.setup();
    const { unmount } = render(<Sidebar />);
    const views = screen.getByRole('tablist', { name: 'Sidebar view' });
    const objects = within(views).getByRole('tab', { name: 'Objects' });
    const favorites = within(views).getByRole('tab', { name: 'Favorites' });
    expect(objects).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('searchbox', { name: 'Filter objects' })).toBeInTheDocument();
    expect(screen.getByTestId('favorites-panel')).not.toBeVisible();

    await user.click(favorites);
    expect(favorites).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('favorites-panel')).toBeVisible();
    expect(screen.queryByRole('searchbox', { name: 'Filter objects' })).toBeNull();

    unmount();
    render(<Sidebar />);
    expect(screen.getByRole('tab', { name: 'Favorites' })).toHaveAttribute('aria-selected', 'true');
  });

  it('arrow keys, Home and End move between the two views', async () => {
    const user = userEvent.setup();
    render(<Sidebar />);
    screen.getByRole('tab', { name: 'Objects' }).focus();
    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('tab', { name: 'Favorites' })).toHaveFocus();
    expect(screen.getByRole('tab', { name: 'Favorites' })).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{ArrowLeft}');
    expect(screen.getByRole('tab', { name: 'Objects' })).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{End}');
    expect(screen.getByRole('tab', { name: 'Favorites' })).toHaveFocus();
    await user.keyboard('{Home}');
    expect(screen.getByRole('tab', { name: 'Objects' })).toHaveFocus();
    expect(screen.getByRole('tab', { name: 'Objects' })).toHaveAttribute('aria-selected', 'true');
  });

  it('the ⋯ menu toggles system schemas, refreshes and collapses all', async () => {
    const user = userEvent.setup();
    const reload = vi.spyOn(useTree.getState(), 'reloadStored').mockResolvedValue();
    act(() => useTree.setState({ expanded: new Set(['a', 'b']) }));
    render(<Sidebar />);
    const more = () => screen.getByRole('button', { name: 'Objects options' });

    await user.click(more());
    const system = screen.getByRole('menuitemcheckbox', { name: /Show system schemas/ });
    expect(system).toHaveAttribute('aria-checked', 'false');
    await user.click(system);
    expect(useTree.getState().showSystem).toBe(true);

    await user.click(more());
    await user.click(screen.getByRole('menuitem', { name: /Collapse all/ }));
    expect(useTree.getState().expanded.size).toBe(0);

    await user.click(more());
    await user.click(screen.getByRole('menuitem', { name: /Refresh all/ }));
    expect(reload).toHaveBeenCalledOnce();
  });
});
