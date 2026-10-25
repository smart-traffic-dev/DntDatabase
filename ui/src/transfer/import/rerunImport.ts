import type { DryRunRequest, ImportRequest, JobParams } from '../../bindings';
import { registerRerun } from '../../jobs/rerun';
import { useDialogs } from '../../store/dialogs';

/**
 * "Run again" of an import or a dry run (R5): the wizard opens pre-filled from the stored request.
 * Nothing runs from here: the user goes through the file check, the review and the confirmation again.
 */
function reopen(request: ImportRequest) {
  useDialogs.getState().requestDialog({ kind: 'importData', connectionId: request.connection_id, table: request.table, params: request });
}

registerRerun('import', (params: JobParams) => reopen(params.request as unknown as ImportRequest));
registerRerun('import_dry_run', (params: JobParams) => reopen((params.request as unknown as DryRunRequest).request));
