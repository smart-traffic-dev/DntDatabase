import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import { SelectMenu, type SelectMenuOption } from './SelectMenu';
import { menuList, menuOptionLabels, pickMenuOption } from './selectMenuTestKit';

const FRUIT: SelectMenuOption[] = [
  { value: 'apple', label: 'Apple' },
  { value: 'apricot', label: 'Apricot' },
  { value: 'banana', label: 'Banana', hint: 'yellow' },
  { value: 'cherry', label: 'Cherry', disabled: true },
];

function Harness(props: Partial<Parameters<typeof SelectMenu>[0]> & { onPick?(v: string): void }) {
  const [value, setValue] = useState(props.value ?? 'apple');
  return (
    <SelectMenu
      label="Fruit"
      options={FRUIT}
      {...props}
      value={value}
      onChange={(v) => {
        setValue(v);
        props.onPick?.(v);
      }}
    />
  );
}

describe('SelectMenu', () => {
  it('opens on click and picks an option', async () => {
    const user = userEvent.setup();
    const onPick = vi.fn();
    render(<Harness onPick={onPick} />);
    const box = screen.getByRole('combobox', { name: 'Fruit' });
    expect(box).toHaveTextContent('Apple');
    expect(box).toHaveAttribute('aria-expanded', 'false');
    await pickMenuOption(user, box, 'Banana');
    expect(onPick).toHaveBeenCalledWith('banana');
    expect(box).toHaveTextContent('Banana');
    expect(screen.queryByRole('listbox')).toBeNull();
    expect(box).toHaveFocus();
  });

  it('lists its options and ignores a disabled one', async () => {
    const user = userEvent.setup();
    const onPick = vi.fn();
    render(<Harness onPick={onPick} />);
    const box = screen.getByRole('combobox', { name: 'Fruit' });
    expect(await menuOptionLabels(user, box)).toEqual(['Apple', 'Apricot', 'Banana', 'Cherry']);
    expect(screen.queryByRole('listbox')).toBeNull();
    await user.click(box);
    await user.click(screen.getByRole('option', { name: 'Cherry' }));
    expect(onPick).not.toHaveBeenCalled();
  });

  it('keyboard: arrows move, Enter picks, Escape closes and keeps the focus', async () => {
    const user = userEvent.setup();
    const onPick = vi.fn();
    render(<Harness onPick={onPick} />);
    const box = screen.getByRole('combobox', { name: 'Fruit' });
    box.focus();
    await user.keyboard('{ArrowDown}');
    expect(menuList('Fruit')).toBeInTheDocument();
    await user.keyboard('{ArrowDown}{ArrowDown}{Enter}');
    expect(onPick).toHaveBeenLastCalledWith('banana');
    await user.keyboard('{ArrowDown}');
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('listbox')).toBeNull();
    expect(box).toHaveFocus();
  });

  it('type-ahead jumps to the option starting with the typed letters', async () => {
    const user = userEvent.setup();
    const onPick = vi.fn();
    render(<Harness onPick={onPick} />);
    const box = screen.getByRole('combobox', { name: 'Fruit' });
    box.focus();
    // On the closed control: opens the list on the match.
    await user.keyboard('b');
    expect(menuList('Fruit')).toBeInTheDocument();
    await user.keyboard('{Enter}');
    expect(onPick).toHaveBeenLastCalledWith('banana');
    // In the open list: "ap" then a repeated "a" cycles Apple / Apricot.
    await user.keyboard('{ArrowDown}');
    await user.keyboard('apr{Enter}');
    expect(onPick).toHaveBeenLastCalledWith('apricot');
  });

  it('a search field filters long lists; typing on the closed control starts the search', async () => {
    const user = userEvent.setup();
    const onPick = vi.fn();
    render(<Harness searchable onPick={onPick} />);
    const box = screen.getByRole('combobox', { name: 'Fruit' });
    box.focus();
    await user.keyboard('n');
    const search = screen.getByRole('textbox', { name: 'Search fruit' });
    expect(search).toHaveValue('n');
    expect(search).toHaveFocus();
    // "n" is in Banana only.
    expect(screen.getAllByRole('option').map((o) => o.getAttribute('aria-label'))).toEqual(['Banana']);
    await user.type(search, 'x');
    expect(screen.getByText('No match.')).toBeInTheDocument();
    await user.clear(search);
    await user.type(search, 'yell{Enter}');
    expect(onPick).toHaveBeenLastCalledWith('banana');
  });

  it('placeholder, invalid, id and size', () => {
    render(
      <>
        <label htmlFor="fruit-box">Pick a fruit</label>
        <SelectMenu label="Fruit" id="fruit-box" value="" placeholder="Choose…" options={FRUIT} onChange={() => undefined} invalid size="xs" />
      </>,
    );
    const box = screen.getByRole('combobox', { name: 'Fruit' });
    expect(box).toHaveTextContent('Choose…');
    expect(box).toHaveAttribute('aria-invalid', 'true');
    expect(box).toHaveClass('select-menu-xs', 'invalid');
    expect(screen.getByLabelText('Pick a fruit')).toBe(box);
  });

  it('a disabled control does not open', async () => {
    const user = userEvent.setup();
    render(<Harness disabled />);
    await user.click(screen.getByRole('combobox', { name: 'Fruit' }));
    expect(screen.queryByRole('listbox')).toBeNull();
  });
});
