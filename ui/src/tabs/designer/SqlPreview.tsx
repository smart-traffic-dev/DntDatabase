import { useEffect, useRef, useState } from 'react';
import type { ElementRef, Impact } from '../../bindings';
import './sqlPreview.css';

/** The statements as one script, each ended by `;` (what Copy and Open in query hand over). */
export const scriptOf = (statements: string[]) => statements.map((s) => `${s};`).join('\n');

const nameOf = (el: ElementRef): string | null => ('name' in el ? el.name : null);

/** A line touches an element that loses data: it names it, quoted the MySQL or the PostgreSQL way. */
function losesData(line: string, impacts: Impact[]): boolean {
  return impacts.some((i) => {
    const name = i.kind === 'data_loss' ? nameOf(i.element) : null;
    return !!name && (line.includes(`\`${name.replaceAll('`', '``')}\``) || line.includes(`"${name.replaceAll('"', '""')}"`));
  });
}

const IMPACT_MARK: Record<Impact['kind'], string> = { data_loss: '!', not_transactional: '!', rebuild: '·' };

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

export interface SqlPreviewProps {
  statements: string[];
  impacts: Impact[];
  /** A problem of the change as a whole (`DesignRender.error`), shown as an inline alert. */
  error?: string | null;
  /** The SQL follows the draft (the designer's drawer) rather than a fixed plan (the review dialog). */
  live?: boolean;
  /** Shows "Open in query", which hands the script to a new query tab. */
  onOpenInQuery?(sql: string): void;
}

/**
 * Read-only SQL with numbered lines (lines of statements that lose data tinted danger), Copy and
 * Open in query, and the change's impacts beside it.
 */
export function SqlPreview({ statements, impacts, error, live, onOpenInQuery }: SqlPreviewProps) {
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const script = scriptOf(statements);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(script);
    } catch {
      setCopied(false);
      setCopyError(true);
      return;
    }
    setCopyError(false);
    setCopied(true);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1500);
  };

  let n = 0;
  const lines = statements.flatMap((s, si) => {
    const parts = s.split('\n');
    return parts.map((text, li) => ({ no: ++n, key: `${si}:${li}`, text: li === parts.length - 1 ? `${text};` : text, loss: losesData(text, impacts) }));
  });
  const count = plural(statements.length, 'statement');

  return (
    <section className="dz-preview" aria-label="SQL preview">
      <div className="dz-preview-bar">
        <strong>SQL preview</strong>
        <span className="dz-subtle">{live ? `live · ${count}` : count}</span>
        <span className="dz-spacer" />
        <button type="button" className="btn small" disabled={statements.length === 0} onClick={() => void copy()}>
          {copied ? 'Copied' : 'Copy'}
        </button>
        {onOpenInQuery ? (
          <button type="button" className="btn small" disabled={statements.length === 0} onClick={() => onOpenInQuery(script)}>
            Open in query
          </button>
        ) : null}
      </div>
      <div className="dz-preview-body">
        <div className="dz-sql-pane">
          {error ? (
            <div role="alert" className="dz-alert">
              {error}
            </div>
          ) : null}
          {copyError ? (
            <div role="alert" className="dz-alert">
              Couldn't copy — select the text and press Ctrl+C.
            </div>
          ) : null}
          {lines.length === 0 ? (
            <p className="dz-subtle dz-sql-empty">No changes to apply.</p>
          ) : (
            <div className="dz-sql" aria-label="SQL">
              {lines.map((l) => (
                <div key={l.key} className={`dz-sql-line${l.loss ? ' dz-loss' : ''}`}>
                  <span className="dz-ln" aria-hidden="true">
                    {l.no}
                  </span>
                  <span className="dz-code">{l.text}</span>
                </div>
              ))}
            </div>
          )}
        </div>
        <aside className="dz-impacts" aria-label="Impact">
          <strong className="dz-caption">Impact</strong>
          {impacts.length === 0 ? (
            <span className="dz-subtle">None</span>
          ) : (
            <ul>
              {impacts.map((i, k) => (
                <li key={k} className={`dz-impact dz-impact-${i.kind}`}>
                  <span aria-hidden="true">{IMPACT_MARK[i.kind]}</span>
                  <span>{i.message}</span>
                </li>
              ))}
            </ul>
          )}
        </aside>
      </div>
    </section>
  );
}
