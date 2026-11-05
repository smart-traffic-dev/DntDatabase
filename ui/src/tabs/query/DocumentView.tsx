import { useEffect, useMemo, useState } from 'react';
import type { Value } from '../../bindings';
import { api } from '../../api';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import { rowToDocument } from './documents';
import { writeClipboard } from './ResultGrid';

/** Documents listed at most; a longer result is cut here (the Table view shows every row). */
export const MAX_DOCUMENTS = 500;

/** The first `MAX_DOCUMENTS` rows of `result` (a stored result is read from the backend). */
function useRows(result: ResultSet): { rows: Value[][]; error: string | null } {
  const [stored, setStored] = useState<{ token: string; rows: Value[][] } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const token = result.token;
  const wanted = Math.min(result.rowCount, MAX_DOCUMENTS);
  useEffect(() => {
    if (!token) return;
    let current = true;
    void (async () => {
      let rows: Value[][] = [];
      try {
        while (rows.length < wanted) {
          const page = await api.queryResultPage(token, rows.length, wanted - rows.length);
          if (page.rows.length === 0) break;
          rows = rows.concat(page.rows);
        }
        if (current) {
          setError(null);
          setStored({ token, rows });
        }
      } catch (e) {
        if (current) setError(e instanceof Error ? e.message : 'Could not load result rows');
      }
    })();
    return () => {
      current = false;
    };
  }, [token, wanted]);
  return useMemo(() => {
    if (token) return { rows: stored?.token === token ? stored.rows : [], error };
    const rows: Value[][] = [];
    for (let at = 0; at < wanted; at++) {
      const row = result.chunks[Math.floor(at / CHUNK)]?.[at % CHUNK];
      if (row) rows.push(row);
    }
    return { rows, error: null };
  }, [token, stored, error, result.chunks, wanted]);
}

/**
 * The rows of a document result as JSON documents (2-space indent, read-only), one block each with
 * its own copy button. Run events carry no padding mask, so a `Null` cell is left out of its document.
 */
export function DocumentView({ result }: { result: ResultSet }) {
  const { rows, error } = useRows(result);
  const [copied, setCopied] = useState<number | null>(null);
  const names = useMemo(() => result.columns.map((c) => c.name), [result.columns]);
  const texts = useMemo(() => rows.map((row) => JSON.stringify(rowToDocument(names, row, null), null, 2)), [rows, names]);
  return (
    <div className="qr-docs" data-testid="document-view">
      {error && <div className="qr-banner warning" role="status">{error}</div>}
      {texts.map((text, i) => (
        <section key={i} className="qr-doc" data-testid="document" aria-label={`Document ${i + 1}`}>
          <button
            type="button"
            className="gt-btn qr-doc-copy"
            aria-label={`Copy document ${i + 1}`}
            title="Copy this document"
            onClick={() => {
              void writeClipboard(text).then((ok) => setCopied(ok ? i : null));
            }}
          >
            {copied === i ? 'Copied' : 'Copy'}
          </button>
          <pre className="rv-text">{text}</pre>
        </section>
      ))}
      {result.rowCount > MAX_DOCUMENTS && (
        <div className="qr-doc-more">Showing the first {MAX_DOCUMENTS} documents; the Table view lists every row.</div>
      )}
    </div>
  );
}
