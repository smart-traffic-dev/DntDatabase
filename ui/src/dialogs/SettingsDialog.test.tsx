import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useAppSettings } from '../store/appSettings';
import { useLayout } from '../store/layout';
import { useTabs } from '../store/tabs';
import { ThemeProvider } from '../theme/ThemeProvider';
import { callsOf, mockBackend, resetStores } from '../test/backend';
import { SettingsDialog } from './SettingsDialog';

describe('SettingsDialog', () => {
  beforeEach(() => {
    resetStores();
    mockBackend({
      get_log_level: () => 'Info',
      get_table_page_size: () => 100,
      get_confirm_unbounded_writes: () => true,
      get_idle_tx_warn_minutes: () => 5,
      set_idle_tx_warn_minutes: () => null,
      get_jobs_per_connection: () => 2,
      get_external_tools_dir: () => null,
    });
  });

  it('settings_idle_tx_minutes_bounds', async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <SettingsDialog onClose={() => {}} initialTab="queryData" />
      </ThemeProvider>,
    );
    const field = await screen.findByLabelText('Warn when a transaction stays open for (minutes)');
    await screen.findByDisplayValue('5');
    for (const bad of ['0', '121', '2.5']) {
      await user.clear(field);
      await user.type(field, bad);
      await user.tab();
      expect(await screen.findByRole('alert')).toHaveTextContent('Enter a whole number from 1 to 120.');
    }
    expect(callsOf('set_idle_tx_warn_minutes')).toEqual([]);

    await user.clear(field);
    await user.type(field, '30');
    await user.tab();
    await screen.findByText(/Saved\./);
    expect(callsOf('set_idle_tx_warn_minutes')).toEqual([{ minutes: 30 }]);
    expect(useAppSettings.getState().idleTxWarnMinutes).toBe(30);
  });

  it('arrow keys move between tabs; Layout and Appearance controls act at once', async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <SettingsDialog onClose={() => {}} />
      </ThemeProvider>,
    );
    const appearance = screen.getByRole('tab', { name: 'Appearance' });
    expect(appearance).toHaveFocus();
    expect(screen.getByRole('radiogroup', { name: 'Theme' })).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Bigger' }));
    expect(useLayout.getState().fontScale).toBeCloseTo(1.1);
    await user.click(screen.getByRole('button', { name: 'Reset' }));
    expect(useLayout.getState().fontScale).toBe(1);

    appearance.focus();
    await user.keyboard('{ArrowDown}');
    const layoutTab = screen.getByRole('tab', { name: 'Layout' });
    expect(layoutTab).toHaveFocus();
    expect(layoutTab).toHaveAttribute('aria-selected', 'true');
    await user.click(screen.getByRole('radio', { name: 'Two rows' }));
    expect(useTabs.getState().layout).toBe('rows');
    await user.click(screen.getByRole('checkbox', { name: /Show sidebar/ }));
    expect(useLayout.getState().sidebarVisible).toBe(false);

    layoutTab.focus();
    await user.keyboard('{End}');
    expect(screen.getByRole('tab', { name: 'Keyboard shortcuts' })).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{Home}');
    expect(appearance).toHaveAttribute('aria-selected', 'true');
  });

  it('Density radio group switches to Comfortable', async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <SettingsDialog onClose={() => {}} />
      </ThemeProvider>,
    );
    const group = screen.getByRole('radiogroup', { name: 'Density' });
    expect(within(group).getByRole('radio', { name: 'Compact' })).toHaveAttribute('aria-checked', 'true');
    await user.click(within(group).getByRole('radio', { name: 'Comfortable' }));
    expect(within(group).getByRole('radio', { name: 'Comfortable' })).toHaveAttribute('aria-checked', 'true');
    expect(document.documentElement).toHaveAttribute('data-density', 'comfortable');
    document.documentElement.removeAttribute('data-density');
    localStorage.removeItem('density');
  });
});
