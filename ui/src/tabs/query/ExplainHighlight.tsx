import type { ReactNode } from 'react';

/** The filter substring inside `text`, marked for the tree and the grid. */
export function Highlight({ text, filter }: { text: string; filter: string }) {
  const needle = filter.trim().toLowerCase();
  if (needle.length === 0) return <>{text}</>;
  const hay = text.toLowerCase();
  const parts: ReactNode[] = [];
  let from = 0;
  let at = hay.indexOf(needle);
  while (at >= 0) {
    if (at > from) parts.push(text.slice(from, at));
    parts.push(
      <mark className="ex-mark" key={at}>
        {text.slice(at, at + needle.length)}
      </mark>,
    );
    from = at + needle.length;
    at = hay.indexOf(needle, from);
  }
  if (from < text.length) parts.push(text.slice(from));
  return <>{parts}</>;
}
