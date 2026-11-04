import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { AiConfig, AiConnectionView, AiSettings } from '../bindings';
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

function settings(config: AiConfig): AiSettings {
  return { config, has_key: true, origin: 'https://api.example.com', host: 'api.example.com' };
}

function backend(config: AiConfig, optIns: AiConnectionView[]) {
  let list = optIns;
  mockBackend({
    ai_settings_get: () => settings(config),
    ai_opt_in_list: () => list,
    ai_opt_in_set: ({ connId, enabled }) => {
      list = enabled ? [{ connection_id: connId as string, name: 'Production', granted_at: 'now', active: true }] : [];
      return list;
    },
  });
}

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'Production')]);
});

describe('AI status indicator', () => {
  it('turns AI on through the consent dialog, then off', async () => {
    backend(SET_UP, []);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    const button = await screen.findByTestId('status-ai');
    expect(button).toHaveTextContent('AI off');
    expect(button).toHaveAccessibleName('AI is off for this connection');

    await user.click(button);
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI for this connection…' }));
    const dialog = await screen.findByTestId('ai-consent-dialog');
    expect(dialog).toHaveTextContent('Turn on AI for Production');
    expect(dialog).toHaveTextContent('api.example.com');
    await user.click(within(dialog).getByRole('checkbox'));
    await user.click(within(dialog).getByRole('button', { name: 'Turn on AI' }));

    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent(/^AI$/));
    expect(callsOf('ai_opt_in_set')).toEqual([{ connId: ids.prod, enabled: true }]);
    expect(screen.getByTestId('status-ai')).toHaveAccessibleName('AI suggestions are on');

    await user.click(screen.getByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'Turn off AI for this connection' }));
    await waitFor(() => expect(screen.getByTestId('status-ai')).toHaveTextContent('AI off'));
  });

  it('cancelling consent sends nothing', async () => {
    backend(SET_UP, []);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    await user.click(await screen.findByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'Turn on AI for this connection…' }));
    await user.click(within(await screen.findByTestId('ai-consent-dialog')).getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByTestId('ai-consent-dialog')).not.toBeInTheDocument();
    expect(callsOf('ai_opt_in_set')).toEqual([]);
  });

  it('shows nothing until AI is set up', async () => {
    backend({ ...SET_UP, base_url: null, model: null }, []);
    render(<AiStatus connId={ids.prod} />);
    await waitFor(() => expect(useAi.getState().loaded).toBe(true));
    expect(screen.queryByTestId('status-ai')).not.toBeInTheDocument();
  });

  it('AI settings… opens Settings on the AI tab', async () => {
    backend(SET_UP, []);
    const user = userEvent.setup();
    render(<AiStatus connId={ids.prod} />);
    await user.click(await screen.findByTestId('status-ai'));
    await user.click(screen.getByRole('menuitem', { name: 'AI settings…' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'ai' });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('shows paused, failing and working states', async () => {
    backend(SET_UP, [{ connection_id: ids.prod, name: 'Production', granted_at: 'now', active: true }]);
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
