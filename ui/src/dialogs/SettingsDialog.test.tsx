import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useAppSettings } from '../store/appSettings';
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
    });
  });

  it('settings_idle_tx_minutes_bounds', async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <SettingsDialog onClose={() => {}} />
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
