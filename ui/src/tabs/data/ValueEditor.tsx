import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { api, toAppError } from '../../api';
import type { CellInput, ColumnMeta, ObjectRef, Value } from '../../bindings';
import {
  IMAGE_MIME,
  bytesToBase64,
  checkJson,
  contentOfPayload,
  contentOfValue,
  detectImage,
  formatJson,
  parseHex,
  toHex,
  type Content,
} from './valueModel';
import './valueEditor.css';

type TabName = 'text' | 'json' | 'hex' | 'image';
const TAB_LABEL: Record<TabName, string> = { text: 'Text', json: 'JSON', hex: 'Hex', image: 'Image' };

/** Longest binary value the Hex tab shows (and edits); longer values are viewed in part and saved from a file. */
export const HEX_LIMIT = 256 * 1024;
/** Largest image previewed. */
const IMAGE_LIMIT = 8 * 1024 * 1024;

const NO_BYTES = new Uint8Array();
const textOf = (c: Content) => (c.kind === 'text' ? c.text : '');
const bytesOf = (c: Content) => (c.kind === 'binary' ? c.bytes : NO_BYTES);
const hexOf = (bytes: Uint8Array) => toHex(bytes.subarray(0, HEX_LIMIT));

export interface ValueEditorProps {
  connectionId: string;
  object: ObjectRef;
  column: ColumnMeta;
  /** The cell's value on the page (cut short when `truncated`). */
  value: Value;
  /** The page cut the value: the complete one is fetched by `fetchKey` before anything can be edited. */
  truncated: boolean;
  /** The row's key, to fetch the complete value; null when the row has none. */
  fetchKey: [string, Value][] | null;
  /** Whether the value can be changed (else it is only viewed). */
  editable: boolean;
  /** Stages `input`; `token` stands for the complete original when it was fetched (the row's conflict check uses it). */
  onStage(input: CellInput, token: string | null): void;
  onClose(): void;
}

/**
 * The value of one cell in a panel: Text (character count, wraps), JSON (checked, formatted) for text,
 * Hex and Image (PNG, JPEG, GIF and WebP by their first bytes) for binary data. A value the page cut
 * short is fetched in full first. Values can come from / go to a file; Save stages the change.
 */
export function ValueEditor({ connectionId, object, column, value, truncated, fetchKey, editable, onStage, onClose }: ValueEditorProps) {
  const jsonColumn = column.logical === 'Json';
  const binaryColumn = column.logical === 'Binary';
  const modeOf = (c: Content) => (c.kind === 'binary' || binaryColumn ? 'binary' : 'text');
  const defaultTab = (c: Content): TabName =>
    modeOf(c) === 'binary' ? (detectImage(bytesOf(c)) ? 'image' : 'hex') : jsonColumn ? 'json' : 'text';

  const [origin, setOrigin] = useState<Content>(() => contentOfValue(value));
  const [mode, setMode] = useState<'text' | 'binary'>(() => modeOf(origin));
  const [text, setText] = useState(() => textOf(origin));
  const [bytes, setBytes] = useState(() => bytesOf(origin));
  const [hex, setHex] = useState(() => hexOf(bytesOf(origin)));
  const [tab, setTab] = useState<TabName>(() => defaultTab(origin));
  const [phase, setPhase] = useState<'loading' | 'ready'>(truncated ? 'loading' : 'ready');
  const [error, setError] = useState<string | null>(null);
  /** The token of the complete original (fetched), and of a value loaded from a file. */
  const [fetched, setFetched] = useState<string | null>(null);
  const [loadedToken, setLoadedToken] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const live = useRef(true);

  const show = (c: Content) => {
    setMode(modeOf(c));
    setText(textOf(c));
    setBytes(bytesOf(c));
    setHex(hexOf(bytesOf(c)));
    setTab(defaultTab(c));
  };

  useEffect(() => {
    live.current = true;
    if (truncated) {
      if (!fetchKey) {
        setError('This row has no key, so its full value cannot be read.');
      } else {
        api.dataCellFetch(connectionId, object, fetchKey, column.name).then(
          (cell) => {
            if (!live.current) return;
            const c = contentOfPayload(cell.payload);
            setOrigin(c);
            show(c);
            setFetched(cell.token);
            setPhase('ready');
          },
          (e) => {
            if (live.current) setError(toAppError(e).message);
          },
        );
      }
    }
    return () => {
      live.current = false;
    };
    // The panel is mounted afresh for each cell.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const originText = textOf(origin);
  const originHex = hexOf(bytesOf(origin));
  const dirty = loadedToken !== null || (mode === 'text' ? text !== originText : hex !== originHex);
  const parsedHex = useMemo(() => (mode === 'binary' && tab === 'hex' && hex.length <= HEX_LIMIT * 3 ? parseHex(hex) : NO_BYTES), [mode, tab, hex]);
  const hexProblem = mode === 'binary' && parsedHex === null ? 'Hex: enter pairs of digits 0-9 and a-f.' : null;
  const jsonCheck = useMemo(() => (mode === 'text' && (tab === 'json' || (jsonColumn && dirty)) ? checkJson(text) : { ok: true as const }), [mode, tab, jsonColumn, dirty, text]);
  const jsonProblem = jsonCheck.ok ? null : `Invalid JSON: ${jsonCheck.message}`;
  const ready = phase === 'ready';
  const readOnly = !editable || !ready;
  const hexReadOnly = readOnly || bytes.length > HEX_LIMIT;
  const canSave = editable && ready && dirty && !hexProblem && !(jsonColumn && jsonProblem);
  const tabs: TabName[] = mode === 'text' ? ['text', 'json'] : ['hex', 'image'];

  const type = detectImage(bytes);
  const chars = useMemo(() => (mode === 'text' && tab !== 'hex' ? Array.from(text).length : 0), [mode, tab, text]);

  const edited = (next: string) => {
    setText(next);
    setLoadedToken(null);
  };
  const editedHex = (next: string) => {
    setHex(next);
    setLoadedToken(null);
    const b = parseHex(next);
    if (b) setBytes(b);
  };

  const guarded = async (run: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await run();
    } catch (e) {
      if (live.current) setError(toAppError(e).message);
    } finally {
      if (live.current) setBusy(false);
    }
  };

  const loadFile = () =>
    guarded(async () => {
      const path = await api.filePickOpen('cell_value');
      if (!path) return;
      const cell = await api.dataCellLoadFile(connectionId, object, column.name, path);
      if (!live.current) return;
      show(contentOfPayload(cell.payload));
      setLoadedToken(cell.token);
    });

  const saveFile = () =>
    guarded(async () => {
      const ext = mode === 'binary' ? (type ?? 'bin') : jsonColumn ? 'json' : 'txt';
      const path = await api.filePickSave('cell_value', `${column.name}.${ext === 'jpeg' ? 'jpg' : ext}`);
      if (!path) return;
      let token = loadedToken ?? fetched;
      if (!token && fetchKey) token = (await api.dataCellFetch(connectionId, object, fetchKey, column.name)).token;
      if (!token) return;
      await api.dataCellSaveFile(token, path);
    });

  const save = () => {
    let input: CellInput;
    if (loadedToken) input = { kind: 'loaded', token: loadedToken };
    else if (mode === 'text') input = { kind: 'text', text };
    else {
      const b = parseHex(hex);
      if (!b) return;
      input = { kind: 'base64', data: bytesToBase64(b) };
    }
    onStage(input, fetched);
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.stopPropagation();
      onClose();
    }
  };

  const nullable = column.nullable !== false;
  const canSaveFile = ready && (loadedToken !== null || ((fetched !== null || fetchKey !== null) && !dirty));
  const imageSrc = type && bytes.length <= IMAGE_LIMIT ? `data:${IMAGE_MIME[type]};base64,${bytesToBase64(bytes)}` : null;

  return (
    <section className="ve" aria-label="Value editor" onKeyDown={onKeyDown}>
      <header className="ve-head">
        <span className="ve-title">
          {column.name} <span className="muted">{column.db_type}</span>
        </span>
        <button type="button" className="qt-btn" aria-label="Close value editor" onClick={onClose}>
          Close
        </button>
      </header>
      <div role="tablist" aria-label="View" className="ve-tabs">
        {tabs.map((t) => (
          <button key={t} type="button" role="tab" aria-selected={tab === t} className={`ve-tab${tab === t ? ' ve-on' : ''}`} onClick={() => setTab(t)}>
            {TAB_LABEL[t]}
          </button>
        ))}
      </div>
      <div className="ve-body">
        {phase === 'loading' && !error ? <p className="ve-note muted">Loading the full value…</p> : null}
        {tab === 'text' || tab === 'json' ? (
          <>
            {tab === 'json' ? (
              <div className="ve-tools">
                <button
                  type="button"
                  className="qt-btn"
                  disabled={readOnly || !jsonCheck.ok}
                  onClick={() => {
                    const r = formatJson(text);
                    if (r.ok) edited(r.text);
                  }}
                >
                  Format
                </button>
              </div>
            ) : null}
            <textarea
              className="ve-text"
              aria-label="Value"
              spellCheck={false}
              readOnly={readOnly}
              value={text}
              onChange={(e) => edited(e.target.value)}
            />
            <p className="ve-note muted">{chars.toLocaleString('en-US')} characters</p>
          </>
        ) : null}
        {tab === 'hex' ? (
          <>
            <textarea
              className="ve-text ve-hex"
              aria-label="Hex"
              spellCheck={false}
              readOnly={hexReadOnly}
              value={hex}
              onChange={(e) => editedHex(e.target.value)}
            />
            <p className="ve-note muted">
              {bytes.length.toLocaleString('en-US')} bytes
              {bytes.length > HEX_LIMIT ? `; the first ${HEX_LIMIT.toLocaleString('en-US')} are shown. Use Save to file to get all of it.` : ''}
            </p>
          </>
        ) : null}
        {tab === 'image' ? (
          imageSrc ? (
            <div className="ve-image">
              <img src={imageSrc} alt="Image preview" />
            </div>
          ) : (
            <p className="ve-note muted">{type ? 'The image is too large to preview.' : 'This value is not a PNG, JPEG, GIF or WebP image.'}</p>
          )
        ) : null}
        {error ? (
          <p role="alert" className="ve-error">
            {error}
          </p>
        ) : null}
        {!error && hexProblem ? (
          <p role="alert" className="ve-error">
            {hexProblem}
          </p>
        ) : null}
        {!error && !hexProblem && jsonProblem ? (
          <p role="alert" className="ve-error">
            {jsonProblem}
          </p>
        ) : null}
      </div>
      <footer className="ve-actions">
        {editable ? (
          <>
            <button type="button" className="qt-btn" disabled={busy || !ready} onClick={() => void loadFile()}>
              Load from file…
            </button>
            {nullable ? (
              <button type="button" className="qt-btn" disabled={!ready} onClick={() => onStage({ kind: 'null' }, fetched)}>
                Set NULL
              </button>
            ) : null}
          </>
        ) : null}
        <button type="button" className="qt-btn" disabled={busy || !canSaveFile} onClick={() => void saveFile()}>
          Save to file…
        </button>
        <span className="spacer" />
        {editable ? (
          <button type="button" className="qt-btn qt-primary" disabled={!canSave} onClick={save}>
            Save
          </button>
        ) : null}
      </footer>
    </section>
  );
}

