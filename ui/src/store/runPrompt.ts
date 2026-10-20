// The Production confirmation and the read-only "blocked" notice of a run (spec §5.2):
// `runQuery` awaits them, `DialogHost` shows them.
import { create } from 'zustand';
import type { PlanView } from '../bindings';

/** The names `query_run` must receive for `plan`: its `confirm_name`s, once each, in statement order. */
export function confirmNamesOf(plan: PlanView): string[] {
  const names: string[] = [];
  for (const s of plan.stmts) {
    if (s.confirm_name != null && !names.includes(s.confirm_name)) names.push(s.confirm_name);
  }
  return names;
}

/** A confirmation's answer: the names the user typed to run, or null for Cancel. */
export type RunAnswer = string[] | null;

export interface PendingRunPrompt {
  kind: 'confirm' | 'blocked';
  plan: PlanView;
  resolve(answer: RunAnswer): void;
}

interface RunPromptState {
  pending: PendingRunPrompt | null;
  /**
   * Answers the prompt on screen (a confirmation: true = run, with the typed `names`, by default the
   * plan's own; a notice: ignored).
   */
  settle(ok: boolean, names?: string[]): void;
}

export const useRunPrompt = create<RunPromptState>()((set, get) => ({
  pending: null,
  settle: (ok, names) => {
    const pending = get().pending;
    if (!pending) return;
    set({ pending: null });
    pending.resolve(ok ? (names ?? confirmNamesOf(pending.plan)) : null);
  },
}));

function ask(kind: PendingRunPrompt['kind'], plan: PlanView): Promise<RunAnswer> {
  return new Promise((resolve) => {
    // A new prompt replaces the old one, which counts as cancelled.
    useRunPrompt.getState().settle(false);
    useRunPrompt.setState({ pending: { kind, plan, resolve } });
  });
}

/** Lists the statements that need a confirmation; the names to send when the user chose to run, else null. */
export const askRunConfirmation = (plan: PlanView): Promise<RunAnswer> => ask('confirm', plan);

/** Lists the statements a read-only connection refuses; resolves when the dialog is closed. */
export async function showBlocked(plan: PlanView): Promise<void> {
  await ask('blocked', plan);
}
