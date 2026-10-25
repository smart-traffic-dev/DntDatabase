import type { DumpRequest, JobParams, RestoreRequest, ToolDumpRequest } from '../../bindings';
import { registerRerun } from '../../jobs/rerun';
import { useDialogs } from '../../store/dialogs';

/**
 * "Run again" of dumps and restores (R5). A dump reopens its dialog pre-filled and starts from its
 * Start button; a restore reopens pre-filled but must be analyzed and confirmed again (its plan
 * was single use). Nothing runs from here.
 */
function reopenDump(params: { kind: 'dump'; request: DumpRequest } | { kind: 'tool_dump'; request: ToolDumpRequest }) {
  const { connection_id: connectionId, database } = params.request;
  useDialogs.getState().requestDialog({ kind: 'dump', connectionId, database, params });
}

function reopenRestore(request: RestoreRequest) {
  useDialogs.getState().requestDialog({ kind: 'restore', connectionId: request.connection_id, database: request.database, params: request });
}

registerRerun('dump', (p: JobParams) => reopenDump({ kind: 'dump', request: p.request as unknown as DumpRequest }));
registerRerun('tool_dump', (p: JobParams) => reopenDump({ kind: 'tool_dump', request: p.request as unknown as ToolDumpRequest }));
registerRerun('restore', (p: JobParams) => reopenRestore(p.request as unknown as RestoreRequest));
registerRerun('tool_restore', (p: JobParams) => reopenRestore(p.request as unknown as RestoreRequest));
