import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { AiConfig, AiConnectionChoice, AiSettings } from '../bindings';
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

function view(config: AiConfig, hasKey = false, consentOrigin: string | null = null): AiSettings {
  const url = config.base_url ? new URL(config.base_url) : null;
  return {
    config,
    has_key: hasKey,
    origin: url ? url.origin : null,
    host: url ? url.host : null,
    consented: !!url && url.origin === consentOrigin,
  };
}

interface World {
  settings: AiSettings;
  consentOrigin: string | null;
  choices: AiConnectionChoice[];
}

function backend(world: World, over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  mockBackend({
    ai_settings_get: () => world.settings,
    ai_connections: () => world.choices,
    ai_settings_set: ({ config }) => {
      const c = config as AiConfig;
      if (c.base_url && !/^https?:\/\//.test(c.base_url)) {
        throw new Failure(appError('config', 'The base URL must start with http:// or https://.'));
      }
      world.settings = view(c, world.settings.has_key, world.consentOrigin);
      return world.settings;
    },
    ai_consent_set: ({ granted }) => {
      world.consentOrigin = granted ? world.settings.origin : null;
      world.settings = view(world.settings.config, world.settings.has_key, world.consentOrigin);
      return world.settings;
    },
    ai_connection_set: ({ connId, enabled }) => {
      world.choices = [
        ...world.choices.filter((c) => c.connection_id !== connId),
        { connection_id: connId as string, enabled: enabled as boolean },
      ];
      return world.choices;
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

const production = (s: ReturnType<typeof config>) => ({ ...s, config: { ...s.config, env_tag: 'production' as const } });

beforeEach(() => {
  resetStores();
  useConnections
    .getState()
    .setStored([], [production(config(ids.prod, 'Shop')), config(ids.analytics, 'Analytics')]);
  world = { settings: view(DEFAULTS), consentOrigin: null, choices: [] };
  backend(world);
});

describe('Settings → AI', () => {
  it("shows the endpoint's own error", async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1' });
    backend(world, {
      ai_models: () => {
        throw new Failure(appError('ai', 'The endpoint rejected the request (404): Not Found'));
      },
    });
    const user = userEvent.setup();
    renderAi();
    await user.click(await screen.findByRole('button', { name: 'Load models' }));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('The endpoint rejected the request (404): Not Found');
    expect(alert).not.toHaveTextContent('database');
  });

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
    expect(screen.queryByRole('button', { name: 'Turn on AI…' })).not.toBeInTheDocument();
  });

  it('turns AI on once for every connection, then off for one, then off', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'coder' });
    const user = userEvent.setup();
    renderAi();
    expect(await screen.findByTestId('ai-consent-off')).toHaveTextContent('nothing is sent to api.example.com');
    await user.click(screen.getByRole('button', { name: 'Turn on AI…' }));

    const dialog = await screen.findByTestId('ai-consent-dialog');
    expect(dialog).toHaveTextContent('api.example.com');
    expect(dialog).toHaveTextContent('Row data is never sent.');
    const turnOn = within(dialog).getByRole('button', { name: 'Turn on AI' });
    expect(turnOn).toBeDisabled();
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toHaveFocus();
    await user.click(
      within(dialog).getByRole('checkbox', {
        name: "I am allowed to share these databases' schemas and queries with this endpoint",
      }),
    );
    expect(turnOn).toBeEnabled();
    await user.click(turnOn);
    await waitFor(() => expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument());
    expect(callsOf('ai_consent_set')).toEqual([{ granted: true }]);
    expect(await screen.findByTestId('ai-consent-on')).toBeInTheDocument();

    // On by default, except the Production one.
    const list = screen.getByRole('list', { name: 'Connections with AI' });
    expect(within(list).getByRole('checkbox', { name: 'Analytics' })).toBeChecked();
    expect(within(list).getByRole('checkbox', { name: 'Shop' })).not.toBeChecked();
    expect(within(list).getByText('Production')).toBeInTheDocument();

    await user.click(within(list).getByRole('checkbox', { name: 'Analytics' }));
    await waitFor(() => expect(within(list).getByRole('checkbox', { name: 'Analytics' })).not.toBeChecked());
    expect(callsOf('ai_connection_set')).toEqual([{ connId: ids.analytics, enabled: false }]);

    await user.click(screen.getByRole('button', { name: 'Turn off AI' }));
    expect(await screen.findByTestId('ai-consent-off')).toBeInTheDocument();
    expect(callsOf('ai_consent_set').at(-1)).toEqual({ granted: false });
  });

  it('Enter in the consent dialog cancels', async () => {
    world.settings = view({ ...DEFAULTS, base_url: 'https://api.example.com/v1', model: 'coder' });
    const user = userEvent.setup();
    renderAi();
    await user.click(await screen.findByRole('button', { name: 'Turn on AI…' }));
    const dialog = await screen.findByTestId('ai-consent-dialog');
    const box = within(dialog).getByRole('checkbox');
    await user.click(box);
    box.focus();
    await user.keyboard('{Enter}');
    await waitFor(() => expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument());
    expect(callsOf('ai_consent_set')).toEqual([]);
  });

  it('saving a new endpoint asks for consent at once', async () => {
    const user = userEvent.setup();
    renderAi();
    await user.type(await screen.findByLabelText('Base URL'), 'https://api.example.com/v1');
    await user.type(screen.getByLabelText('Model'), 'coder');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(await screen.findByTestId('ai-consent-dialog')).toHaveTextContent('api.example.com');
  });

  it('consent to another endpoint does not count', async () => {
    world.consentOrigin = 'https://one.example';
    world.settings = view({ ...DEFAULTS, base_url: 'https://two.example/v1', model: 'coder' }, false, world.consentOrigin);
    renderAi();
    expect(await screen.findByTestId('ai-consent-off')).toHaveTextContent('nothing is sent to two.example');
  });
});
