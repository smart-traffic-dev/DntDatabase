import type { JobParams } from '../bindings';

/**
 * "Run again" (R5): reopens the job's dialog or wizard pre-filled from its stored parameters.
 * The import, dump and restore UIs register a handler per kind; a write job goes through its
 * preview, plan and confirmation again, nothing re-runs from here directly.
 */
type RerunKind = JobParams['kind'];
type RerunHandler = (params: JobParams) => void;

const handlers = new Map<RerunKind, RerunHandler>();

/** Registers the handler of `kind`; returns the unregister (tests, hot reload). */
export function registerRerun(kind: RerunKind, handler: RerunHandler): () => void {
  handlers.set(kind, handler);
  return () => {
    if (handlers.get(kind) === handler) handlers.delete(kind);
  };
}

/** A handler exists for these parameters. */
export const canRerun = (params: JobParams | null | undefined): params is JobParams => !!params && handlers.has(params.kind);

/** Opens the job's dialog again; `false` when no handler knows the kind. */
export function rerun(params: JobParams): boolean {
  const handler = handlers.get(params.kind);
  if (!handler) return false;
  handler(params);
  return true;
}
