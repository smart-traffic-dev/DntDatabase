import { useRef, useState, type KeyboardEvent, type PointerEvent } from 'react';
import './splitter.css';

export const SPLITTER_STEP = 16;
export const SPLITTER_BIG_STEP = 64;

interface Props {
  /** `vertical`: a vertical line between side-by-side panes (drag left / right). `horizontal`: between stacked panes. */
  orientation: 'vertical' | 'horizontal';
  /** Current size (px) of the pane before the line (left of it / above it). */
  value: number;
  min: number;
  max: number;
  /** Size restored by a double-click. */
  defaultValue: number;
  onChange(value: number): void;
  label: string;
}

const clamp = (n: number, min: number, max: number) => Math.min(max, Math.max(min, n));

/**
 * A draggable divider after a pane (its right / bottom edge). Pointer drag (captured, so it
 * follows the pointer outside the bar), arrow keys (16 px, Shift 64 px), Home / End and a
 * double-click reset.
 */
export function Splitter({ orientation, value, min, max, defaultValue, onChange, label }: Props) {
  const [dragging, setDragging] = useState(false);
  const start = useRef<{ pointer: number; value: number } | null>(null);
  const vertical = orientation === 'vertical';
  const axis = (e: { clientX: number; clientY: number }) => (vertical ? e.clientX : e.clientY);
  const change = (next: number) => onChange(clamp(next, min, max));

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture?.(e.pointerId);
    start.current = { pointer: axis(e), value };
    setDragging(true);
    document.body.style.cursor = vertical ? 'col-resize' : 'row-resize';
    document.body.style.userSelect = 'none';
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const s = start.current;
    if (s) change(s.value + axis(e) - s.pointer);
  };
  const end = (e: PointerEvent<HTMLDivElement>) => {
    if (!start.current) return;
    start.current = null;
    setDragging(false);
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
    if (e.currentTarget.hasPointerCapture?.(e.pointerId)) e.currentTarget.releasePointerCapture?.(e.pointerId);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? SPLITTER_BIG_STEP : SPLITTER_STEP;
    const [less, more] = vertical ? ['ArrowLeft', 'ArrowRight'] : ['ArrowUp', 'ArrowDown'];
    if (e.key === less) change(value - step);
    else if (e.key === more) change(value + step);
    else if (e.key === 'Home') change(min);
    else if (e.key === 'End') change(max);
    else return;
    e.preventDefault();
  };

  return (
    <div
      role="separator"
      aria-label={label}
      aria-orientation={orientation}
      aria-valuenow={Math.round(value)}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      className={`splitter ${orientation}${dragging ? ' dragging' : ''}`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onLostPointerCapture={end}
      onKeyDown={onKeyDown}
      onDoubleClick={() => onChange(clamp(defaultValue, min, max))}
    />
  );
}
