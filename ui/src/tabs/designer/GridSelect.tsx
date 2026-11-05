import { SelectMenu, type SelectMenuOption } from '../../components/SelectMenu';
import './comboInput.css';

interface Props {
  /** Accessible name (the cell has no visible label: the column header names it). */
  label: string;
  value: string;
  /** The choices: plain values (shown as is) or full options. */
  options: (string | SelectMenuOption)[];
  onChange(value: string): void;
  disabled?: boolean;
  invalid?: boolean;
  title?: string;
  /** `xs` inside a chip (an index part); `sm` (the default) fills a grid cell. */
  size?: 'xs' | 'sm';
  className?: string;
}

const toOption = (o: string | SelectMenuOption): SelectMenuOption => (typeof o === 'string' ? { value: o, label: o } : o);

/**
 * A select of the designer grids: the app's themed SelectMenu, borderless until hovered or
 * focused like the grid's text fields. ↑ / ↓ stay the grid's (row to row); Alt+↓, Enter or Space open it.
 */
export function GridSelect({ label, value, options, onChange, disabled, invalid, title, size = 'sm', className }: Props) {
  const list = options.map(toOption);
  // The current value stays shown (and selectable) when it is not one of the choices.
  const all = value !== '' && !list.some((o) => o.value === value) ? [{ value, label: value }, ...list] : list;
  return (
    <SelectMenu
      label={label}
      value={value}
      options={all}
      onChange={onChange}
      disabled={disabled}
      invalid={invalid}
      title={title}
      size={size}
      mono
      searchable={all.length >= 12}
      arrowsOpen={false}
      className={`dz-grid-select${className ? ` ${className}` : ''}`}
    />
  );
}
