import { getVersion } from '@tauri-apps/api/app';
import { useEffect, useState } from 'react';
import { Modal } from './Modal';

/** Help > About DntDatabase: name, version (from the app; "dev" outside Tauri) and a one-line description. */
export function AboutDialog({ onClose }: { onClose(): void }) {
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    getVersion().then(
      (v) => live && setVersion(v),
      () => live && setVersion('dev'),
    );
    return () => {
      live = false;
    };
  }, []);
  return (
    <Modal title="About DntDatabase" onClose={onClose} className="narrow">
      <div className="modal-body stack">
        <p>
          <strong>DntDatabase</strong> <span data-testid="about-version">{version ? `version ${version}` : ''}</span>
        </p>
        <p>Database manager for MySQL, MariaDB and PostgreSQL</p>
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" onClick={onClose}>
          Close
        </button>
      </footer>
    </Modal>
  );
}
