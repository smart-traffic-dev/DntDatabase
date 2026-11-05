import type { KeyBytes, KeyEdit, KeyName, KeyType, Value } from '../../bindings';

// Building `KeyEdit` values (spec §7.6): what the user types is text, sent as UTF-8 bytes in base64;
// the Hex mode of a string value is parsed as hex pairs.

/** Standard base64 of `bytes`. */
export function bytesToBase64(bytes: Uint8Array): KeyBytes {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

/** The bytes of a standard base64 string. */
export function base64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

/** `text` as UTF-8, in base64. */
export const textBytes = (text: string): KeyBytes => bytesToBase64(new TextEncoder().encode(text));

/** Hex pairs (whitespace ignored, `0x` prefixes allowed) as bytes; null when it is not valid hex. */
export function parseHex(text: string): Uint8Array | null {
  const clean = text.replace(/0x/gi, '').replace(/\s+/g, '');
  if (clean.length % 2 !== 0 || /[^0-9a-f]/i.test(clean)) return null;
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** `bytes` as space-separated hex pairs. */
export const toHex = (bytes: Uint8Array): string => [...bytes].map((b) => b.toString(16).padStart(2, '0')).join(' ');

/** A key named by text the user typed. */
export const keyNameOf = (text: string): KeyName => ({ display: text, raw: textBytes(text) });

/** The bytes a value of a key stands for; null when only a preview of them is known. */
export function valueBytes(v: Value): Uint8Array | null {
  switch (v.t) {
    case 'Null':
      return new Uint8Array();
    case 'Bytes':
      return v.v.preview.length === v.v.len ? Uint8Array.from(v.v.preview) : null;
    case 'Bool':
    case 'Int':
    case 'UInt':
    case 'Float':
    case 'Decimal':
    case 'Text':
    case 'Json':
    case 'Uuid':
    case 'Date':
    case 'Time':
    case 'DateTime':
    case 'TimestampTz':
      return new TextEncoder().encode(String(v.v));
    case 'Array':
      return new TextEncoder().encode(JSON.stringify(v.v));
    case 'Other':
      return new TextEncoder().encode(v.v.text);
  }
}

/** Whether `bytes` are valid UTF-8 (else Text / JSON would show replacement characters). */
export function isUtf8(bytes: Uint8Array): boolean {
  try {
    new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    return true;
  } catch {
    return false;
  }
}

/**
 * Whether a value can be edited as text and saved back without loss: its bytes are known in full
 * and, for raw bytes, are UTF-8 (else the edit would save replacement characters).
 */
export function editableText(v: Value): boolean {
  const bytes = valueBytes(v);
  return bytes !== null && (v.t !== 'Bytes' || isUtf8(bytes));
}

/** `valueBytes` in base64 (what an edit of an existing field / member sends). */
export function valueKeyBytes(v: Value): KeyBytes | null {
  const bytes = valueBytes(v);
  return bytes ? bytesToBase64(bytes) : null;
}

/** A value as the text the pane shows. */
export function valueText(v: Value): string {
  switch (v.t) {
    case 'Null':
      return '';
    case 'Bytes': {
      const bytes = Uint8Array.from(v.v.preview);
      const text = new TextDecoder('utf-8', { fatal: false }).decode(bytes);
      return v.v.preview.length === v.v.len ? text : `${text}…`;
    }
    case 'Array':
      return JSON.stringify(v.v);
    case 'Other':
      return v.v.text;
    default:
      return String(v.v);
  }
}

/** The types a new key can have, as the toolbar's `New key…` dialog lists them. */
export type NewKeyType = 'String' | 'Hash' | 'List' | 'Set' | 'ZSet';

/** The first value of a new key. `field` is the hash field; `score` the sorted-set score. */
export interface NewKey {
  name: string;
  type: NewKeyType;
  value: string;
  field: string;
  score: number;
}

/** The edit that creates `k`: one write that also sets its first value. */
export function newKeyEdit(k: NewKey): KeyEdit {
  const key = keyNameOf(k.name);
  switch (k.type) {
    case 'String':
      return { op: 'set_string', key, value: textBytes(k.value) };
    case 'Hash':
      return { op: 'hash_set', key, field: textBytes(k.field), value: textBytes(k.value) };
    case 'List':
      return { op: 'list_push', key, value: textBytes(k.value), head: false };
    case 'Set':
      return { op: 'set_add', key, member: textBytes(k.value) };
    case 'ZSet':
      return { op: 'z_set_add', key, member: textBytes(k.value), score: k.score };
  }
}

/** Two keys are the same when their bytes are (never by `display`). */
export const sameKey = (a: KeyName | null | undefined, b: KeyName | null | undefined): boolean => !!a && !!b && a.raw === b.raw;

/** The label of a key type (badges, the type filter). */
export function keyTypeLabel(t: KeyType): string {
  if (typeof t === 'object') return t.Other;
  switch (t) {
    case 'String':
      return 'String';
    case 'Hash':
      return 'Hash';
    case 'List':
      return 'List';
    case 'Set':
      return 'Set';
    case 'ZSet':
      return 'Sorted set';
    case 'Stream':
      return 'Stream';
    case 'Json':
      return 'JSON';
  }
}

/** A TTL in words: `no TTL`, else at most two units (`12m 5s`, `3h 1m`, `2d 4h`). */
export function formatTtl(ms: number | null): string {
  if (ms === null) return 'no TTL';
  let s = Math.max(0, Math.ceil(ms / 1000));
  const parts: string[] = [];
  for (const [unit, size] of [
    ['d', 86400],
    ['h', 3600],
    ['m', 60],
    ['s', 1],
  ] as const) {
    const n = Math.floor(s / size);
    s -= n * size;
    if (n > 0 || (unit === 's' && parts.length === 0)) parts.push(`${n}${unit}`);
  }
  return parts.slice(0, 2).join(' ');
}
