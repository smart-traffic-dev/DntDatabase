import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { AiConfig, AiConnectionChoice, AiSettings } from '../bindings';
import { AI_ERROR_AFTER, useAi } from '../store/ai';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { callsOf, config, ids, mockBackend, resetStores } from '../test/backend';
import { AiStatus } from './AiStatus';

const SET_UP: AiConfig = {
  base_url: 'https://api.example.com/v1',
  model: 'coder',
  trigger: 'automatic',
  max_tokens: 128,
  timeout_ms: 5000,
  max_lines: 8,
};

/** `mongo`: the consent names MongoDB (false: one given before the consent text did). */
function settings(config: AiConfig, consented: boolean, mongo = consented): AiSettings {
  return {
    config,
    has_key: true,
    origin: 'https://api.example.com',
    host: 'api.example.com',
    consented,
    consent_covers_mongo: consented && mongo,
  };
}

function backend(config: AiConfig, consented: boolean, initial: AiConnectionChoice[] = [], mongo = consented) {
  let agreed = consented;
  let agreedMongo = mongo;
  let choices = initial;
  mockBackend({
    ai_settings_get: () => settings(config, agreed, agreedMongo),
    ai_connections: () => choices,
    ai_consent_set: ({ granted }) => {
      // Granting agrees to the current consent text, which names MongoDB.
      agreed = granted as boolean;
      agreedMongo = agreed;
      return settings(config, agreed, agreedMongo);
    },
    ai_connection_set: ({ connId, enabled }) => {
      choices = [...choices.filter((c) => c.connection_id !== connId), { connection_id: connId as string, enabled: enabled as boolean }];
      return choices;
    },
  });
}

const production = (s: ReturnType<typeof config>) => ({ ...s, config: { ...s.config, env_tag: 'production' as const } });

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'Shop')]);
});

describe('AI status indicator', () => {
  it('asks for consent once, which turns AI on; off and on again need no dialog', async () => {
    backend(SET_UP, false);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    const button = await screen.findByTestId('status-ai');
    expect(button).toHaveTextContent('AI off');
    expect(button).toHaveAccessibleName('AI is off: nothing is sent until you turn it on');

    await user.click(button);
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI…' }));
    const dialog = await screen.findByTestId('ai-consent-dialog');
    expect(dialog).toHaveTextContent('Turn on AI suggestions');
    expect(dialog).toHaveTextContent('api.example.com');
    await user.click(within(dialog).getByRole('checkbox'));
    await user.click(within(dialog).getByRole('button', { name: 'Turn on AI' }));

    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent(/^AI$/));
    expect(callsOf('ai_consent_set')).toEqual([{ granted: true }]);
    // On by default: no choice of its own needed.
    expect(callsOf('ai_connection_set')).toEqual([]);
    expect(screen.getByTestId('status-ai')).toHaveAccessibleName('AI suggestions are on');

    await user.click(screen.getByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'Turn off AI for this connection' }));
    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent('AI off'));
    expect(screen.getByTestId('status-ai')).toHaveAccessibleName('AI is off for this connection');

    await user.click(screen.getByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI for this connection' }));
    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent(/^AI$/));
    expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument();
    expect(callsOf('ai_connection_set')).toEqual([
      { connId: ids.prod, enabled: false },
      { connId: ids.prod, enabled: true },
    ]);
  });

  it('a Production connection is off until turned on, and consent turns it on too', async () => {
    useConnections.getState().setStored([], [production(config(ids.prod, 'Shop'))]);
    backend(SET_UP, false);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    await user.click(await screen.findByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI…' }));
    const dialog = await screen.findByTestId('ai-consent-dialog');
    await user.click(within(dialog).getByRole('checkbox'));
    await user.click(within(dialog).getByRole('button', { name: 'Turn on AI' }));
    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent(/^AI$/));
    expect(callsOf('ai_connection_set')).toEqual([{ connId: ids.prod, enabled: true }]);
  });

  it('cancelling consent sends nothing', async () => {
    backend(SET_UP, false);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    await user.click(await screen.findByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI…' }));
    await user.click(within(await screen.findByTestId('ai-consent-dialog')).getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument();
    expect(callsOf('ai_consent_set')).toEqual([]);
    expect(callsOf('ai_connection_set')).toEqual([]);
  });

  it('shows nothing until AI is set up', async () => {
    backend({ ...SET_UP, base_url: null, model: null }, false);
    render(<AiStatus connId={ids.prod} />);
    await waitFor(() => expect(useAi.getState().loaded).toBe(true));
    expect(screen.queryByTestId('status-ai')).not.toBeInTheDocument();
  });

  it('AI settings… opens Settings on the AI tab', async () => {
    backend(SET_UP, false);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    await user.click(await screen.findByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'AI settings…' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'ai' });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('shows paused, failing and working states', async () => {
    backend(SET_UP, true);
    render(<AiStatus connId={ids.prod} />);
    const button = await screen.findByTestId('status-ai');
    await waitFor(() => expect(button).toHaveAccessibleName('AI suggestions are on'));

    useAi.getState().begin();
    await waitFor(() => expect(button).toHaveClass('ai-busy'));
    useAi.getState().end({ kind: 'paused', message: 'The API key was rejected (401).' });
    await waitFor(() => expect(button).toHaveAccessibleName('AI is paused: The API key was rejected (401).'));

    useAi.getState().end({ kind: 'ok' });
    for (let i = 0; i < AI_ERROR_AFTER; i++) useAi.getState().end({ kind: 'failed', message: 'The request timed out.' });
    await waitFor(() => expect(button).toHaveAccessibleName('AI requests are failing: The request timed out.'));
    expect(button).toHaveClass('ai-error');
  });
});

describe('AI status indicator in Command tabs', () => {
  const MONGO = '33333333-3333-4333-8333-333333333333';
  const REDIS = '44444444-4444-4444-8444-444444444444';

  beforeEach(() => {
    useConnections
      .getState()
      .setStored([], [config(MONGO, 'docs', { kind: 'mongo' }), config(REDIS, 'cache', { kind: 'redis' })]);
  });

  it('a consent from before MongoDB was named asks again for a MongoDB tab', async () => {
    backend(SET_UP, true, [], false);
    const user = userEvent.setup();
    render(<AiStatus connId={MONGO} />);
    const button = await screen.findByTestId('status-ai');
    await waitFor(() => expect(button).toHaveAccessibleName('AI is off: nothing is sent until you turn it on'));
    expect(button).toHaveTextContent('AI off');

    await user.click(button);
    expect(screen.queryByRole('menuitem', { name: 'Turn on AI for this connection' })).not.toBeInTheDocument();
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI…' }));
    const dialog = await screen.findByTestId('ai-consent-dialog');
    expect(dialog).toHaveTextContent('MongoDB');
    await user.click(within(dialog).getByRole('checkbox'));
    await user.click(within(dialog).getByRole('button', { name: 'Turn on AI' }));
    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent(/^AI$/));
    expect(callsOf('ai_consent_set')).toEqual([{ granted: true }]);
    expect(useAi.getState().consentedMongo).toBe(true);
  });

  it('a Redis tab offers nothing to turn on', async () => {
    backend(SET_UP, true);
    const user = userEvent.setup();
    render(<AiStatus connId={REDIS} />);
    const button = await screen.findByTestId('status-ai');
    await waitFor(() => expect(button).toHaveAccessibleName('AI suggestions are not available for this connection'));
    await user.click(button);
    expect(screen.getAllByRole('menuitem').map((m) => m.textContent)).toEqual(['AI settings…']);
  });
});
