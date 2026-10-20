import { useState, type InputHTMLAttributes } from 'react';

interface Props extends Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> {
  /** Accessible name of the field, used for the reveal button ("Show <label>"). */
  label: string;
}

/** A password input with one consistent reveal control (the browser's native eye is hidden in CSS). */
export function PasswordInput({ label, ...rest }: Props) {
  const [shown, setShown] = useState(false);
  return (
    <div className="pw-wrap">
      <input {...rest} type={shown ? 'text' : 'password'} />
      <button
        type="button"
        className="pw-toggle"
        aria-label={`${shown ? 'Hide' : 'Show'} ${label}`}
        aria-pressed={shown}
        onClick={() => setShown((v) => !v)}
      >
        {shown ? 'Hide' : 'Show'}
      </button>
    </div>
  );
}
