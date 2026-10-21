// The palette's "Recent" section: the ids of the commands run last, newest first. A per-user
// convenience kept in localStorage; storage that throws or holds junk just means no Recent.

const KEY = 'dnt.palette.recent';
/** How many recent commands are kept (and listed). */
export const MAX_RECENT = 5;

export function recentCommands(): string[] {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(KEY) ?? '[]');
    return Array.isArray(parsed) ? parsed.filter((id): id is string => typeof id === 'string').slice(0, MAX_RECENT) : [];
  } catch {
    return [];
  }
}

export function rememberCommand(id: string): void {
  try {
    const next = [id, ...recentCommands().filter((r) => r !== id)].slice(0, MAX_RECENT);
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {
    // Storage unavailable (private mode, quota): Recent stays as it was.
  }
}
