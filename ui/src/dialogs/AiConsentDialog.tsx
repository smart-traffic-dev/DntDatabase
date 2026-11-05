import { useState } from 'react';
import { Modal } from './Modal';

/**
 * Consent before AI editor suggestions are turned on (AI editor completion spec §6.3), once for
 * the endpoint's host, not per connection. Says what leaves the machine and where to; "Turn on AI"
 * stays disabled until the user confirms they may share it. Enter and the initial focus are Cancel.
 */
export function AiConsentDialog({
  host,
  onConfirm,
  onClose,
}: {
  /** Host of the AI endpoint's base URL. */
  host: string;
  onConfirm(): void;
  onClose(): void;
}) {
  const [allowed, setAllowed] = useState(false);
  return (
    <Modal
      title="Turn on AI suggestions"
      subtitle={`Suggestions come from ${host}`}
      tone="warning"
      role="alertdialog"
      onClose={onClose}
      initialFocus="[data-cancel]"
      testId="ai-consent-dialog"
    >
      <div className="modal-body stack">
        <p>
          While you type in a query tab, the app sends to <strong>{host}</strong>:
        </p>
        <ul>
          <li>the text of the query editor, with passwords hidden;</li>
          <li>the names and types of that database's tables and columns.</li>
        </ul>
        <p>
          Row data is never sent. AI is on for every MySQL and PostgreSQL connection except those tagged Production. Turn
          it off for a connection from its query tab's status bar, its settings, or Settings → AI.
        </p>
        <label className="check">
          <input type="checkbox" checked={allowed} onChange={(e) => setAllowed(e.target.checked)} />
          <span>I am allowed to share these databases' schemas and queries with this endpoint</span>
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
