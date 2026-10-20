// "Retry statement" and "Fetch all" (spec §6.2): both run one stored statement of the last
// run again. The backend derives a plan of it (`query_plan_statement`: the statement verbatim,
// DELIMITER context kept, guarded again with the current config); that plan then goes through
// the normal run flow.
import { api, toAppError } from '../../api';
import { runChannel } from '../../api/query';
import type { PlanView } from '../../bindings';
import { canRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';

export interface RerunOptions {
  /** Row limit of this run; defaults to the tab's row limit. */
  maxRows?: number;
}

/** Runs a derived plan through the full run flow (confirmation included). */
export type PlanRunner = (tabId: string, plan: PlanView, opts: RerunOptions) => Promise<void>;

let registered: PlanRunner | null = null;

/** The run flow registers itself here, so a derived plan with a Confirm verdict asks like any run. */
export function setPlanRunner(runner: PlanRunner | null) {
  registered = runner;
}

/** Fallback when the run flow has not registered: runs plans that need no confirmation. */
async function runAllowed(tabId: string, plan: PlanView, opts: RerunOptions): Promise<void> {
  const store = useQueryRun.getState();
  if (plan.stmts.some((s) => s.verdict !== 'allow')) {
    store.apply(tabId, {
      t: 'notice',
      index: plan.stmts[0]?.index ?? 0,
      message: 'This statement needs confirmation; run it from the editor.',
    });
    return;
  }
  const doc = useQueryDocs.getState().docs.get(tabId);
  const before = store.get(tabId);
  store.begin(tabId, plan);
  try {
    const runId = await api.queryRun(
      plan.plan_id,
      false,
      { max_rows: opts.maxRows ?? doc?.rowLimit ?? 10000, continue_on_error: doc?.continueOnError ?? false },
      runChannel((ev) => useQueryRun.getState().apply(tabId, ev, plan.plan_id)),
    );
    useQueryRun.getState().attach(tabId, runId);
  } catch (e) {
    // The run never started: the previous results stay, with the reason.
    useQueryRun.setState((s) => ({ runs: new Map(s.runs).set(tabId, before) }));
    useQueryRun.getState().apply(tabId, { t: 'notice', index: plan.stmts[0]?.index ?? 0, message: toAppError(e).message });
  }
}

/** Tabs with a derive-and-run in flight: a second click meanwhile does nothing. */
const pending = new Set<string>();

/**
 * Runs statement `index` of the tab's last run again. Does nothing while a run is active or
 * before any run. Derived plans do not evict the source plan, so this works repeatedly.
 */
export async function rerunStatement(tabId: string, index: number, opts: RerunOptions = {}): Promise<void> {
  const run = useQueryRun.getState().get(tabId);
  if (!run.plan || !canRun(run) || pending.has(tabId)) return;
  const source = run.plan.plan_id;
  pending.add(tabId);
  try {
    let derived: PlanView;
    try {
      derived = await api.queryPlanStatement(source, index);
    } catch (e) {
      // e.g. plan_expired: the statement can only be run from the editor again.
      useQueryRun.getState().apply(tabId, { t: 'notice', index, message: toAppError(e).message });
      return;
    }
    // A run (or a newer plan) may have started while the plan was derived.
    const now = useQueryRun.getState().get(tabId);
    if (!canRun(now) || now.plan?.plan_id !== source) return;
    await (registered ?? runAllowed)(tabId, derived, opts);
  } finally {
    pending.delete(tabId);
  }
}