import type { KeyType } from '../../bindings';
import { formatCount, plural } from '../../lib/plural';
import { useKeysTabs, type KeysTabState } from '../../store/keysTabs';
import { formatTtl, keyTypeLabel, sameKey } from './keyEdits';

/** The type filter: `All types`, then the types a scan can ask for. */
const TYPE_OPTIONS: { value: string; label: string; type: KeyType | null }[] = [
  { value: '', label: 'All types', type: null },
  { value: 'String', label: 'String', type: 'String' },
  { value: 'Hash', label: 'Hash', type: 'Hash' },
  { value: 'List', label: 'List', type: 'List' },
  { value: 'Set', label: 'Set', type: 'Set' },
  { value: 'ZSet', label: 'Sorted set', type: 'ZSet' },
  { value: 'Stream', label: 'Stream', type: 'Stream' },
  { value: 'Json', label: 'JSON', type: 'Json' },
];

const optionOf = (t: KeyType | null) => (t === null ? '' : typeof t === 'object' ? '' : t);

/** `N keys loaded`, or `N of 1,234 keys loaded` when the database's total is known. */
export function loadedText(loaded: number, total: number | null): string {
  return total === null ? `${plural(loaded, 'key')} loaded` : `${formatCount(loaded)} of ${plural(total, 'key')} loaded`;
}

interface Props {
  tabId: string;
  state: KeysTabState;
  /** The database's key count from the tree, when known. */
  total: number | null;
}

/** The left pane of the key browser: pattern, type filter, the scanned keys and `Load more`. */
export function KeyList({ tabId, state, total }: Props) {
  const store = useKeysTabs.getState();
  return (
    <section className="kt-list" aria-label="Key list">
      <div className="kt-filter">
        <input
          type="search"
          className="kt-pattern mono"
          aria-label="Key pattern"
          placeholder="Pattern, e.g. user:*"
          value={state.pattern}
          onChange={(e) => store.setPattern(tabId, e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              void store.rescan(tabId);
            }
          }}
        />
        <select
          aria-label="Key type"
          value={optionOf(state.keyType)}
          onChange={(e) => store.setKeyType(tabId, TYPE_OPTIONS.find((o) => o.value === e.target.value)?.type ?? null)}
        >
          {TYPE_OPTIONS.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </div>
      {state.error && !state.selected ? (
        <p role="alert" className="kt-error">
          {state.error}
        </p>
      ) : null}
      <ul className="kt-keys" role="listbox" aria-label="Keys">
        {state.keys.map((k) => {
          const selected = sameKey(state.selected, k.name);
          return (
            <li
              key={k.name.raw}
              role="option"
              aria-selected={selected}
              tabIndex={selected ? 0 : -1}
              className={`kt-key${selected ? ' on' : ''}`}
              data-testid="key-row"
              onClick={() => void store.select(tabId, k.name)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  void store.select(tabId, k.name);
                }
              }}
            >
              <span className="kt-key-name mono">{k.name.display}</span>
              <span className="kt-badge">{keyTypeLabel(k.key_type)}</span>
              <span className="kt-ttl">{formatTtl(k.ttl_ms)}</span>
            </li>
          );
        })}
      </ul>
      <footer className="kt-foot">
        <span className="kt-count" data-testid="keys-count">
          {state.scanning && state.keys.length === 0 ? 'Scanning…' : loadedText(state.keys.length, total)}
        </span>
        {state.cursor !== null ? (
          <button type="button" className="btn small" disabled={state.scanning} onClick={() => void store.loadMore(tabId)}>
            Load more
          </button>
        ) : null}
      </footer>
    </section>
  );
}
