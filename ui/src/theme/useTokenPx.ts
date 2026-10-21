import { useEffect, useState } from 'react';

function read(name: string, fallback: number): number {
  const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue(name));
  return Number.isFinite(v) && v > 0 ? v : fallback;
}

/**
 * A px size token (e.g. `--dnt-row-h-tree`) as a number, for code that needs the value itself
 * (virtualized row heights, canvas grids). Follows Settings → Density: re-read when `<html>`'s
 * `data-density` changes.
 */
export function useTokenPx(name: string, fallback: number): number {
  const [px, setPx] = useState(() => read(name, fallback));
  useEffect(() => {
    setPx(read(name, fallback));
    const observer = new MutationObserver(() => setPx(read(name, fallback)));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-density'] });
    return () => observer.disconnect();
  }, [name, fallback]);
  return px;
}
