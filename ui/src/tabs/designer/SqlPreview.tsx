import { useEffect, useRef, useState } from 'react';
import type { ElementRef, Impact } from '../../bindings';
import './sqlPreview.css';

const PG_HEADER = [
  '-- The table designer applies these statements in one transaction (BEGIN … COMMIT),',
  "-- after SET LOCAL lock_timeout = '5s' and SET LOCAL standard_conforming_strings = on.",
].join('\n');

/** The PostgreSQL header of a DDL edit's script (`ddl_edit/apply.rs`). */
export const DDL_EDIT_PG_HEADER = [
  '-- These statements are applied in one transaction (BEGIN … COMMIT),',
  "-- after SET lock_timeout = '5s' and SET LOCAL standard_conforming_strings = on.",
].join('\n');

/** A MySQL `DELIMITER`: the first of `$$`, `//`, `;;`, `$$1`, `$$2`, … that no statement contains (as `ddl_edit/apply.rs`). */
export function scriptDelimiter(statements: string[]): string {
  const free = (d: string) => !statements.some((s) => s.includes(d));
  for (const d of ['$$', '//', ';;']) if (free(d)) return d;
  for (let n = 1; ; n++) if (free(`$$${n}`)) return `$$${n}`;
}

export interface ScriptOptions {
  /** The PostgreSQL header; defaults to the table designer's. */
  pgHeader?: string;
  /** MySQL compound statements (routines, triggers, events): a re-runnable `DELIMITER` script. */
  delimited?: boolean;
}

/**
 * The statements as one script, each ended by `;` (what Copy and Open in query hand over). On
 * PostgreSQL a header says how the designer runs them, and `transaction` wraps them in BEGIN / COMMIT.
 * On MySQL, `delimited` ends each with a delimiter no statement contains, between `DELIMITER` lines.
 */
export function scriptOf(statements: string[], engine: 'mysql' | 'postgres' = 'mysql', transaction = false, options: ScriptOptions = {}): string {
  if (engine !== 'postgres') {
    if (!options.delimited) return statements.map((s) => `${s};`).join('\n');
    const d = scriptDelimiter(statements);
    return `DELIMITER ${d}\n${statements.map((s) => `${s}${d}`).join('\n')}\nDELIMITER ;`;
  }
  const body = statements.map((s) => `${s};`).join('\n');
  const header = options.pgHeader ?? PG_HEADER;
  return transaction ? `${header}\nBEGIN;\n${body}\nCOMMIT;` : `${header}\n${body}`;
}

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
  /** PostgreSQL scripts get a header (and BEGIN / COMMIT in Open in query). */
  engine?: 'mysql' | 'postgres';
  /** Shows "Open in query", which hands the script to a new query tab. */
  onOpenInQuery?(sql: string): void;
  /** Shows the impacts aside (the table designer); false hides it (DDL edits, whose plan has none). */
  showImpacts?: boolean;
  /** How Copy and Open in query build the script (header, DELIMITER form). */
  script?: ScriptOptions;
}

/**
 * Read-only SQL with numbered lines (lines of statements that lose data tinted danger), Copy and
 * Open in query, and the change's impacts beside it.
 */
export function SqlPreview({ statements, impacts, error, live, engine = 'mysql', onOpenInQuery, showImpacts = true, script: options }: SqlPreviewProps) {
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const script = scriptOf(statements, engine, false, options);

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

  // Shown verbatim (the bytes that are sent); a rule between statements marks where each ends.
  let n = 0;
  const blocks = statements.map((s, si) => ({
    key: si,
    lines: s.split('\n').map((text, li) => ({ no: ++n, key: `${si}:${li}`, text, loss: losesData(text, impacts) })),
  }));
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
          <button type="button" className="btn small" disabled={statements.length === 0} onClick={() => onOpenInQuery(scriptOf(statements, engine, true, options))}>
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
          {blocks.length === 0 ? (
            <p className="dz-subtle dz-sql-empty">No changes to apply.</p>
          ) : (
            <div className="dz-sql" role="group" aria-label="SQL statements">
              {blocks.map((b) => (
                <div key={b.key} className="dz-sql-stmt">
                  {b.lines.map((l) => (
                    <div key={l.key} className={`dz-sql-line${l.loss ? ' dz-loss' : ''}`}>
                      <span className="dz-ln" aria-hidden="true">
                        {l.no}
                      </span>
                      <span className="dz-code">{l.text}</span>
                    </div>
                  ))}
                </div>
              ))}
            </div>
          )}
        </div>
        {showImpacts ? (
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
        ) : null}
      </div>
    </section>
  );
}
