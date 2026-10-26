import type { SelectHTMLAttributes } from 'react';
import './comboInput.css';

/** A select of the designer grids, with the same chevron and borders as `ComboInput`. */
export function GridSelect({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <span className="dz-combo dz-select">
      <select {...rest} className={`dz-input mono${className ? ` ${className}` : ''}`}>
        {children}
      </select>
      <svg className="dz-combo-chevron" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
        <path d="M6 9l6 6 6-6" />
      </svg>
    </span>
  );
}
