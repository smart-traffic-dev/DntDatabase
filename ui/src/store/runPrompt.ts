// The Production confirmation and the read-only "blocked" notice of a run (spec §5.2):
// `runQuery` awaits them, `DialogHost` shows them.
import { create } from 'zustand';
import type { PlanView } from '../bindings';

export interface PendingRunPrompt {
  kind: 'confirm' | 'blocked';
  plan: PlanView;
  resolve(ok: boolean): void;
}

interface RunPromptState {
  pending: PendingRunPrompt | null;
  /** Answers the prompt on screen (a confirmation: true = run; a notice: ignored). */
  settle(ok: boolean): void;
}

export const useRunPrompt = create<RunPromptState>()((set, get) => ({
  pending: null,
  settle: (ok) => {
    const pending = get().pending;
    if (!pending) return;
    set({ pending: null });
    pending.resolve(ok);
  },
}));

function ask(kind: PendingRunPrompt['kind'], plan: PlanView): Promise<boolean> {
  return new Promise((resolve) => {
    // A new prompt replaces the old one, which counts as cancelled.
    useRunPrompt.getState().settle(false);
    useRunPrompt.setState({ pending: { kind, plan, resolve } });
  });
}

/** Lists the statements that need a confirmation; true when the user chose to run. */
export const askRunConfirmation = (plan: PlanView): Promise<boolean> => ask('confirm', plan);

/** Lists the statements a read-only connection refuses; resolves when the dialog is closed. */
export async function showBlocked(plan: PlanView): Promise<void> {
  await ask('blocked', plan);
}
