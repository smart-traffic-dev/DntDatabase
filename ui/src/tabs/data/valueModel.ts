// What the value editor works with (pure): the content of a cell as text or bytes, hex, images
// recognised by their first bytes, and JSON pretty-printing that leaves every token as written.
import type { CellPayload, Value } from '../../bindings';
import { cellView } from '../query/format';
import { plainText } from '../query/gridCopy';

/** A cell's content: NULL, text, or binary data. */
export type Content = { kind: 'null' } | { kind: 'text'; text: string } | { kind: 'binary'; bytes: Uint8Array };

export function contentOfValue(v: Value): Content {
  switch (v.t) {
    case 'Null':
      return { kind: 'null' };
    case 'Bytes':
      return { kind: 'binary', bytes: Uint8Array.from(v.v.preview) };
    case 'Array':
      return { kind: 'text', text: cellView(v).text };
    default:
      return { kind: 'text', text: plainText(v) };
  }
}

export function contentOfPayload(p: CellPayload): Content {
  switch (p.kind) {
    case 'null':
      return { kind: 'null' };
    case 'text':
      return { kind: 'text', text: p.text };
    case 'base64':
      return { kind: 'binary', bytes: base64ToBytes(p.data) };
  }
}

export function bytesToBase64(bytes: Uint8Array): string {
  let out = '';
  for (let i = 0; i < bytes.length; i += 0x8000) out += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(out);
}

export function base64ToBytes(data: string): Uint8Array {
  const bin = atob(data);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Bytes as lowercase hex pairs separated by a space. */
export function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join(' ');
}

/** Hex pairs (any whitespace between them, optional `0x`) as bytes; null when it is not whole bytes of hex. */
export function parseHex(text: string): Uint8Array | null {
  const digits = text.replace(/\s+/g, '').replace(/^0x/i, '');
  if (digits.length % 2 !== 0 || !/^[0-9a-f]*$/i.test(digits)) return null;
  const out = new Uint8Array(digits.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(digits.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export type ImageType = 'png' | 'jpeg' | 'gif' | 'webp';

export const IMAGE_MIME: Record<ImageType, string> = {
  png: 'image/png',
  jpeg: 'image/jpeg',
  gif: 'image/gif',
  webp: 'image/webp',
};

const startsWith = (b: Uint8Array, sig: number[], at = 0) => sig.every((x, i) => b[at + i] === x);

/** The image format the bytes start with (PNG, JPEG, GIF, WebP), by their magic bytes. */
export function detectImage(b: Uint8Array): ImageType | null {
  if (startsWith(b, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return 'png';
  if (startsWith(b, [0xff, 0xd8, 0xff])) return 'jpeg';
  if (startsWith(b, [0x47, 0x49, 0x46, 0x38]) && (b[4] === 0x37 || b[4] === 0x39) && b[5] === 0x61) return 'gif';
  if (startsWith(b, [0x52, 0x49, 0x46, 0x46]) && startsWith(b, [0x57, 0x45, 0x42, 0x50], 8)) return 'webp';
  return null;
}

export type JsonResult = { ok: true; text: string } | { ok: false; message: string };

/** `text` checked as JSON (the parser's message when it is not). */
export function checkJson(text: string): { ok: true } | { ok: false; message: string } {
  try {
    JSON.parse(text);
    return { ok: true };
  } catch (e) {
    return { ok: false, message: e instanceof Error ? e.message : String(e) };
  }
}

/**
 * `text` indented by two spaces. Strings and numbers are copied as written (a number with 20 digits
 * stays exact); only the white space between tokens changes.
 */
export function formatJson(text: string): JsonResult {
  const checked = checkJson(text);
  if (!checked.ok) return checked;
  let out = '';
  let depth = 0;
  const pad = () => '  '.repeat(depth);
  const nextSignificant = (from: number) => {
    let j = from;
    while (j < text.length && /\s/.test(text[j]!)) j++;
    return j;
  };
  let i = 0;
  while (i < text.length) {
    const c = text[i]!;
    if (/\s/.test(c)) {
      i++;
    } else if (c === '"') {
      let j = i + 1;
      while (text[j] !== '"') j += text[j] === '\\' ? 2 : 1;
      out += text.slice(i, j + 1);
      i = j + 1;
    } else if (c === '{' || c === '[') {
      const close = c === '{' ? '}' : ']';
      const j = nextSignificant(i + 1);
      if (text[j] === close) {
        out += c + close;
        i = j + 1;
      } else {
        depth++;
        out += `${c}\n${pad()}`;
        i++;
      }
    } else if (c === '}' || c === ']') {
      depth--;
      out += `\n${pad()}${c}`;
      i++;
    } else if (c === ',') {
      out += `,\n${pad()}`;
      i++;
    } else if (c === ':') {
      out += ': ';
      i++;
    } else {
      let j = i;
      while (j < text.length && !/[\s,:\]}]/.test(text[j]!)) j++;
      out += text.slice(i, j);
      i = j;
    }
  }
  return { ok: true, text: out };
}
