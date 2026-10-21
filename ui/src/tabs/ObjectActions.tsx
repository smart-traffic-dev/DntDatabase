import { useRef } from 'react';
import { DatabaseIcon, LockIcon } from '../components/icons';
import { useConnections } from '../store/connections';
import type { ObjectTabRef as Tab } from '../store/tabs';
import { DataActions } from './DataView';
import { READ_ONLY_TIP, SectionRefresh, resourceOf, useSubTab } from './objectSections';
import { ENV_SHORT, useStripWidth } from './stripWidth';
import './query/toolbar.css';

/**
 * The object tab's action cluster at the right end of its group's tab strip (editor v2 §2):
 * the context chip `[ENV] database` (connection, server, object path and read-only in the
 * tooltip), then the visible section's actions: on Data, Filter, Refresh and `⋯`; on the other
 * sections, Refresh.
 */
export function ObjectActions({ tab }: { tab: Tab }) {
  const bar = useRef<HTMLDivElement>(null);
  const narrow = useStripWidth(bar) === 'narrow';
  const conn = useConnections((s) => s.summaries.get(tab.connectionId)?.config);
  const sub = useSubTab(tab);
  const env = conn?.env_tag ?? null;
  const { objectRef } = tab;
  const path = [objectRef.database, objectRef.schema, objectRef.name].filter(Boolean).join(' › ');
  const tip = [
    conn ? `${conn.name} (${conn.host}:${conn.port})${env ? ` · ${env}` : ''}` : null,
    path,
    conn?.read_only ? READ_ONLY_TIP : null,
  ]
    .filter(Boolean)
    .join('\n');
  return (
    <div className={`tab-actions${narrow ? ' w-narrow' : ' w-wide'}`} role="toolbar" aria-label="Object" ref={bar}>
      <span className="ta-chip ta-chip-static" data-testid="object-context" title={tip}>
        {env ? <span className={`qt-env env-${env}`}>{ENV_SHORT[env]}</span> : narrow ? <DatabaseIcon /> : null}
        {!narrow && <span className="ta-chip-text">{objectRef.database}</span>}
        {conn?.read_only ? (
          <span className="ta-chip-ro" role="img" aria-label="Read-only">
            <LockIcon />
          </span>
        ) : null}
      </span>
      {sub === 'data' ? <DataActions tab={tab} /> : <SectionRefresh tab={tab} resource={resourceOf(sub)} />}
    </div>
  );
}
