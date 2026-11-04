import { useState } from 'react';
import { Modal } from './Modal';

/**
 * Consent before AI editor suggestions are turned on for one connection (AI editor completion
 * spec §6.3). Says what leaves the machine and where to; "Turn on AI" stays disabled until the
 * user confirms they may share it. Enter and the initial focus are Cancel.
 */
export function AiConsentDialog({
  connectionName,
  host,
  onConfirm,
  onClose,
}: {
  connectionName: string;
  /** Host of the AI endpoint's base URL. */
  host: string;
  onConfirm(): void;
  onClose(): void;
}) {
  const [allowed, setAllowed] = useState(false);
  return (
    <Modal
      title={`Turn on AI for ${connectionName}`}
      subtitle={`Suggestions come from ${host}`}
      tone="warning"
      role="alertdialog"
      onClose={onClose}
      initialFocus="[data-cancel]"
      testId="ai-consent-dialog"
    >
      <div className="modal-body stack">
        <p>
          While you type in a query tab of this connection, the app sends to <strong>{host}</strong>:
        </p>
        <ul>
          <li>the text of the query editor, with passwords hidden;</li>
          <li>the names and types of this database's tables and columns.</li>
        </ul>
        <p>
          Row data is never sent. You can turn AI off for this connection at any time in Settings → AI or the connection's
          settings.
        </p>
        <label className="check">
          <input type="checkbox" checked={allowed} onChange={(e) => setAllowed(e.target.checked)} />
          <span>I am allowed to share this database's schema and queries with this endpoint</span>
        </label>
      </div>
      <footer className="modal-foot">
        <span className="spacer" />
        <button type="button" className="btn" data-cancel onClick={onClose}>
          Cancel
        </button>
        <button type="button" className="btn primary" disabled={!allowed} onClick={onConfirm}>
          Turn on AI
        </button>
      </footer>
    </Modal>
  );
}
