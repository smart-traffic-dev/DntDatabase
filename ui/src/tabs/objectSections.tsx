import { useRef, type KeyboardEvent } from 'react';
import type { TableDetail } from '../bindings';
import { RefreshIcon } from '../components/icons';
import { useObjectData, type Resource, type SubTab } from '../store/objectData';
import type { ObjectTabRef as Tab } from '../store/tabs';

export const READ_ONLY_TIP = 'This connection is read-only. Change it in Edit connection.';

const SUB_LABEL: Record<SubTab, string> = { data: 'Data', info: 'Info', ddl: 'DDL', structure: 'Structure' };

/** DOM ids of an object tab's section tab and of its panel (the panel is in ObjectTab, the tabs in the toolbar row). */
const domKey = (tabId: string) => encodeURIComponent(tabId);
export const sectionTabId = (tabId: string, sub: SubTab) => `ot-${domKey(tabId)}-tab-${sub}`;
export const sectionPanelId = (tabId: string) => `ot-${domKey(tabId)}-panel`;

/**
 * The object's sections (Data, Info, DDL, Structure) as flat text tabs at the left of the tab's
 * toolbar row, so the content starts right under it. One tab stop; arrows / Home / End switch.
 */
export function ObjectSections({ tab }: { tab: Tab }) {
  const subs = subTabsOf(tab.objectRef.kind);
  const sub = useSubTab(tab);
  const refs = useRef<Partial<Record<SubTab, HTMLButtonElement | null>>>({});
  const select = (next: SubTab) => {
    useObjectData.getState().setSub(tab.id, next);
    refs.current[next]?.focus();
  };
  const onKeyDown = (e: KeyboardEvent) => {
    const at = subs.indexOf(sub);
    const target =
      e.key === 'ArrowRight' ? subs[(at + 1) % subs.length]
      : e.key === 'ArrowLeft' ? subs[(at - 1 + subs.length) % subs.length]
      : e.key === 'Home' ? subs[0]
      : e.key === 'End' ? subs[subs.length - 1]
      : undefined;
    if (!target) return;
    e.preventDefault();
    select(target);
  };
  return (
    <div role="tablist" aria-label="Object sections" className="ot-tabs" onKeyDown={onKeyDown}>
      {subs.map((s) => (
        <button
          key={s}
          type="button"
          role="tab"
          id={sectionTabId(tab.id, s)}
          aria-selected={s === sub}
          aria-controls={sectionPanelId(tab.id)}
          tabIndex={s === sub ? 0 : -1}
          className={`ot-subtab${s === sub ? ' on' : ''}`}
          data-testid={s === 'ddl' ? 'object-tab-ddl' : undefined}
          ref={(el) => {
            refs.current[s] = el;
          }}
          onClick={() => select(s)}
        >
          {SUB_LABEL[s]}
        </button>
      ))}
    </div>
  );
}

/**
 * Tables and views (incl. materialized) open on their Data (like Navicat), then Info, DDL and
 * Structure; other objects have DDL only.
 */
export function subTabsOf(kind: Tab['objectRef']['kind']): SubTab[] {
  return kind === 'Table' || kind === 'View' || kind === 'MaterializedView' ? ['data', 'info', 'ddl', 'structure'] : ['ddl'];
}

// Data also loads the (cached) detail: its row estimate is shown next to the page.
export const resourceOf = (sub: SubTab): Resource => (sub === 'ddl' ? 'ddl' : 'detail');

/** The row estimate shown beside the Data pager: the object's cached detail has it (views have none). */
export function useRowEstimate(tab: Tab): number | null {
  return useObjectData((s) => {
    const detail = s.byTab[tab.id]?.detail;
    return detail?.status === 'ok' ? (detail.data as TableDetail).row_estimate : null;
  });
}

/** The visible sub-tab of an object tab (the stored one when the object has it, else its first). */
export function useSubTab(tab: Tab): SubTab {
  const subs = subTabsOf(tab.objectRef.kind);
  const stored = useObjectData((s) => s.byTab[tab.id]?.sub);
  return stored && subs.includes(stored) ? stored : subs[0]!;
}

/**
 * Reloads the visible section of an object tab (Info, DDL, Structure: its metadata / DDL); busy
 * while that section loads. It sits in the group's tab strip (editor v2 §2).
 */
export function SectionRefresh({ tab, resource }: { tab: Tab; resource: Resource }) {
  const busy = useObjectData((s) => s.byTab[tab.id]?.[resource]?.status === 'loading');
  return (
    <button
      type="button"
      className="ta-btn"
      aria-label={busy ? 'Refreshing…' : 'Refresh'}
      title={busy ? 'Loading this section…' : 'Reload this section'}
      disabled={busy}
      aria-busy={busy}
      onClick={() => void useObjectData.getState().refresh(tab, resource)}
    >
      <RefreshIcon />
    </button>
  );
}
