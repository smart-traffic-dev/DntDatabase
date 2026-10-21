import { useMemo, type ReactNode } from 'react';

/** The raw plan is JSON (PostgreSQL, MySQL / MariaDB FORMAT=JSON), not text (EXPLAIN ANALYZE). */
export function isJson(raw: string): boolean {
  const t = raw.trim();
  if (!t.startsWith('{') && !t.startsWith('[')) return false;
  try {
    JSON.parse(t);
    return true;
  } catch {
    return false;
  }
}

/** Indented JSON, or the text unchanged when it is not JSON. */
export function prettyPlan(raw: string): string {
  return isJson(raw) ? JSON.stringify(JSON.parse(raw.trim()), null, 2) : raw;
}

// Keys (a string before `:`), strings, numbers, literals.
const TOKEN = /("(?:[^"\\]|\\.)*")(\s*:)?|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|\b(true|false|null)\b/g;

/** Colours of a pretty-printed JSON text. */
function colour(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  let from = 0;
  for (const m of text.matchAll(TOKEN)) {
    if (m.index > from) out.push(text.slice(from, m.index));
    const [whole, str, colon, n, lit] = m;
    if (str !== undefined) {
      out.push(
        <span key={m.index} className={colon ? 'ex-jk' : 'ex-js'}>
          {str}
        </span>,
      );
      if (colon) out.push(colon);
    } else if (n !== undefined) {
      out.push(
        <span key={m.index} className="ex-jn">
          {n}
        </span>,
      );
    } else if (lit !== undefined) {
      out.push(
        <span key={m.index} className="ex-jn">
          {lit}
        </span>,
      );
    }
    from = m.index + whole.length;
  }
  if (from < text.length) out.push(text.slice(from));
  return out;
}

/** The engine's output: JSON indented and coloured, other text as it came. */
export function ExplainJson({ raw }: { raw: string }) {
  const json = isJson(raw);
  const text = useMemo(() => prettyPlan(raw), [raw]);
  const body = useMemo(() => (json ? colour(text) : text), [json, text]);
  return (
    <pre className="ex-raw" data-testid="explain-raw">
      {body}
    </pre>
  );
}
