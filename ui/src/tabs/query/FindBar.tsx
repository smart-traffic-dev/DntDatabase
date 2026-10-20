import { useEffect, useRef, useState, type KeyboardEvent } from 'react';

/** Find-in-results bar over the grid: Enter / Shift+Enter step, Esc closes. */
export function FindBar({
  query,
  index,
  total,
  capped,
  onQuery,
  onStep,
  onClose,
}: {
  query: string;
  /** 0-based current match, -1 for none. */
  index: number;
  total: number;
  capped: boolean;
  onQuery(q: string): void;
  onStep(dir: 1 | -1): void;
  onClose(): void;
}) {
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      onStep(e.shiftKey ? -1 : 1);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      onClose();
    }
  };
  const status = !query ? '' : total === 0 ? 'No matches' : `${index + 1} / ${total.toLocaleString('en-US')}${capped ? '+' : ''}`;
  return (
    <div className="gt-bar" data-testid="find-bar">
      <input
        ref={input}
        className="gt-input"
        type="text"
        placeholder="Find in results"
        aria-label="Find in results"
        value={query}
        onChange={(e) => onQuery(e.target.value)}
        onKeyDown={onKeyDown}
      />
      <span className="gt-count" aria-live="polite" data-testid="find-count">
        {status}
      </span>
      <button type="button" className="gt-btn" aria-label="Previous match" disabled={total === 0} onClick={() => onStep(-1)}>
        ↑
      </button>
      <button type="button" className="gt-btn" aria-label="Next match" disabled={total === 0} onClick={() => onStep(1)}>
        ↓
      </button>
      <button type="button" className="gt-btn" aria-label="Close find" onClick={onClose}>
        ✕
      </button>
    </div>
  );
}

/** Go-to-row bar (Ctrl+G): Enter jumps, Esc closes. `onGo` returns an error message or null. */
export function GoToBar({ rows, first, onGo, onClose }: { rows: number; first: number; onGo(input: string): string | null; onClose(): void }) {
  const input = useRef<HTMLInputElement>(null);
  const [text, setText] = useState('');
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    input.current?.focus();
  }, []);
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      const message = onGo(text);
      setError(message);
      if (message === null) onClose();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      onClose();
    }
  };
  return (
    <div className="gt-bar" data-testid="goto-bar">
      <input
        ref={input}
        className="gt-input"
        type="text"
        inputMode="numeric"
        placeholder={`Go to row (${first.toLocaleString('en-US')}–${(first + rows - 1).toLocaleString('en-US')})`}
        aria-label="Go to row"
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setError(null);
        }}
        onKeyDown={onKeyDown}
      />
      {error && (
        <span className="gt-count" role="alert">
          {error}
        </span>
      )}
      <button type="button" className="gt-btn" aria-label="Close go to row" onClick={onClose}>
        ✕
      </button>
    </div>
  );
}
