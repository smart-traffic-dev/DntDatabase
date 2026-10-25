import type { DumpRequest, JobParams, RestoreRequest, ToolDumpRequest } from '../../bindings';
import { registerRerun } from '../../jobs/rerun';
import { useDialogs } from '../../store/dialogs';

/**
 * "Run again" of dumps and restores (R5). A dump reopens its dialog pre-filled and starts from its
 * Start button; a restore reopens pre-filled but must be analyzed and confirmed again (its plan
 * was single use). Nothing runs from here.
 */

// Every "Run again" opens a fresh dialog (the handlers get the stored parameters, not the job id).
let reruns = 0;

function reopenDump(params: { kind: 'dump'; request: DumpRequest } | { kind: 'tool_dump'; request: ToolDumpRequest }) {
  const { connection_id: connectionId, database } = params.request;
  reruns += 1;
  useDialogs.getState().requestDialog({ kind: 'dump', connectionId, database, params, rerun: reruns });
}

function reopenRestore(request: RestoreRequest) {
  reruns += 1;
  useDialogs.getState().requestDialog({ kind: 'restore', connectionId: request.connection_id, database: request.database, params: request, rerun: reruns });
}

registerRerun('dump', (p: JobParams) => reopenDump({ kind: 'dump', request: p.request as unknown as DumpRequest }));
registerRerun('tool_dump', (p: JobParams) => reopenDump({ kind: 'tool_dump', request: p.request as unknown as ToolDumpRequest }));
registerRerun('restore', (p: JobParams) => reopenRestore(p.request as unknown as RestoreRequest));
registerRerun('tool_restore', (p: JobParams) => reopenRestore(p.request as unknown as RestoreRequest));
