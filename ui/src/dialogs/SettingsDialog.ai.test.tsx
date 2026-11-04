import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { AiConfig, AiConnectionView, AiSettings } from '../bindings';
import { useConnections } from '../store/connections';
import { ThemeProvider } from '../theme/ThemeProvider';
import { appError, callsOf, config, Failure, ids, mockBackend, resetStores } from '../test/backend';
import { SettingsDialog } from './SettingsDialog';

const KEY = 'sk-test-secret-123';

const DEFAULTS: AiConfig = {
  base_url: null,
  model: null,
  trigger: 'automatic',
  max_tokens: 128,
  timeout_ms: 5000,
  max_lines: 8,
};

function view(config: AiConfig, hasKey = false): AiSettings {
  const url = config.base_url ? new URL(config.base_url) : null;
  return { config, has_key: hasKey, origin: url ? url.origin : null, host: url ? url.host : null };
}

interface World {
  settings: AiSettings;
  optIns: AiConnectionView[];
}

function backend(world: World, over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  mockBackend({
    ai_settings_get: () => world.settings,
    ai_opt_in_list: () => world.optIns,
    ai_settings_set: ({ config }) => {
      const c = config as AiConfig;
      if (c.base_url && !/^https?:\/\//.test(c.base_url)) {
        throw new Failure(appError('config', 'The base URL must start with http:// or https://.'));
      }
      world.settings = view(c, world.settings.has_key);
      return world.settings;
    },
    ai_key_set: () => {
      world.settings = { ...world.settings, has_key: true };
      return world.settings;
    },
    ai_key_clear: () => {
      world.settings = { ...world.settings, has_key: false };
      return world.settings;
    },
    ai_models: () => ['coder-a', 'coder-b'],
    ai_test: () => ({ verdict: 'ok', elapsed_ms: 42, sample: 'active = 1' }),
    ai_opt_in_set: ({ connId, enabled }) => {
      world.optIns = enabled
        ? [{ connection_id: connId as string, name: 'Production', granted_at: 'now', active: true }]
        : [];
      return world.optIns;
    },
    ...over,
  });
}

function renderAi() {
  return render(
    <ThemeProvider>
      <SettingsDialog onClose={() => {}} initialTab="ai" />
    </ThemeProvider>,
  );
}

let world: World;

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'Production'), config(ids.analytics, 'Analytics')]);
  world = { settings: view(DEFAULTS), optIns: [] };
  backend(world);
});

describe('Settings → AI', () => {
  it('Test saves unsaved changes first, so it tests what is on screen', async () => {
    const user = userEvent.setup();
    renderAi();
    await user.type(await screen.findByLabelText('Base URL'), 'https://api.deepseek.com/beta');
    await user.type(screen.getByLabelText('Model'), 'deepseek-flash');
    await user.click(screen.getByRole('button', { name: 'Test' }));
    expect(await screen.findByText(/The endpoint works/)).toBeInTheDocument();
    expect(callsOf('ai_settings_set')).toEqual([
      { config: { ...DEFAULTS, base_url: 'https://api.deepseek.com/beta', model: 'deepseek-flash' } },
    ]);
    expect(callsOf('ai_test')).toHaveLength(1);
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
  });

  it('saves the endpoint and shows validation errors', async () => {
    const user = userEvent.setup();
    renderAi();
    const url = await screen.findByLabelText('Base URL');
    const save = screen.getByRole('button', { name: 'Save' });
    expect(save).toBeDisabled();

    await user.type(url, 'api.example.com');
    await user.click(save);
    expect(await screen.findByRole('alert')).toHaveTextContent('The base URL must start with http:// or https://.');

    await user.clear(url);
    await user.type(url, 'https://api.example.com/v1');
    await user.type(screen.getByLabelText('Model'), 'coder');
    await user.click(screen.getByRole('radio', { name: 'Manual (Alt+\\)' }));
    await user.click(save);
    expect(await screen.findByText('Saved.')).toBeInTheDocument();
    expect(callsOf('ai_settings_set').at(-1)).toEqual({
      config: { ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'coder', trigger: 'manual' },
    });
    expect(save).toBeDisabled();
  });

  it('sends the key once and never shows it again', async () => {
    const user = userEvent.setup();
    renderAi();
    const key = await screen.findByLabelText('API key');
    expect(key).toHaveAttribute('type', 'password');
    expect(key).toHaveAttribute('placeholder', 'No key');
    await user.type(key, KEY);
    await user.click(screen.getByRole('button', { name: 'Save key' }));
    expect(await screen.findByText('The API key is saved.')).toBeInTheDocument();
    expect(callsOf('ai_key_set')).toEqual([{ key: KEY }]);
    expect(key).toHaveValue('');
    expect(key).toHaveAttribute('placeholder', 'A key is saved');
    expect(document.body.textContent).not.toContain(KEY);

    await user.click(screen.getByRole('button', { name: 'Clear' }));
    expect(await screen.findByText('The API key is removed.')).toBeInTheDocument();
    expect(key).toHaveAttribute('placeholder', 'No key');
  });

  it('loads models and tests the endpoint', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'coder' });
    const user = userEvent.setup();
    renderAi();
    await user.click(await screen.findByRole('button', { name: 'Load models' }));
    await waitFor(() => expect(document.querySelectorAll('#settings-ai-models option')).toHaveLength(2));

    await user.click(screen.getByRole('button', { name: 'Test' }));
    expect(await screen.findByText(/The endpoint works \(42 ms\)\. Suggested: active = 1/)).toBeInTheDocument();
  });

  it('warns when the model ignores the suffix', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'chat' });
    backend(world, { ai_test: () => ({ verdict: 'ignores_suffix', elapsed_ms: 10, sample: null }) });
    const user = userEvent.setup();
    renderAi();
    await user.click(await screen.findByRole('button', { name: 'Test' }));
    expect(
      await screen.findByText('The model may be ignoring the suffix. Use a code model with fill-in-the-middle support.'),
    ).toBeInTheDocument();
  });

  it('needs a saved base URL before AI can be turned on', async () => {
    renderAi();
    expect(await screen.findByText('Save a base URL first.')).toBeInTheDocument();
    expect(screen.getByLabelText('Connection to turn AI on for')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Turn on…' })).toBeDisabled();
  });

  it('turns AI on only after consent, and off again', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'coder' });
    const user = userEvent.setup();
    renderAi();
    expect(await screen.findByText('AI is off for every connection.')).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText('Connection to turn AI on for'), ids.prod);
    await user.click(screen.getByRole('button', { name: 'Turn on…' }));

    const dialog = await screen.findByTestId('ai-consent-dialog');
    expect(dialog).toHaveTextContent('api.example.com');
    expect(dialog).toHaveTextContent('Row data is never sent.');
    const turnOn = within(dialog).getByRole('button', { name: 'Turn on AI' });
    expect(turnOn).toBeDisabled();
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toHaveFocus();

    await user.click(
      within(dialog).getByRole('checkbox', {
        name: "I am allowed to share this database's schema and queries with this endpoint",
      }),
    );
    expect(turnOn).toBeEnabled();
    await user.click(turnOn);
    await waitFor(() => expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument());
    expect(callsOf('ai_opt_in_set')).toEqual([{ connId: ids.prod, enabled: true }]);
    expect(await screen.findByText('Production')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Turn off' }));
    expect(await screen.findByText('AI is off for every connection.')).toBeInTheDocument();
    expect(callsOf('ai_opt_in_set').at(-1)).toEqual({ connId: ids.prod, enabled: false });
  });

  it('Enter in the consent dialog cancels', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'coder' });
    const user = userEvent.setup();
    renderAi();
    await user.selectOptions(await screen.findByLabelText('Connection to turn AI on for'), ids.prod);
    await user.click(screen.getByRole('button', { name: 'Turn on…' }));
    const dialog = await screen.findByTestId('ai-consent-dialog');
    const box = within(dialog).getByRole('checkbox');
    await user.click(box);
    box.focus();
    await user.keyboard('{Enter}');
    await waitFor(() => expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument());
    expect(callsOf('ai_opt_in_set')).toEqual([]);
  });

  it('marks consent given to another endpoint', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://two.example/v1', model: 'coder' });
    world.optIns = [{ connection_id: ids.prod, name: 'Production', granted_at: 'now', active: false }];
    renderAi();
    expect(await screen.findByText(/Consent needed again for the new endpoint/)).toBeInTheDocument();
  });
});
