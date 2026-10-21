import type { Tab } from '../store/tabs';

/**
 * The action cluster at the right end of a group's tab strip, for that group's active tab
 * (editor v2 §2): the query or object tab's controls live here instead of a toolbar row.
 */
export function TabStripActions({ tab }: { groupId: string; tab: Tab | undefined }) {
  if (!tab) return null;
  return null;
}
