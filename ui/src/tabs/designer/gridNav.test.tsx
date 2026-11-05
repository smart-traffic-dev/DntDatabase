import { render } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { onGridNavKey } from './gridNav';

function Grid() {
  const row = (n: number, locked = false) => (
    <div role="row" key={n} tabIndex={-1} data-row={n}>
      <div role="cell" />
      <div role="cell">
        <input aria-label={`name${n}`} defaultValue={`col${n}`} disabled={locked} />
      </div>
      <div role="cell">
        <input aria-label={`type${n}`} defaultValue="int" />
      </div>
      <div role="cell">
        <input type="checkbox" aria-label={`null${n}`} />
      </div>
    </div>
  );
  return (
    <div className="dz-grid" role="table" onKeyDown={onGridNavKey}>
      <div role="row" className="dz-head">
        <div role="columnheader" />
      </div>
      {row(0)}
      {row(1, true)}
      {row(2)}
    </div>
  );
}

describe('designer grid arrow keys', () => {
  it('↑ / ↓ go to the same field of the row above / below, with its text selected', async () => {
    const user = userEvent.setup();
    const { getByLabelText } = render(<Grid />);
    await user.click(getByLabelText('type0'));
    await user.keyboard('{ArrowDown}');
    expect(document.activeElement).toBe(getByLabelText('type1'));
    await user.keyboard('{ArrowDown}');
    const t2 = getByLabelText('type2') as HTMLInputElement;
    expect(document.activeElement).toBe(t2);
    expect([t2.selectionStart, t2.selectionEnd]).toEqual([0, 3]);
    await user.keyboard('{ArrowDown}'); // last row: stays
    expect(document.activeElement).toBe(t2);
    await user.keyboard('{ArrowUp}{ArrowUp}');
    expect(document.activeElement).toBe(getByLabelText('type0'));
  });

  it('a row whose field is disabled gets the focus itself', async () => {
    const user = userEvent.setup();
    const { getByLabelText, container } = render(<Grid />);
    await user.click(getByLabelText('name0'));
    await user.keyboard('{ArrowDown}');
    expect(document.activeElement).toBe(container.querySelector('[data-row="1"]'));
  });

  it('← / → leave a text field only from its edge, and skip to the next field', async () => {
    const user = userEvent.setup();
    const { getByLabelText } = render(<Grid />);
    const name = getByLabelText('name0') as HTMLInputElement;
    await user.click(name);
    name.setSelectionRange(2, 2);
    await user.keyboard('{ArrowRight}');
    expect(document.activeElement).toBe(name); // caret moved inside the text
    name.setSelectionRange(4, 4);
    await user.keyboard('{ArrowRight}');
    expect(document.activeElement).toBe(getByLabelText('type0'));
    await user.keyboard('{ArrowRight}'); // whole text selected on entry: leaves at once
    expect(document.activeElement).toBe(getByLabelText('null0'));
    await user.keyboard('{ArrowLeft}');
    expect(document.activeElement).toBe(getByLabelText('type0'));
  });
});
