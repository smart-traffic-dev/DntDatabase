import { useEffect, useState } from 'react';

/** `Date.now()`, ticking once a second while `active` (restarted when `key` changes). */
export function useNow(active: boolean, key: number | null): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [active, key]);
  return now;
}
