import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useAppSettings } from '../store/appSettings';
import { ThemeProvider } from '../theme/ThemeProvider';
import { callsOf, fail, mockBackend, resetStores } from '../test/backend';
import { SettingsDialog } from './SettingsDialog';

const picker = vi.hoisted(() => ({ open: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => picker);

function renderSettings() {
  return render(
    <ThemeProvider>
      <SettingsDialog onClose={() => {}} initialTab="queryData" />
    </ThemeProvider>,
  );
}

beforeEach(() => {
  resetStores();
  picker.open.mockReset();
  mockBackend({
    get_log_level: () => 'Info',
    get_table_page_size: () => 100,
    get_confirm_unbounded_writes: () => true,
    get_idle_tx_warn_minutes: () => 5,
    get_jobs_per_connection: () => 2,
    set_jobs_per_connection: () => null,
    get_external_tools_dir: () => null,
    set_external_tools_dir: (a) => {
      if (a.dir === 'relative') throw fail('field_name', { field: 'dir', message: 'The folder must be an absolute path that exists.' });
      return null;
    },
  });
});

describe('Settings: background jobs and external tools', () => {
  it('Background jobs per connection accepts 1 to 8 and saves', async () => {
    const user = userEvent.setup();
    renderSettings();
    const field = await screen.findByLabelText('Background jobs per connection');
    await screen.findByDisplayValue('2');
    for (const bad of ['0', '9', '1.5', '']) {
      await user.clear(field);
      if (bad) await user.type(field, bad);
      await user.tab();
      expect(await screen.findByRole('alert')).toHaveTextContent('Enter a whole number from 1 to 8.');
    }
    expect(callsOf('set_jobs_per_connection')).toEqual([]);
    await user.clear(field);
    await user.type(field, '4');
    await user.tab();
    expect(await screen.findByText(/Saved\./)).toBeInTheDocument();
    expect(callsOf('set_jobs_per_connection')).toEqual([{ n: 4 }]);
    expect(useAppSettings.getState().jobsPerConnection).toBe(4);
  });

  it('External tools folder saves a typed path, a browsed one, and empty as PATH only', async () => {
    const user = userEvent.setup();
    renderSettings();
    const field = await screen.findByLabelText('External tools folder');
    await user.type(field, 'C:\\pg\\bin');
    await user.tab();
    await screen.findByText(/Saved\./);
    expect(callsOf('set_external_tools_dir')).toEqual([{ dir: 'C:\\pg\\bin' }]);

    picker.open.mockResolvedValueOnce('D:\\tools');
    await user.click(screen.getByRole('button', { name: 'Browse…' }));
    expect(picker.open).toHaveBeenCalledWith(expect.objectContaining({ directory: true }));
    expect(await screen.findByDisplayValue('D:\\tools')).toBeInTheDocument();
    expect(callsOf('set_external_tools_dir').at(-1)).toEqual({ dir: 'D:\\tools' });

    await user.clear(field);
    await user.tab();
    await vi.waitFor(() => expect(callsOf('set_external_tools_dir').at(-1)).toEqual({ dir: null }));
    expect(useAppSettings.getState().externalToolsDir).toBeNull();
  });

  it('External tools folder shows the backend refusal', async () => {
    const user = userEvent.setup();
    renderSettings();
    const field = await screen.findByLabelText('External tools folder');
    await user.type(field, 'relative');
    await user.tab();
    expect(await screen.findByRole('alert')).toHaveTextContent('The folder must be an absolute path that exists.');
  });
});
