import type { JobParams, SeedRequest } from '../../bindings';
import { registerRerun } from '../../jobs/rerun';
import { useDialogs } from '../../store/dialogs';

/**
 * "Run again" of a seed job: the dialog opens with the stored request. Nothing runs from here: the
 * user goes through the preview, the review and the confirmation again.
 */
registerRerun('seed', (params: JobParams) => {
  const request = params.request as unknown as SeedRequest;
  useDialogs.getState().requestDialog({
    kind: 'seedData',
    connectionId: request.connection_id,
    table: request.table,
    params: request,
    rerun: Date.now(),
  });
});
