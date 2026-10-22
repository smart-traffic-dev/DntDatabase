import { useLayoutEffect, useState, type RefObject } from 'react';
import type { EnvTag } from '../bindings';

/** The environment badge of a strip's context chip. */
export const ENV_SHORT: Record<EnvTag, string> = { production: 'PROD', staging: 'STG', local: 'LCL' };

/** Under this group width the strip's action cluster keeps only the chip badge, Run / Stop and `⋯` (editor v2 §2). */
export const NARROW_GROUP_PX = 360;

export type StripWidth = 'wide' | 'narrow';

const widthOf = (px: number): StripWidth => (px > 0 && px < NARROW_GROUP_PX ? 'narrow' : 'wide');

/**
 * The width class of the editor group an action cluster sits in, measured on its tab strip
 * (the strip spans the group; the cluster itself is only as wide as its buttons).
 */
export function useStripWidth(ref: RefObject<HTMLElement | null>): StripWidth {
  const [width, setWidth] = useState<StripWidth>('wide');
  useLayoutEffect(() => {
    const el = ref.current?.closest<HTMLElement>('.tabstrip-toolbar') ?? ref.current;
    if (!el) return;
    setWidth(widthOf(el.getBoundingClientRect().width));
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box) setWidth(widthOf(box.width));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

/** Where a menu opened from `el` goes: under it, right-aligned with it (a button at the toolbar's right end). */
export const belowRight = (el: HTMLElement | null, menuWidth = 220) => {
  const r = el?.getBoundingClientRect();
  return { x: Math.max(4, (r?.right ?? 0) - menuWidth), y: (r?.bottom ?? 0) + 2 };
};

/** Under `el`, left-aligned with it (a button at the toolbar's left side). */
export const belowLeft = (el: HTMLElement | null) => {
  const r = el?.getBoundingClientRect();
  return { x: Math.max(4, r?.left ?? 0), y: (r?.bottom ?? 0) + 2 };
};
