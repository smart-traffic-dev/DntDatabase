import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import { draftToFilter, emptyDraft, type FilterDraft } from '../store/tableData';
import { FilterBar } from './FilterBar';

function Harness({ onApply, initial = emptyDraft(), error = null }: { onApply(draft: FilterDraft): void; initial?: FilterDraft; error?: string | null }) {
  const [draft, setDraft] = useState(initial);
  return <FilterBar columns={['id', 'name']} draft={draft} active={false} error={error} onChange={setDraft} onApply={() => onApply(draft)} onClear={() => undefined} />;
}

describe('FilterBar', () => {
  it('filter_bar_builds_conditions', async () => {
    const user = userEvent.setup();
    const onApply = vi.fn();
    render(<Harness onApply={onApply} />);
    const bar = screen.getByRole('search', { name: 'Filter' });
    await user.click(within(bar).getByRole('button', { name: '+ Condition' }));
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 1' }), 'between');
    await user.type(within(bar).getByRole('textbox', { name: 'From 1' }), '1');
    await user.type(within(bar).getByRole('textbox', { name: 'To 1' }), '9');
    await user.click(within(bar).getByRole('button', { name: '+ Condition' }));
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 2' }), 'in');
    await user.type(within(bar).getByRole('textbox', { name: 'Value 2' }), 'a, b');
    await user.click(within(bar).getByRole('button', { name: '+ Condition' }));
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 3' }), 'is_null');
    expect(within(bar).queryByRole('textbox', { name: /3$/ })).toBeNull();
    await user.click(within(bar).getByRole('button', { name: 'Apply' }));
    const draft = onApply.mock.calls[0]![0] as FilterDraft;
    expect(draftToFilter(draft).filter).toEqual({
      mode: 'and',
      conditions: [
        { column: 'id', op: 'between', values: ['1', '9'] },
        { column: 'id', op: 'in', values: ['a', 'b'] },
        { column: 'id', op: 'is_null', values: [] },
      ],
    });
  });

  it('raw_where_toggle_sends_raw_filter', async () => {
    const user = userEvent.setup();
    const onApply = vi.fn();
    render(<Harness onApply={onApply} />);
    const bar = screen.getByRole('search', { name: 'Filter' });
    await user.click(within(bar).getByRole('button', { name: '+ Condition' }));
    await user.click(within(bar).getByRole('button', { name: 'Raw WHERE' }));
    expect(within(bar).getByRole('button', { name: 'Raw WHERE' })).toHaveAttribute('aria-pressed', 'true');
    // The condition rows give way to one SQL input.
    expect(within(bar).queryByTestId('filter-row')).toBeNull();
    expect(within(bar).queryByRole('button', { name: '+ Condition' })).toBeNull();
    await user.type(within(bar).getByRole('textbox', { name: 'Raw WHERE' }), "total > 100 AND status = 'paid'{Enter}");
    const draft = onApply.mock.calls[0]![0] as FilterDraft;
    expect(draftToFilter(draft)).toEqual({
      filter: { mode: 'and', conditions: [], raw_where: "total > 100 AND status = 'paid'" },
      error: null,
    });
    // Back to the conditions.
    await user.click(within(bar).getByRole('button', { name: 'Raw WHERE' }));
    expect(within(bar).getByRole('button', { name: '+ Condition' })).toBeInTheDocument();
  });

  it('an empty raw text is no filter; the error shows under the bar', () => {
    expect(draftToFilter({ ...emptyDraft(), rawMode: true, rawText: '   ' })).toEqual({ filter: null, error: null });
    render(<Harness onApply={() => undefined} initial={{ ...emptyDraft(), rawMode: true, rawText: 'x ==' }} error="Syntax error near '=='" />);
    expect(screen.getByRole('alert')).toHaveTextContent("Syntax error near '=='");
  });
});
