import type { BinaryEncoding, EmptyAs, ImportMode, KeyChoice } from '../../../bindings';
import { DEFAULT_MAX_ERRORS } from '../importState';
import { IssueList } from './MappingStep';
import type { StepProps } from './stepProps';

/** The key an upsert starts with: the primary key, else the first usable unique index. */
export function defaultKey(keys: KeyChoice[]): KeyChoice | null {
  return keys.find((k) => k.primary) ?? keys[0] ?? null;
}

const sameColumns = (a: string[], b: string[]) => a.length === b.length && a.every((c, i) => c === b[i]);

/** A whole number from an input, or `fallback` when it is not one ≥ `min`. */
const whole = (text: string, min: number, fallback: number) => {
  const n = Number(text);
  return Number.isInteger(n) && n >= min ? n : fallback;
};

/** Step 4: mode, error policy, conversion and batch options. */
export function OptionsStep({ state, dispatch }: StepProps) {
  const target = state.target;
  if (!target) return null;
  const { mode, errors, convert } = state;
  const setMode = (m: ImportMode) => dispatch({ type: 'mode', mode: m });
  const key = mode.kind === 'upsert' ? target.keys.find((k) => sameColumns(k.columns, mode.key)) ?? null : null;
  const hasIdentity = target.server.kind === 'postgres' && target.columns.some((c) => c.identity !== null);

  return (
    <div className="stack">
      <fieldset className="import-wizard-fieldset">
        <legend>Rows already in the table</legend>
        <label className="check import-wizard-check">
          <input type="radio" name="iw-mode" checked={mode.kind === 'append'} onChange={() => setMode({ kind: 'append' })} />
          <span>Append</span>
        </label>
        <label className="check import-wizard-check">
          <input type="radio" name="iw-mode" checked={mode.kind === 'replace'} onChange={() => setMode({ kind: 'replace', truncate: false })} />
          <span>Replace all</span>
        </label>
        {mode.kind === 'replace' && (
          <div className="import-wizard-sub" role="group" aria-label="How rows are removed">
            <label className="check import-wizard-check">
              <input type="radio" name="iw-replace" checked={!mode.truncate} onChange={() => setMode({ kind: 'replace', truncate: false })} />
              <span>Delete rows</span>
            </label>
            <label className="check import-wizard-check">
              <input type="radio" name="iw-replace" checked={mode.truncate} onChange={() => setMode({ kind: 'replace', truncate: true })} />
              <span>Truncate</span>
            </label>
          </div>
        )}
        <label className="check import-wizard-check">
          <input
            type="radio"
            name="iw-mode"
            checked={mode.kind === 'upsert'}
            onChange={() => setMode({ kind: 'upsert', key: defaultKey(target.keys)?.columns ?? [] })}
          />
          <span>Upsert</span>
        </label>
        {mode.kind === 'upsert' && (
          <div className="import-wizard-sub">
            {target.keys.length === 0 ? (
              <p className="import-wizard-note">This table has no primary key or NOT NULL unique index: an upsert is not possible.</p>
            ) : (
              <div className="field">
                <label htmlFor="iw-key">Upsert key</label>
                <select
                  id="iw-key"
                  value={key?.name ?? ''}
                  onChange={(e) => setMode({ kind: 'upsert', key: target.keys.find((k) => k.name === e.target.value)?.columns ?? [] })}
                >
                  {!key && <option value="">Choose…</option>}
                  {target.keys.map((k) => (
                    <option key={k.name} value={k.name}>
                      {k.name} ({k.columns.join(', ')}){k.primary ? ' · primary key' : ''}
                    </option>
                  ))}
                </select>
              </div>
            )}
          </div>
        )}
      </fieldset>
      <IssueList issues={state.report?.errors ?? []} tone="error" />

      <fieldset className="import-wizard-fieldset">
        <legend>When a row is refused</legend>
        <label className="check import-wizard-check">
          <input type="radio" name="iw-errors" checked={errors.kind === 'stop'} onChange={() => dispatch({ type: 'errors', errors: { kind: 'stop' } })} />
          <span>Stop the import</span>
        </label>
        <label className="check import-wizard-check">
          <input
            type="radio"
            name="iw-errors"
            checked={errors.kind === 'skip'}
            onChange={() => dispatch({ type: 'errors', errors: { kind: 'skip', max_errors: DEFAULT_MAX_ERRORS } })}
          />
          <span>Skip and log the row</span>
        </label>
        {errors.kind === 'skip' && (
          <div className="import-wizard-sub field">
            <label htmlFor="iw-max-errors">Stop after this many refused rows</label>
            <input
              id="iw-max-errors"
              type="number"
              min={1}
              value={errors.max_errors}
              onChange={(e) => dispatch({ type: 'errors', errors: { kind: 'skip', max_errors: whole(e.target.value, 1, errors.max_errors) } })}
            />
          </div>
        )}
      </fieldset>

      <div className="import-wizard-options">
        <div className="field">
          <label htmlFor="iw-empty">Empty text cells</label>
          <select id="iw-empty" value={convert.empty} onChange={(e) => dispatch({ type: 'convert', patch: { empty: e.target.value as EmptyAs } })}>
            <option value="null">NULL</option>
            <option value="empty_string">Empty string</option>
          </select>
        </div>
        <div className="field">
          <label htmlFor="iw-null-text">NULL text</label>
          <input
            id="iw-null-text"
            placeholder="None (e.g. \N or NULL)"
            value={convert.null_text ?? ''}
            onChange={(e) => dispatch({ type: 'convert', patch: { null_text: e.target.value === '' ? null : e.target.value } })}
          />
        </div>
        <div className="field">
          <label htmlFor="iw-binary">Binary columns</label>
          <select id="iw-binary" value={convert.binary} onChange={(e) => dispatch({ type: 'convert', patch: { binary: e.target.value as BinaryEncoding } })}>
            <option value="hex">Hex (0x… or bare hex)</option>
            <option value="base_64">Base64</option>
            <option value="utf_8_text">UTF-8 text</option>
          </select>
        </div>
        <label className="check import-wizard-check">
          <input type="checkbox" checked={convert.trim} onChange={(e) => dispatch({ type: 'convert', patch: { trim: e.target.checked } })} />
          <span>Trim spaces around text</span>
        </label>
      </div>

      <div className="import-wizard-options">
        <div className="field">
          <label htmlFor="iw-batch-rows">Rows per batch</label>
          <input
            id="iw-batch-rows"
            type="number"
            min={1}
            max={100000}
            value={state.batchRows}
            onChange={(e) => dispatch({ type: 'batch', rows: whole(e.target.value, 1, state.batchRows), bytes: state.batchBytes })}
          />
        </div>
        <div className="field">
          <label htmlFor="iw-batch-kib">Batch size (KiB)</label>
          <input
            id="iw-batch-kib"
            type="number"
            min={1}
            max={65536}
            value={Math.round(state.batchBytes / 1024)}
            onChange={(e) => dispatch({ type: 'batch', rows: state.batchRows, bytes: whole(e.target.value, 1, state.batchBytes / 1024) * 1024 })}
          />
        </div>
        {hasIdentity && (
          <label className="check import-wizard-check">
            <input type="checkbox" checked={state.overrideIdentity} onChange={(e) => dispatch({ type: 'overrideIdentity', value: e.target.checked })} />
            <span>Override identity (write identity ALWAYS columns)</span>
          </label>
        )}
      </div>
    </div>
  );
}
