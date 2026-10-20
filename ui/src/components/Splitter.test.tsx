import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { Splitter } from './Splitter';

function Harness({ orientation = 'horizontal', start = 200, onChange }: { orientation?: 'horizontal' | 'vertical'; start?: number; onChange?: (v: number) => void }) {
  const [value, setValue] = useState(start);
  return (
    <Splitter
      orientation={orientation}
      label="Resize"
      value={value}
      min={100}
      max={400}
      defaultValue={250}
      onChange={(v) => {
        setValue(v);
        onChange?.(v);
      }}
    />
  );
}

const bar = () => screen.getByRole('separator', { name: 'Resize' });

afterEach(() => {
  document.body.style.cursor = '';
  document.body.style.userSelect = '';
});

describe('Splitter', () => {
  it('exposes separator semantics', () => {
    render(<Harness />);
    expect(bar()).toHaveAttribute('aria-orientation', 'horizontal');
    expect(bar()).toHaveAttribute('aria-valuenow', '200');
    expect(bar()).toHaveAttribute('aria-valuemin', '100');
    expect(bar()).toHaveAttribute('aria-valuemax', '400');
    expect(bar()).toHaveAttribute('tabindex', '0');
  });

  it('arrow keys step 16 px (Shift 64 px) and clamp', () => {
    render(<Harness />);
    fireEvent.keyDown(bar(), { key: 'ArrowDown' });
    expect(bar()).toHaveAttribute('aria-valuenow', '216');
    fireEvent.keyDown(bar(), { key: 'ArrowUp' });
    fireEvent.keyDown(bar(), { key: 'ArrowUp' });
    expect(bar()).toHaveAttribute('aria-valuenow', '184');
    fireEvent.keyDown(bar(), { key: 'ArrowDown', shiftKey: true });
    expect(bar()).toHaveAttribute('aria-valuenow', '248');
    for (let i = 0; i < 5; i++) fireEvent.keyDown(bar(), { key: 'ArrowDown', shiftKey: true });
    expect(bar()).toHaveAttribute('aria-valuenow', '400');
  });

  it('a vertical splitter answers Left / Right, not Up / Down', () => {
    render(<Harness orientation="vertical" />);
    fireEvent.keyDown(bar(), { key: 'ArrowDown' });
    expect(bar()).toHaveAttribute('aria-valuenow', '200');
    fireEvent.keyDown(bar(), { key: 'ArrowRight' });
    expect(bar()).toHaveAttribute('aria-valuenow', '216');
    fireEvent.keyDown(bar(), { key: 'ArrowLeft', shiftKey: true });
    expect(bar()).toHaveAttribute('aria-valuenow', '152');
  });

  it('Home and End jump to the limits', () => {
    render(<Harness />);
    fireEvent.keyDown(bar(), { key: 'Home' });
    expect(bar()).toHaveAttribute('aria-valuenow', '100');
    fireEvent.keyDown(bar(), { key: 'End' });
    expect(bar()).toHaveAttribute('aria-valuenow', '400');
  });

  it('double-click resets to the default', () => {
    render(<Harness start={120} />);
    fireEvent.doubleClick(bar());
    expect(bar()).toHaveAttribute('aria-valuenow', '250');
  });

  it('pointer drag reports the new size, clamped, and sets the body cursor while dragging', () => {
    const onChange = vi.fn();
    render(<Harness onChange={onChange} />);
    const el = bar();
    el.setPointerCapture = vi.fn();
    el.releasePointerCapture = vi.fn();
    el.hasPointerCapture = () => true;
    fireEvent.pointerDown(el, { button: 0, clientY: 300, pointerId: 1 });
    expect(el.setPointerCapture).toHaveBeenCalledWith(1);
    expect(document.body.style.cursor).toBe('row-resize');
    fireEvent.pointerMove(el, { clientY: 340, pointerId: 1 });
    expect(onChange).toHaveBeenLastCalledWith(240);
    fireEvent.pointerMove(el, { clientY: 900, pointerId: 1 });
    expect(onChange).toHaveBeenLastCalledWith(400);
    fireEvent.pointerUp(el, { pointerId: 1 });
    expect(document.body.style.cursor).toBe('');
    expect(document.body.style.userSelect).toBe('');
    onChange.mockClear();
    fireEvent.pointerMove(el, { clientY: 100, pointerId: 1 });
    expect(onChange).not.toHaveBeenCalled();
  });

  it('a vertical drag follows the x axis', () => {
    const onChange = vi.fn();
    render(<Harness orientation="vertical" onChange={onChange} />);
    const el = bar();
    el.setPointerCapture = vi.fn();
    fireEvent.pointerDown(el, { button: 0, clientX: 100, pointerId: 1 });
    expect(document.body.style.cursor).toBe('col-resize');
    fireEvent.pointerMove(el, { clientX: 70, pointerId: 1 });
    expect(onChange).toHaveBeenLastCalledWith(170);
    fireEvent.pointerUp(el, { pointerId: 1 });
  });
});
