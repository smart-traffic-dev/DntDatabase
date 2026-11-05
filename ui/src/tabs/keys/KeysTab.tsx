import { useEffect, useState } from 'react';
import { useCaps } from '../../engine/useCaps';
import { useConnections } from '../../store/connections';
import { useKeysTabs } from '../../store/keysTabs';
import type { KeysTabRef } from '../../store/tabs';
import { useTree } from '../../store/tree';
import { connNodeId, seg } from '../../store/treeModel';
import { EditPreviewDialog } from '../data/EditPreviewDialog';
import { NewKeyDialog } from './KeyDialogs';
import { newKeyEdit } from './keyEdits';
import { KeyList } from './KeyList';
import { KeyValuePane } from './KeyValuePane';
import './keys.css';

/**
 * The key browser of one Redis database (spec §7.6): the scanned keys on the left, the selected
 * key's value on the right. Every change is one `KeyEdit`, reviewed in `EditPreviewDialog` and
 * applied by `data_apply`. Without `data_edit`, or on a read-only connection, it only reads.
 */
export function KeysTab({ tab }: { tab: KeysTabRef }) {
  const state = useKeysTabs((s) => s.byTab[tab.id]);
  const caps = useCaps(tab.connectionId);
  const readOnly = useConnections((s) => s.summaries.get(tab.connectionId)?.config.read_only ?? false);
  const total = useTree((s) => s.nodes.get(`${connNodeId(tab.connectionId)}/db:${seg(tab.database)}`)?.keyCount ?? null);
  const [creating, setCreating] = useState(false);
  const { id, connectionId, database } = tab;

  useEffect(() => {
    useKeysTabs.getState().ensure({ id, connectionId, database });
  }, [id, connectionId, database]);

  if (!state) return null;
  const editable = !!caps?.data_edit && !readOnly;
  const store = useKeysTabs.getState();
  return (
    <div className="keys-tab" data-testid="keys-tab">
      <div className="kt-bar" role="toolbar" aria-label="Keys">
        {editable ? (
          <button type="button" className="btn small" onClick={() => setCreating(true)}>
            New key…
          </button>
        ) : (
          <span className="muted">Read-only</span>
        )}
      </div>
      <div className="kt-body">
        <KeyList tabId={tab.id} state={state} total={total} />
        <KeyValuePane tabId={tab.id} state={state} editable={editable} />
      </div>
      {creating ? (
        <NewKeyDialog
          onCancel={() => setCreating(false)}
          onSubmit={(k) => {
            setCreating(false);
            void store.preview(tab.id, newKeyEdit(k), k.type);
          }}
        />
      ) : null}
      {state.pending ? (
        <EditPreviewDialog
          pending={state.pending}
          family="command"
          onConfirm={() => void store.confirm(tab.id)}
          onCancel={() => store.cancel(tab.id)}
        />
      ) : null}
    </div>
  );
}
