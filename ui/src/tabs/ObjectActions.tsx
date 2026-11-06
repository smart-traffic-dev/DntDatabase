import { useRef } from 'react';
import { DatabaseIcon, DesignIcon, LockIcon } from '../components/icons';
import { openDesignTab } from '../components/openObject';
import { capsOfConnection } from '../engine/useCaps';
import { useConnections } from '../store/connections';
import type { ObjectTabRef as Tab } from '../store/tabs';
import { DataActions, DataPager } from './DataView';
import { ObjectSections, READ_ONLY_TIP, SectionRefresh, resourceOf, useRowEstimate, useSubTab } from './objectSections';
import { ContextMark } from '../components/EnvTag';
import { useStripWidth } from './stripWidth';
import './query/toolbar.css';

/**
 * The object tab's action cluster at the right end of its group's tab strip (editor v2 §2):
 * the context chip `[ENV] connection › database` (server, object path and read-only in the
 * tooltip), the sections (Data, Info, DDL, Structure), then at the right end the visible section's
 * actions: on Data, the pager, Filter, Refresh and `⋯`; on the other sections, Refresh.
 */
export function ObjectActions({ tab }: { tab: Tab }) {
  const bar = useRef<HTMLDivElement>(null);
  const narrow = useStripWidth(bar) === 'narrow';
  const conn = useConnections((s) => s.summaries.get(tab.connectionId)?.config);
  const sub = useSubTab(tab);
  const estimate = useRowEstimate(tab);
  const env = conn?.env_tag ?? null;
  const { objectRef } = tab;
  // F4 opens the designer too; the button makes it findable. Tables only, on an open connection.
  const connected = useConnections((s) => s.info.has(tab.connectionId));
  const designable = objectRef.kind === 'Table' && connected && (capsOfConnection(tab.connectionId)?.designer ?? false);
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
        <ContextMark env={env} kind={conn?.kind} />
        {!conn && !env && narrow ? <DatabaseIcon /> : null}
        {!narrow && (
          <>
            {conn && (
              <>
                <span className="ta-chip-conn">{conn.name}</span>
                <span className="ta-chip-sep" aria-hidden="true">
                  ›
                </span>
              </>
            )}
            <span className="ta-chip-text">{objectRef.schema ? `${objectRef.database} › ${objectRef.schema}` : objectRef.database}</span>
          </>
        )}
        {conn?.read_only ? (
          <span className="ta-chip-ro" role="img" aria-label="Read-only">
            <LockIcon />
          </span>
        ) : null}
      </span>
      <ObjectSections tab={tab} />
      <span className="ta-fill" />
      {designable && (
        <>
          <button
            type="button"
            className="ta-btn"
            aria-label="Design table"
            title="Design table (F4)"
            onClick={() => void openDesignTab(tab.connectionId, { database: objectRef.database, schema: objectRef.schema }, objectRef.name)}
          >
            <DesignIcon />
          </button>
          <span className="ta-sep" aria-hidden="true" />
        </>
      )}
      {sub === 'data' ? (
        <>
          <DataPager tab={tab} estimate={estimate} />
          <span className="ta-sep" aria-hidden="true" />
          <DataActions tab={tab} />
        </>
      ) : (
        <SectionRefresh tab={tab} resource={resourceOf(sub)} />
      )}
    </div>
  );
}
