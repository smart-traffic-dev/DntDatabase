import { RefreshIcon } from '../components/icons';
import { useObjectData, type Resource, type SubTab } from '../store/objectData';
import type { ObjectTabRef as Tab } from '../store/tabs';

export const READ_ONLY_TIP = 'This connection is read-only. Change it in Edit connection.';

/**
 * Tables and views (incl. materialized) open on their Data (like Navicat), then Info, DDL and
 * Structure; other objects have DDL only.
 */
export function subTabsOf(kind: Tab['objectRef']['kind']): SubTab[] {
  return kind === 'Table' || kind === 'View' || kind === 'MaterializedView' ? ['data', 'info', 'ddl', 'structure'] : ['ddl'];
}

// Data also loads the (cached) detail: its row estimate is shown next to the page.
export const resourceOf = (sub: SubTab): Resource => (sub === 'ddl' ? 'ddl' : 'detail');

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
