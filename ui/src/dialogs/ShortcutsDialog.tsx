import { Modal } from './Modal';

const SHORTCUTS: { group: string; items: [string, string][] }[] = [
  {
    group: 'Query',
    items: [
      ['Run the statement at the cursor', 'Ctrl+Enter'],
      ['Run all statements', 'Ctrl+Shift+Enter'],
      ['Explain', 'Ctrl+Shift+E'],
      ['Stop the running query', 'Esc'],
    ],
  },
  {
    group: 'Tabs and files',
    items: [
      ['New query', 'Ctrl+T'],
      ['Open SQL file', 'Ctrl+O'],
      ['Save', 'Ctrl+S'],
      ['Save as', 'Ctrl+Shift+S'],
      ['Close tab', 'Ctrl+W'],
      ['Next / previous tab', 'Ctrl+Tab / Ctrl+Shift+Tab'],
    ],
  },
  {
    group: 'Window',
    items: [
      ['Show / hide the sidebar', 'Ctrl+B'],
      ['Developer tools', 'Ctrl+Shift+I'],
      ['Open a menu', 'Alt+F, E, V, Q, T, H'],
    ],
  },
];

/** The app's keyboard shortcuts (Help > Keyboard shortcuts). */
export function ShortcutsDialog({ onClose }: { onClose(): void }) {
  return (
    <Modal title="Keyboard shortcuts" onClose={onClose} className="narrow">
      <div className="modal-body stack">
        {SHORTCUTS.map((g) => (
          <section key={g.group}>
            <h3 className="field-label">{g.group}</h3>
            <dl className="kv">
              {g.items.map(([action, keys]) => (
                <div key={action} style={{ display: 'contents' }}>
                  <dt>{action}</dt>
                  <dd>
                    <kbd className="mono">{keys}</kbd>
                  </dd>
                </div>
              ))}
            </dl>
          </section>
        ))}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" onClick={onClose}>
          Close
        </button>
      </footer>
    </Modal>
  );
}
