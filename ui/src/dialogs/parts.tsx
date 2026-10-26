import { Fragment, useEffect, useId, useRef, useState, type ReactNode } from 'react';
import type { ActionReport, ServerInfo } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { CheckIcon, ErrorIcon, InfoIcon, LockIcon, WarningIcon } from '../components/icons';
import { useConnections } from '../store/connections';
import './parts.css';

/** "MySQL 8.0.36", "MariaDB 11.4.2", "PostgreSQL 16.2". */
export function serverLabel(info: ServerInfo | undefined): string | null {
  if (!info) return null;
  const name = info.variant === 'MariaDb' ? 'MariaDB' : info.kind === 'mysql' ? 'MySQL' : 'PostgreSQL';
  return `${name} ${info.version.join('.')}`;
}

/**
 * Where a dialog acts: the environment chip, the connection and the path to the object, the server
 * version and a read-only mark. Tinted on Production.
 */
export function ContextBar({ connectionId, path = [] }: { connectionId: string; path?: (string | null | undefined)[] }) {
  const cfg = useConnections((s) => s.summaries.get(connectionId)?.config);
  const info = useConnections((s) => s.info.get(connectionId));
  const env = cfg?.env_tag ?? null;
  const server = serverLabel(info);
  return (
    <div className={`ctx-bar${env === 'production' ? ' ctx-prod' : ''}`} data-testid="context-bar">
      {env && <EnvTag env={env} variant="chip" testId="env-chip" />}
      <strong className="ctx-conn">{cfg?.name ?? connectionId}</strong>
      {path.filter((p): p is string => !!p).map((p, i) => (
        <Fragment key={i}>
          <span className="ctx-sep" aria-hidden="true">
            ›
          </span>
          <span className="ctx-seg">{p}</span>
        </Fragment>
      ))}
      <span className="ctx-grow" />
      {cfg?.read_only && (
        <span className="ctx-ro">
          <LockIcon />
          Read-only
        </span>
      )}
      {server && <span className="ctx-meta">{server}</span>}
    </div>
  );
}

/** "Type orders to confirm": the one typed confirmation of every destructive dialog. */
export function TypedConfirm({
  name,
  value,
  onChange,
  disabled,
  prefix,
}: {
  name: string;
  value: string;
  onChange(v: string): void;
  disabled?: boolean;
  /** Words before "Type" (e.g. "Production ·"). */
  prefix?: string;
}) {
  const id = useId();
  const left = [...name].length - [...value].length;
  const matches = value === name;
  return (
    <div className="typed-confirm">
      <label htmlFor={id}>
        {prefix ? `${prefix} ` : ''}Type <code>{name}</code> to confirm
      </label>
      <div className="typed-confirm-row">
        <input
          id={id}
          aria-label="Confirmation name"
          value={value}
          // Read-only, not disabled: a disabled field drops focus out of the dialog.
          readOnly={disabled}
          autoComplete="off"
          spellCheck={false}
          onChange={(e) => onChange(e.target.value)}
        />
        {value && !matches && left > 0 && name.startsWith(value) && <span className="typed-confirm-left">{left} more</span>}
        {matches && (
          <span className="typed-confirm-ok" aria-hidden="true">
            <CheckIcon />
          </span>
        )}
      </div>
    </div>
  );
}

export type CalloutTone = 'info' | 'warning' | 'danger' | 'success' | 'neutral';

const CALLOUT_ICON: Record<CalloutTone, ReactNode> = {
  info: <InfoIcon />,
  neutral: <InfoIcon />,
  warning: <WarningIcon />,
  danger: <ErrorIcon />,
  success: <CheckIcon />,
};

export function Callout({ tone, title, children, role }: { tone: CalloutTone; title?: ReactNode; children?: ReactNode; role?: 'status' | 'alert' | 'note' }) {
  return (
    <div className={`callout callout-${tone}`} role={role}>
      <span className="callout-icon" aria-hidden="true">
        {CALLOUT_ICON[tone]}
      </span>
      <div className="callout-body">
        {title && <strong className="callout-title">{title}</strong>}
        {children}
      </div>
    </div>
  );
}

/** The outcome of an applied action, one look in every dialog. */
export function OutcomeReport({ report, applied = 'Operation applied.' }: { report: ActionReport; applied?: string }) {
  const { outcome, warnings } = report;
  const tone: CalloutTone = outcome === 'applied' ? 'success' : outcome === 'unknown' ? 'warning' : 'danger';
  const title =
    outcome === 'applied'
      ? applied
      : outcome === 'unknown'
        ? 'Outcome unknown. Do not retry. Reload metadata to check what happened.'
        : outcome === 'rolled_back'
          ? 'The operation was rolled back.'
          : 'The operation was not applied.';
  return (
    <Callout tone={tone} title={title} role="status">
      {warnings.map((w) => (
        <p key={w}>{w}</p>
      ))}
    </Callout>
  );
}

export interface Impact {
  value: string;
  label: string;
  tone?: 'danger' | 'warning';
}

/** A row of numbers the action touches (rows, size, dependent objects). */
export function ImpactTiles({ items, loading }: { items: Impact[]; loading?: boolean }) {
  if (!loading && items.length === 0) return null;
  return (
    <div className="impact-tiles" aria-label="Impact" aria-busy={loading || undefined}>
      {loading
        ? [0, 1, 2].map((i) => (
            <div key={i} className="impact-tile impact-skeleton">
              <span />
              <span />
            </div>
          ))
        : items.map((i) => (
            <div key={i.label} className={`impact-tile${i.tone ? ` impact-${i.tone}` : ''}`}>
              <strong>{i.value}</strong>
              <span>{i.label}</span>
            </div>
          ))}
    </div>
  );
}

/** Read-only SQL with Copy and Open in query; `placeholder` stands in while there is none. */
export function SqlBlock({ sql, placeholder, caption, onOpenInQuery }: { sql: string | null; placeholder?: string; caption?: string; onOpenInQuery?(sql: string): void }) {
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  const copy = async () => {
    if (!sql) return;
    try {
      await navigator.clipboard.writeText(sql);
    } catch {
      setCopyError(true);
      return;
    }
    setCopyError(false);
    setCopied(true);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1500);
  };

  return (
    <section className="sql-block">
      <div className="sql-block-bar">
        <strong>SQL</strong>
        {caption && <span className="sql-block-caption">· {caption}</span>}
        <span className="ctx-grow" />
        <button type="button" className="btn small ghost" disabled={!sql} onClick={() => void copy()}>
          {copied ? 'Copied' : 'Copy'}
        </button>
        {onOpenInQuery && (
          <button type="button" className="btn small ghost" disabled={!sql} onClick={() => sql && onOpenInQuery(sql)}>
            Open in query
          </button>
        )}
      </div>
      <pre className={`sql-block-text${sql ? '' : ' sql-block-empty'}`} aria-label="SQL preview" aria-live="polite">
        {sql ?? placeholder ?? ''}
      </pre>
      {copyError && (
        <p role="alert" className="field-error sql-block-error">
          Couldn't copy — select the text and press Ctrl+C.
        </p>
      )}
    </section>
  );
}

/** Footer text left of the buttons: why the primary is disabled, or what is going on. */
export function FooterNote({ children }: { children?: ReactNode }) {
  if (!children) return <span className="spacer" />;
  return (
    <>
      <span className="foot-note">{children}</span>
      <span className="spacer" />
    </>
  );
}
