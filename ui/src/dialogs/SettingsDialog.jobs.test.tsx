import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useAppSettings } from '../store/appSettings';
import { ThemeProvider } from '../theme/ThemeProvider';
import { callsOf, fail, mockBackend, resetStores } from '../test/backend';
import { SettingsDialog } from './SettingsDialog';

function renderSettings() {
  return render(
    <ThemeProvider>
      <SettingsDialog onClose={() => {}} initialTab="queryData" />
    </ThemeProvider>,
  );
}

function backend(overrides: Parameters<typeof mockBackend>[0] = {}) {
  mockBackend({
    get_log_level: () => 'Info',
    get_table_page_size: () => 100,
    get_confirm_unbounded_writes: () => true,
    get_idle_tx_warn_minutes: () => 5,
    get_jobs_per_connection: () => 2,
    set_jobs_per_connection: () => null,
    get_external_tools_dir: () => null,
    tools_pick_dir: () => 'D:\\tools',
    clear_external_tools_dir: () => null,
    ...overrides,
  });
}

beforeEach(() => {
  resetStores();
  backend();
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

  it('External tools folder is chosen in the backend dialog only, and Clear means PATH only', async () => {
    const user = userEvent.setup();
    renderSettings();
    const field = await screen.findByLabelText('External tools folder');
    // The UI never names the folder: the field cannot be typed into.
    expect(field).toHaveAttribute('readonly');
    expect(screen.getByRole('button', { name: 'Clear' })).toBeDisabled();

    await user.click(screen.getByRole('button', { name: 'Browse…' }));
    expect(await screen.findByDisplayValue('D:\\tools')).toBeInTheDocument();
    expect(callsOf('tools_pick_dir')).toHaveLength(1);
    expect(screen.getByText(/Saved\./)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Clear' }));
    await vi.waitFor(() => expect(callsOf('clear_external_tools_dir')).toHaveLength(1));
    expect(useAppSettings.getState().externalToolsDir).toBeNull();
    expect(field).toHaveValue('');
  });

  it('External tools folder keeps the old folder when the dialog is cancelled', async () => {
    const user = userEvent.setup();
    backend({ get_external_tools_dir: () => 'C:\\pg\\bin', tools_pick_dir: () => null });
    renderSettings();
    await screen.findByDisplayValue('C:\\pg\\bin');
    await user.click(screen.getByRole('button', { name: 'Browse…' }));
    await vi.waitFor(() => expect(callsOf('tools_pick_dir')).toHaveLength(1));
    expect(screen.getByDisplayValue('C:\\pg\\bin')).toBeInTheDocument();
    expect(screen.queryByText(/Saved\./)).toBeNull();
  });

  it('External tools folder shows the backend refusal', async () => {
    const user = userEvent.setup();
    backend({
      tools_pick_dir: () => {
        throw fail('field_name', { field: 'dir', message: 'The tools folder does not exist.' });
      },
    });
    renderSettings();
    await screen.findByLabelText('External tools folder');
    await user.click(screen.getByRole('button', { name: 'Browse…' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('The tools folder does not exist.');
  });
});
