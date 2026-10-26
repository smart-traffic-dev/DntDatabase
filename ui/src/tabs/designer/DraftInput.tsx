import { useId, useRef, useState, type InputHTMLAttributes, type Ref } from 'react';

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, 'value' | 'onChange' | 'defaultValue'> & {
  /** The draft's value, as text. */
  value: string;
  /** Called on every change with the text and a key that is the same for one focus of the field (one undo step). */
  onCommit(text: string, coalesce: string): void;
  /** The text the draft will show for `text` (e.g. `14,` -> `14`); identity by default. */
  normalize?(text: string): string;
  /** A draft error of this field: marks it invalid and describes it. */
  error?: string;
  ref?: Ref<HTMLInputElement>;
};

let focusCount = 0;

/**
 * A text field bound to the draft. Every keystroke edits the draft (the live SQL follows), while
 * the field keeps showing what was typed (`14,`) until the draft changes from elsewhere (undo).
 */
export function DraftInput({ value, onCommit, normalize = (t) => t, error, onFocus, onBlur, className, ...rest }: Props) {
  const [local, setLocal] = useState<{ text: string; expect: string } | null>(null);
  const session = useRef('');
  const errorId = useId();
  const shown = local && local.expect === value ? local.text : value;
  return (
    <>
      <input
        {...rest}
        className={`dz-input${error ? ' dz-invalid' : ''}${className ? ` ${className}` : ''}`}
        value={shown}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? errorId : undefined}
        title={error ?? rest.title}
        onFocus={(e) => {
          session.current = `f${++focusCount}`;
          onFocus?.(e);
        }}
        onBlur={(e) => {
          setLocal(null);
          onBlur?.(e);
        }}
        onChange={(e) => {
          const text = e.target.value;
          setLocal({ text, expect: normalize(text) });
          onCommit(text, session.current);
        }}
      />
      {error ? (
        <span id={errorId} hidden>
          {error}
        </span>
      ) : null}
    </>
  );
}
