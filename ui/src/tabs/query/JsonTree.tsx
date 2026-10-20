import { useState } from 'react';

/** A number whose digits a JS number cannot hold: kept as the source text. */
export class RawNumber {
  constructor(readonly raw: string) {}
}

/** Entries shown per object / array before "… N more". */
const MAX_CHILDREN = 200;

interface ReviverContext {
  source?: string;
}

/**
 * `JSON.parse` that keeps numbers a double would change (big integers, long decimals) as their
 * source text, where the engine exposes it; null when `text` is not JSON.
 */
export function parseJson(text: string): { value: unknown } | null {
  try {
    const value = (JSON.parse as (t: string, r?: (k: string, v: unknown, c: ReviverContext) => unknown) => unknown)(
      text,
      (_k, v, ctx) => {
        if (typeof v !== 'number' || ctx?.source === undefined) return v;
        // More digits than a double keeps exactly (about 15): show the source text.
        const digits = ctx.source.replace(/[eE].*$/, '').replace(/\D/g, '').replace(/^0+/, '').length;
        return digits > 15 ? new RawNumber(ctx.source) : v;
      },
    );
    return { value };
  } catch {
    return null;
  }
}

function Leaf({ value }: { value: unknown }) {
  if (value === null) return <span className="jt-null">null</span>;
  if (value instanceof RawNumber) return <span className="jt-num">{value.raw}</span>;
  switch (typeof value) {
    case 'string':
      return <span className="jt-str">{JSON.stringify(value)}</span>;
    case 'number':
      return <span className="jt-num">{String(value)}</span>;
    case 'boolean':
      return <span className="jt-bool">{String(value)}</span>;
    default:
      return <span>{String(value)}</span>;
  }
}

const isContainer = (v: unknown): v is object | unknown[] => typeof v === 'object' && v !== null && !(v instanceof RawNumber);

function Node({ name, value, depth }: { name: string | null; value: unknown; depth: number }) {
  const [open, setOpen] = useState(depth < 2);
  const label = name === null ? null : <span className="jt-key">{name}</span>;
  if (!isContainer(value)) {
    return (
      <div className="jt-row">
        {label}
        {label && ': '}
        <Leaf value={value} />
      </div>
    );
  }
  const isArray = Array.isArray(value);
  const entries: [string, unknown][] = isArray ? value.map((v, i) => [String(i), v]) : Object.entries(value);
  const summary = isArray ? `[${entries.length}]` : `{${entries.length}}`;
  return (
    <div className="jt-node">
      <button type="button" className="jt-toggle" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="jt-arrow" aria-hidden="true">
          {open ? '▾' : '▸'}
        </span>
        {label}
        {label && ': '}
        <span className="jt-sum">{summary}</span>
      </button>
      {open && (
        <div className="jt-children">
          {entries.slice(0, MAX_CHILDREN).map(([k, v]) => (
            <Node key={k} name={k} value={v} depth={depth + 1} />
          ))}
          {entries.length > MAX_CHILDREN && <div className="jt-more">… {entries.length - MAX_CHILDREN} more</div>}
        </div>
      )}
    </div>
  );
}

/** Collapsible tree of an already parsed JSON value (read-only). */
export function JsonTree({ value }: { value: unknown }) {
  return (
    <div className="jt" aria-label="JSON">
      <Node name={null} value={value} depth={0} />
    </div>
  );
}
