// The Tauri `Channel` a run streams its `RunEvent`s through (spec §6.1): pass it to
// `query_run`; every event reaches `onEvent` in order, ending with `done`.
import { Channel } from '@tauri-apps/api/core';
import type { RunEvent } from '../bindings';

export function runChannel(onEvent: (ev: RunEvent) => void): Channel<RunEvent> {
  const channel = new Channel<RunEvent>();
  channel.onmessage = onEvent;
  return channel;
}
