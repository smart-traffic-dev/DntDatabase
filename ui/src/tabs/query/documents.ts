import type { Value } from '../../bindings';

/** One cell as relaxed Extended JSON. */
export function valueToEjson(v: Value): unknown {
  switch (v.t) {
    case 'Null':
      return null;
    case 'Bool':
    case 'Text':
    case 'Time':
      return v.v;
    case 'Int':
    case 'UInt':
      // Past 2^53 the value arrives as a string: keep it exact.
      return typeof v.v === 'number' ? v.v : { $numberLong: v.v };
    case 'Float':
      return Number.isFinite(v.v) ? v.v : { $numberDouble: String(v.v) };
    case 'Decimal':
      return { $numberDecimal: v.v };
    case 'Json':
      try {
        return JSON.parse(v.v);
      } catch {
        return v.v;
      }
    case 'Uuid':
      return { $uuid: v.v };
    case 'Bytes':
      // Only the preview bytes are in the cell.
      return { $binary: { base64: btoa(String.fromCharCode(...v.v.preview)), subType: '00' } };
    case 'Date':
    case 'DateTime':
    case 'TimestampTz':
      return { $date: v.v };
    case 'Array':
      return v.v.map(valueToEjson);
    case 'Other':
      return v.v.type_name === 'ObjectId' ? { $oid: v.v.text } : { [`$${v.v.type_name}`]: v.v.text };
  }
}

/**
 * A result row as a document keyed by column. `present[i] === false` marks a padding cell (the
 * document had no such field) and is left out. Without a mask every `Null` cell is left out: a
 * field that is really `null` cannot be told from padding, a known limitation of this view.
 */
export function rowToDocument(columns: string[], row: Value[], present: boolean[] | null): Record<string, unknown> {
  const doc: Record<string, unknown> = {};
  columns.forEach((name, i) => {
    const cell = row[i];
    if (!cell) return;
    if (present ? present[i] === false : cell.t === 'Null') return;
    doc[name] = valueToEjson(cell);
  });
  return doc;
}
