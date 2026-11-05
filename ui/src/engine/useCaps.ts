import type { Capabilities } from '../bindings';
import { useConnections } from '../store/connections';
import { capsOf } from './index';

/**
 * What a connection can do: the capabilities its server reports, else what its driver declares
 * (before the connection opens). `null` without a connection (or a deleted one).
 */
export function useCaps(connectionId: string | null): Capabilities | null {
  const kind = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config.kind : undefined));
  const info = useConnections((s) => (connectionId ? s.info.get(connectionId) : undefined));
  return kind ? capsOf(info, kind) : null;
}

/** `useCaps` for code outside React (menu builders, shortcuts); reads the stores once. */
export function capsOfConnection(connectionId: string | null | undefined): Capabilities | null {
  if (!connectionId) return null;
  const { summaries, info } = useConnections.getState();
  const kind = summaries.get(connectionId)?.config.kind;
  return kind ? capsOf(info.get(connectionId), kind) : null;
}
