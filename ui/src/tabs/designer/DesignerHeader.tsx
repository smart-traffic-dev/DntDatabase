import { useRef, type KeyboardEvent } from 'react';
import type { DesignSubTab } from '../../store/tableDesigns';

export const SUB_TABS: { id: DesignSubTab; label: string }[] = [
  { id: 'columns', label: 'Columns' },
  { id: 'indexes', label: 'Indexes' },
  { id: 'foreign_keys', label: 'Foreign keys' },
  { id: 'checks', label: 'Checks' },
  { id: 'options', label: 'Options' },
  { id: 'comment', label: 'Comment' },
];

export const subTabId = (uid: string, sub: DesignSubTab) => `${uid}-tab-${sub}`;
export const subPanelId = (uid: string) => `${uid}-panel`;

export interface DesignerHeaderProps {
  uid: string;
  name: string;
  /** `connection › database · engine` */
  context: string;
  counts: Partial<Record<DesignSubTab, number>>;
  /** Sections the server cannot do: disabled, the text is their tooltip. */
  unavailable?: Partial<Record<DesignSubTab, string>>;
  subTab: DesignSubTab;
  onSubTab(sub: DesignSubTab): void;
  changes: number;
  dirty: boolean;
  canDiscard: boolean;
  canReview: boolean;
  onDiscard(): void;
  onReview(): void;
}

/** One row: the table's name and where it lives, the section switch, the unsaved chip, Discard and Review. */
export function DesignerHeader(p: DesignerHeaderProps) {
  const refs = useRef<Partial<Record<DesignSubTab, HTMLButtonElement | null>>>({});
  const select = (sub: DesignSubTab) => {
    p.onSubTab(sub);
    refs.current[sub]?.focus();
  };
  const onKeyDown = (e: KeyboardEvent) => {
    const ids = SUB_TABS.map((s) => s.id).filter((id) => !p.unavailable?.[id] || id === p.subTab);
    const at = ids.indexOf(p.subTab);
    const target =
      e.key === 'ArrowRight' ? ids[(at + 1) % ids.length]
      : e.key === 'ArrowLeft' ? ids[(at - 1 + ids.length) % ids.length]
      : e.key === 'Home' ? ids[0]
      : e.key === 'End' ? ids[ids.length - 1]
      : undefined;
    if (!target) return;
    e.preventDefault();
    select(target);
  };

  return (
    <div className="dz-header">
      <strong className="dz-title">{p.name}</strong>
      {p.context ? <span className="dz-subtle">{p.context}</span> : null}
      <div role="tablist" aria-label="Table design sections" className="segmented dz-sections" onKeyDown={onKeyDown}>
        {SUB_TABS.map((s) => {
          const on = s.id === p.subTab;
          const count = p.counts[s.id];
          const unavailable = p.unavailable?.[s.id];
          return (
            <button
              key={s.id}
              type="button"
              role="tab"
              id={subTabId(p.uid, s.id)}
              aria-selected={on}
              aria-controls={subPanelId(p.uid)}
              disabled={!!unavailable}
              title={unavailable}
              tabIndex={on ? 0 : -1}
              className={`segment${on ? ' on' : ''}`}
              ref={(el) => {
                refs.current[s.id] = el;
              }}
              onClick={() => select(s.id)}
            >
              {s.label}
              {count !== undefined ? <em className="dz-count">{count}</em> : null}
            </button>
          );
        })}
      </div>
      <span className="dz-spacer" />
      {p.dirty ? (
        <span className="dz-chip">{p.changes === 0 ? 'Unsaved changes' : p.changes === 1 ? '1 unsaved change' : `${p.changes} unsaved changes`}</span>
      ) : null}
      <button type="button" className="btn dz-btn" disabled={!p.canDiscard} onClick={p.onDiscard}>
        Discard
      </button>
      <button type="button" className="btn primary dz-btn" disabled={!p.canReview} onClick={p.onReview}>
        Review &amp; apply…
      </button>
    </div>
  );
}
