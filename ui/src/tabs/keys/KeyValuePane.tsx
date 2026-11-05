import { useState, type KeyboardEvent } from 'react';
import type { KeyEdit, KeyName, KeyValue, Value } from '../../bindings';
import { formatBytes } from '../../jobs/jobFormat';
import { useKeysTabs, valueCounts, type KeysTabState } from '../../store/keysTabs';
import { KeyPromptDialog } from './KeyDialogs';
import {
  bytesToBase64,
  formatTtl,
  keyNameOf,
  keyTypeLabel,
  parseHex,
  sameKey,
  textBytes,
  toHex,
  valueBytes,
  valueKeyBytes,
  valueText,
} from './keyEdits';

/** What the selected key's value is, in words (the header badge). */
function valueTypeLabel(v: KeyValue): string {
  if ('String' in v) return 'String';
  if ('Hash' in v) return 'Hash';
  if ('List' in v) return 'List';
  if ('Set' in v) return 'Set';
  if ('ZSet' in v) return 'Sorted set';
  if ('Stream' in v) return 'Stream';
  return v.Other.type_name;
}

type Edit = (edit: KeyEdit) => void;

/** One text input with an action button; Enter submits. */
function AddRow({ label, buttons }: { label: string; buttons: { label: string; run(text: string): void }[] }) {
  const [text, setText] = useState('');
  const run = (go: (t: string) => void) => {
    go(text);
    setText('');
  };
  return (
    <div className="kt-add">
      <input
        aria-label={label}
        placeholder={label}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && buttons[0]) {
            e.preventDefault();
            run(buttons[0].run);
          }
        }}
      />
      {buttons.map((b) => (
        <button key={b.label} type="button" className="btn small" onClick={() => run(b.run)}>
          {b.label}
        </button>
      ))}
    </div>
  );
}

/** A cell that turns into an input on `Edit`; Enter saves, Escape cancels. */
function EditableCell({ name, value, editable, pad, onSave }: { name: string; value: Value; editable: boolean; pad?: boolean; onSave(text: string): void }) {
  const [draft, setDraft] = useState<string | null>(null);
  if (draft === null || !editable) {
    return (
      <>
        <td className="mono kt-cell">{valueText(value)}</td>
        {editable ? (
          <td className="kt-actions">
            <button type="button" className="btn small" aria-label={`Edit ${name}`} onClick={() => setDraft(valueText(value))}>
              Edit
            </button>
          </td>
        ) : pad ? (
          // A value known only by its preview: nothing to edit, the row keeps its columns.
          <td className="kt-actions" />
        ) : null}
      </>
    );
  }
  const keys = (e: KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      onSave(draft);
      setDraft(null);
    } else if (e.key === 'Escape') {
      e.stopPropagation();
      setDraft(null);
    }
  };
  return (
    <>
      <td className="kt-cell">
        {/* The input replaces the text the user asked to edit, so it takes the focus. */}
        <input className="mono" aria-label={`Value of ${name}`} autoFocus value={draft} onChange={(e) => setDraft(e.target.value)} onKeyDown={keys} />
      </td>
      <td className="kt-actions">
        <button
          type="button"
          className="btn small primary"
          onClick={() => {
            onSave(draft);
            setDraft(null);
          }}
        >
          Save
        </button>
        <button type="button" className="btn small" onClick={() => setDraft(null)}>
          Cancel
        </button>
      </td>
    </>
  );
}

type Mode = 'text' | 'json' | 'hex';

const asText = (bytes: Uint8Array) => new TextDecoder().decode(bytes);
function render(bytes: Uint8Array, mode: Mode): string {
  if (mode === 'hex') return toHex(bytes);
  const text = asText(bytes);
  if (mode === 'json') {
    try {
      return JSON.stringify(JSON.parse(text), null, 2);
    } catch {
      return text;
    }
  }
  return text;
}

/** Whether `bytes` are valid UTF-8 (else Text / JSON would show replacement characters). */
function isUtf8(bytes: Uint8Array): boolean {
  try {
    new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    return true;
  } catch {
    return false;
  }
}

/**
 * A string value: Text / JSON / Hex modes; `Save` sends `set_string`. Until the draft is edited
 * it stands for the exact bytes of the value (a mode switch never goes through a lossy decode);
 * bytes that are not UTF-8 open in Hex. A value known only by its preview cannot be saved.
 */
function StringEditor({ keyName, value, editable: canEdit, edit }: { keyName: KeyName; value: Value; editable: boolean; edit: Edit }) {
  const exact = valueBytes(value);
  const original = exact ?? new TextEncoder().encode(valueText(value));
  const editable = canEdit && exact !== null;
  const [mode, setMode] = useState<Mode>(() => (exact && !isUtf8(exact) ? 'hex' : 'text'));
  const [draft, setDraft] = useState(() => render(original, exact && !isUtf8(exact) ? 'hex' : 'text'));
  const [edited, setEdited] = useState(false);
  const bytes = !edited ? original : mode === 'hex' ? parseHex(draft) : new TextEncoder().encode(draft);
  const switchTo = (next: Mode) => {
    setDraft(render(bytes ?? original, next));
    setMode(next);
  };
  return (
    <div className="kt-string">
      <div className="kt-modes" role="group" aria-label="View as">
        {(['text', 'json', 'hex'] as const).map((m) => (
          <button key={m} type="button" className="btn small" aria-pressed={mode === m} onClick={() => switchTo(m)}>
            {m === 'text' ? 'Text' : m === 'json' ? 'JSON' : 'Hex'}
          </button>
        ))}
      </div>
      <textarea
        className="kt-text mono"
        aria-label="String value"
        readOnly={!editable}
        value={draft}
        spellCheck={false}
        onChange={(e) => {
          setDraft(e.target.value);
          setEdited(true);
        }}
      />
      {canEdit && exact === null ? <p className="hint">Only the start of this value was loaded, so it cannot be edited here.</p> : null}
      {editable ? (
        <div className="kt-save">
          {bytes === null ? (
            <span role="alert" className="kt-invalid">
              Invalid hex
            </span>
          ) : null}
          <button
            type="button"
            className="btn small primary"
            disabled={bytes === null}
            onClick={() => bytes && edit({ op: 'set_string', key: keyName, value: bytesToBase64(bytes) })}
          >
            Save
          </button>
        </div>
      ) : null}
    </div>
  );
}

/** The value of the selected key, by its type (spec §7.6). */
function ValueBody({ keyName, value, editable, edit }: { keyName: KeyName; value: KeyValue; editable: boolean; edit: Edit }) {
  if ('String' in value) {
    // A new value (after Save or Refresh) starts the editor again.
    return <StringEditor key={JSON.stringify(value.String.value)} keyName={keyName} value={value.String.value} editable={editable} edit={edit} />;
  }
  if ('Hash' in value) {
    return (
      <>
        <table className="kt-grid" aria-label="Fields">
          <thead>
            <tr>
              <th>field</th>
              <th>value</th>
              {editable ? <th aria-label="Actions" colSpan={2} /> : null}
            </tr>
          </thead>
          <tbody>
            {value.Hash.fields.map(([f, v]) => {
              const field = valueKeyBytes(f);
              const name = valueText(f);
              return (
                <tr key={JSON.stringify(f)}>
                  <td className="mono">{name}</td>
                  <EditableCell
                    key={JSON.stringify(v)}
                    name={name}
                    value={v}
                    editable={editable && field !== null && valueBytes(v) !== null}
                    pad={editable}
                    onSave={(text) => field && edit({ op: 'hash_set', key: keyName, field, value: textBytes(text) })}
                  />
                  {editable && field !== null ? (
                    <td className="kt-actions">
                      <button type="button" className="btn small" aria-label={`Delete ${name}`} onClick={() => edit({ op: 'hash_delete', key: keyName, field })}>
                        Delete
                      </button>
                    </td>
                  ) : editable ? (
                    <td className="kt-actions" />
                  ) : null}
                </tr>
              );
            })}
          </tbody>
        </table>
        {editable ? <AddField onAdd={(field, text) => edit({ op: 'hash_set', key: keyName, field: textBytes(field), value: textBytes(text) })} /> : null}
      </>
    );
  }
  if ('List' in value) {
    return (
      <>
        <table className="kt-grid" aria-label="Items">
          <thead>
            <tr>
              <th>index</th>
              <th>value</th>
              {editable ? <th aria-label="Actions" colSpan={2} /> : null}
            </tr>
          </thead>
          <tbody>
            {value.List.items.map((v, index) => {
              const bytes = valueKeyBytes(v);
              return (
                <tr key={`${index}:${JSON.stringify(v)}`}>
                  <td className="mono">{index}</td>
                  <EditableCell name={`item ${index}`} value={v} editable={editable && bytes !== null} pad={editable} onSave={(text) => edit({ op: 'list_set', key: keyName, index, value: textBytes(text) })} />
                  {editable && bytes !== null ? (
                    <td className="kt-actions">
                      <button type="button" className="btn small" aria-label={`Remove item ${index}`} onClick={() => edit({ op: 'list_remove', key: keyName, value: bytes, count: 1 })}>
                        Remove
                      </button>
                    </td>
                  ) : editable ? (
                    <td className="kt-actions" />
                  ) : null}
                </tr>
              );
            })}
          </tbody>
        </table>
        {editable ? (
          <AddRow
            label="New item"
            buttons={[
              { label: 'Push head', run: (t) => edit({ op: 'list_push', key: keyName, value: textBytes(t), head: true }) },
              { label: 'Push tail', run: (t) => edit({ op: 'list_push', key: keyName, value: textBytes(t), head: false }) },
            ]}
          />
        ) : null}
      </>
    );
  }
  if ('Set' in value) {
    return (
      <>
        <ul className="kt-members" aria-label="Members">
          {value.Set.members.map((m) => {
            const member = valueKeyBytes(m);
            const name = valueText(m);
            return (
              <li key={JSON.stringify(m)}>
                <span className="mono">{name}</span>
                {editable && member !== null ? (
                  <button type="button" className="btn small" aria-label={`Remove ${name}`} onClick={() => edit({ op: 'set_remove', key: keyName, member })}>
                    Remove
                  </button>
                ) : null}
              </li>
            );
          })}
        </ul>
        {editable ? <AddRow label="New member" buttons={[{ label: 'Add member', run: (t) => edit({ op: 'set_add', key: keyName, member: textBytes(t) }) }]} /> : null}
      </>
    );
  }
  if ('ZSet' in value) {
    return (
      <>
        <table className="kt-grid" aria-label="Members">
          <thead>
            <tr>
              <th>member</th>
              <th>score</th>
              {editable ? <th aria-label="Actions" /> : null}
            </tr>
          </thead>
          <tbody>
            {value.ZSet.members.map(([m, score]) => {
              const member = valueKeyBytes(m);
              const name = valueText(m);
              return (
                <tr key={JSON.stringify(m)}>
                  <td className="mono">{name}</td>
                  <td className="mono">{score}</td>
                  {editable && member !== null ? (
                    <td className="kt-actions">
                      <button type="button" className="btn small" aria-label={`Remove ${name}`} onClick={() => edit({ op: 'z_set_remove', key: keyName, member })}>
                        Remove
                      </button>
                    </td>
                  ) : editable ? (
                    <td />
                  ) : null}
                </tr>
              );
            })}
          </tbody>
        </table>
        {editable ? <AddScored onAdd={(member, score) => edit({ op: 'z_set_add', key: keyName, member: textBytes(member), score })} /> : null}
      </>
    );
  }
  if ('Stream' in value) {
    return (
      <table className="kt-grid" aria-label="Entries">
        <thead>
          <tr>
            <th>id</th>
            <th>fields</th>
          </tr>
        </thead>
        <tbody>
          {value.Stream.entries.map((e) => (
            <tr key={e.id}>
              <td className="mono">{e.id}</td>
              <td className="mono">{e.fields.map(([f, v]) => `${valueText(f)}=${valueText(v)}`).join(', ')}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }
  return <pre className="kt-other mono">{value.Other.text}</pre>;
}

function AddField({ onAdd }: { onAdd(field: string, value: string): void }) {
  const [field, setField] = useState('');
  const [text, setText] = useState('');
  return (
    <div className="kt-add">
      <input aria-label="New field" placeholder="Field" value={field} onChange={(e) => setField(e.target.value)} />
      <input aria-label="New value" placeholder="Value" value={text} onChange={(e) => setText(e.target.value)} />
      <button
        type="button"
        className="btn small"
        disabled={field === ''}
        onClick={() => {
          onAdd(field, text);
          setField('');
          setText('');
        }}
      >
        Add field
      </button>
    </div>
  );
}

function AddScored({ onAdd }: { onAdd(member: string, score: number): void }) {
  const [member, setMember] = useState('');
  const [score, setScore] = useState('');
  const ok = member !== '' && score.trim() !== '' && Number.isFinite(Number(score));
  return (
    <div className="kt-add">
      <input aria-label="New member" placeholder="Member" value={member} onChange={(e) => setMember(e.target.value)} />
      <input aria-label="Score" placeholder="Score" inputMode="decimal" value={score} onChange={(e) => setScore(e.target.value)} />
      <button
        type="button"
        className="btn small"
        disabled={!ok}
        onClick={() => {
          onAdd(member, Number(score));
          setMember('');
          setScore('');
        }}
      >
        Add member
      </button>
    </div>
  );
}

const ttlProblem = (text: string) => (/^\d+$/.test(text.trim()) && Number(text) > 0 ? null : 'Enter a whole number of seconds above 0');

/** The right pane of the key browser: the selected key's header and value. */
export function KeyValuePane({ tabId, state, editable }: { tabId: string; state: KeysTabState; editable: boolean }) {
  const [prompt, setPrompt] = useState<'ttl' | 'rename' | null>(null);
  const store = useKeysTabs.getState();
  const { selected, value } = state;
  const edit: Edit = (e) => void store.preview(tabId, e);
  if (!selected) {
    return (
      <section className="kt-value kt-empty" aria-label="Value">
        {state.notice ? (
          <p role="status" className="kt-notice">
            {state.notice}
          </p>
        ) : (
          <p className="muted">Select a key to see its value.</p>
        )}
      </section>
    );
  }
  const summary = state.keys.find((k) => sameKey(k.name, selected));
  const typeLabel = summary ? keyTypeLabel(summary.key_type) : value ? valueTypeLabel(value.value) : '';
  const counts = value ? valueCounts(value.value) : null;
  const ttl = value ? value.ttl_ms : (summary?.ttl_ms ?? null);
  return (
    <section className="kt-value" aria-label="Value">
      <header className="kt-head" role="toolbar" aria-label="Key">
        <h2 className="kt-title mono" title={selected.display}>
          {selected.display}
        </h2>
        {typeLabel ? <span className="kt-badge">{typeLabel}</span> : null}
        <span className="kt-meta" data-testid="key-ttl">
          TTL: {formatTtl(ttl)}
        </span>
        {editable ? (
          <button type="button" className="btn small" onClick={() => setPrompt('ttl')}>
            Set TTL…
          </button>
        ) : null}
        {editable && ttl !== null ? (
          <button type="button" className="btn small" onClick={() => edit({ op: 'expire', key: selected, ttl_ms: null })}>
            Persist
          </button>
        ) : null}
        {value?.memory_bytes != null ? <span className="kt-meta">{formatBytes(value.memory_bytes)}</span> : null}
        <span className="kt-spacer" />
        {editable ? (
          <>
            <button type="button" className="btn small" onClick={() => setPrompt('rename')}>
              Rename…
            </button>
            <button type="button" className="btn small danger" onClick={() => edit({ op: 'delete', key: selected })}>
              Delete key
            </button>
          </>
        ) : null}
        <button type="button" className="btn small" onClick={() => void store.refresh(tabId)}>
          Refresh
        </button>
      </header>
      {state.error ? (
        <p role="alert" className="kt-error">
          {state.error}
        </p>
      ) : null}
      <div className="kt-value-body">
        {value ? <ValueBody keyName={selected} value={value.value} editable={editable} edit={edit} /> : state.loadingValue ? <p className="muted">Loading…</p> : null}
        {counts && counts.total > counts.loaded ? (
          <button type="button" className="btn small kt-more" disabled={state.loadingValue} onClick={() => void store.loadMoreValue(tabId)}>
            Load more
          </button>
        ) : null}
      </div>
      {prompt === 'ttl' ? (
        <KeyPromptDialog
          title="Set TTL"
          label="TTL in seconds"
          initial={ttl === null ? '' : String(Math.ceil(ttl / 1000))}
          submitLabel="Set TTL"
          validate={ttlProblem}
          onCancel={() => setPrompt(null)}
          onSubmit={(text) => {
            setPrompt(null);
            edit({ op: 'expire', key: selected, ttl_ms: Number(text.trim()) * 1000 });
          }}
        />
      ) : prompt === 'rename' ? (
        <KeyPromptDialog
          title="Rename key"
          label="New name"
          initial={selected.display}
          submitLabel="Rename"
          validate={(text) => (text === '' ? 'Enter a name' : text === selected.display ? 'Enter a different name' : null)}
          onCancel={() => setPrompt(null)}
          onSubmit={(text) => {
            setPrompt(null);
            edit({ op: 'rename', key: selected, to: keyNameOf(text) });
          }}
        />
      ) : null}
    </section>
  );
}
