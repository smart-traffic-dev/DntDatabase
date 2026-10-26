import { Channel } from '@tauri-apps/api/core';
import { useEffect, useRef, useState } from 'react';
import { api, toAppError } from '../api';
import type { UpdateInfo, UpdateProgress } from '../bindings';
import { quitThen } from '../store/queryTabs';
import { Modal } from './Modal';

type Phase =
  | { kind: 'checking' }
  | { kind: 'upToDate' }
  | { kind: 'available'; info: UpdateInfo }
  | { kind: 'installing'; info: UpdateInfo; progress: UpdateProgress | null }
  | { kind: 'error'; message: string; info: UpdateInfo | null };

function percentOf(p: UpdateProgress | null): number | null {
  if (!p || !p.total) return null;
  return Math.min(100, Math.round((p.downloaded / p.total) * 100));
}

function megabytes(bytes: number): string {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * Help > Check for updates (also opened at startup when a newer release exists, with `found`).
 * Install quits like closing the window (jobs, staged edits, open transactions are asked about
 * first), then downloads the signed installer and runs it; the installer restarts the app.
 */
export function UpdateDialog({ found, onClose }: { found?: UpdateInfo; onClose(): void }) {
  const [phase, setPhase] = useState<Phase>(found ? { kind: 'available', info: found } : { kind: 'checking' });
  const live = useRef(true);

  const check = async () => {
    setPhase({ kind: 'checking' });
    try {
      const info = await api.updateCheck();
      if (live.current) setPhase(info ? { kind: 'available', info } : { kind: 'upToDate' });
    } catch (e) {
      if (live.current) setPhase({ kind: 'error', message: toAppError(e).message, info: null });
    }
  };

  useEffect(() => {
    live.current = true;
    if (!found) void check();
    return () => {
      live.current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const install = async (info: UpdateInfo) => {
    setPhase({ kind: 'installing', info, progress: null });
    try {
      const done = await quitThen(async () => {
        const channel = new Channel<UpdateProgress>();
        channel.onmessage = (progress) => {
          if (live.current) setPhase({ kind: 'installing', info, progress });
        };
        await api.updateInstall(channel);
      });
      // Kept open (Cancel in one of the quit questions): nothing was downloaded.
      if (!done && live.current) setPhase({ kind: 'available', info });
    } catch (e) {
      // `update_install` consumed the pending update: a retry checks again first.
      if (live.current) setPhase({ kind: 'error', message: toAppError(e).message, info: null });
    }
  };

  const installing = phase.kind === 'installing';
  const percent = installing ? percentOf(phase.progress) : null;

  return (
    <Modal title="Software update" onClose={onClose} className="narrow" testId="update-dialog" closable={!installing}>
      <div className="modal-body stack">
        {phase.kind === 'checking' && <p>Checking for updates…</p>}
        {phase.kind === 'upToDate' && <p data-testid="update-status">DntDatabase is up to date.</p>}
        {(phase.kind === 'available' || phase.kind === 'installing') && (
          <>
            <p data-testid="update-status">
              <strong>DntDatabase {phase.info.version}</strong> is available (you have {phase.info.current_version}).
            </p>
            {phase.info.notes && (
              <pre className="update-notes" data-testid="update-notes">
                {phase.info.notes}
              </pre>
            )}
            {phase.kind === 'available' && <p>Installing closes DntDatabase and starts the new version.</p>}
          </>
        )}
        {installing && (
          <>
            <div
              className={`jb-bar${percent === null ? ' indeterminate' : ''}`}
              role="progressbar"
              aria-label="Download progress"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={percent ?? undefined}
            >
              <span className="jb-bar-fill" style={percent === null ? undefined : { width: `${percent}%` }} />
            </div>
            <p>
              {phase.progress
                ? `Downloading… ${megabytes(phase.progress.downloaded)}${phase.progress.total ? ` of ${megabytes(phase.progress.total)}` : ''}`
                : 'Preparing to install…'}
            </p>
          </>
        )}
        {phase.kind === 'error' && (
          <p role="alert" className="field-error">
            {phase.message}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" onClick={onClose} disabled={installing}>
          {phase.kind === 'available' ? 'Later' : 'Close'}
        </button>
        {phase.kind === 'available' && (
          <button type="button" className="btn primary" onClick={() => void install(phase.info)}>
            Install and restart
          </button>
        )}
        {phase.kind === 'error' && (
          <button type="button" className="btn primary" onClick={() => void check()}>
            Try again
          </button>
        )}
      </footer>
    </Modal>
  );
}
