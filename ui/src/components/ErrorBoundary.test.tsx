import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ErrorBoundary } from './ErrorBoundary';

function Bomb({ explode }: { explode: boolean }) {
  if (explode) throw new Error('kaboom');
  return <p>child is fine</p>;
}

describe('ErrorBoundary', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('shows a fallback for a crashing child while the neighbouring panel still renders', () => {
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    render(
      <div>
        <ErrorBoundary label="Left panel">
          <Bomb explode />
        </ErrorBoundary>
        <ErrorBoundary label="Right panel">
          <p>right panel</p>
        </ErrorBoundary>
      </div>,
    );
    expect(screen.getByRole('alert')).toHaveTextContent('Left panel failed to render');
    expect(screen.getByRole('alert')).toHaveTextContent('kaboom');
    expect(screen.getByText('right panel')).toBeInTheDocument();
  });

  it('recovers with Try again once the child no longer throws', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    function Harness() {
      const [explode, setExplode] = useState(true);
      return (
        <>
          <button type="button" onClick={() => setExplode(false)}>
            fix
          </button>
          <ErrorBoundary label="Panel">
            <Bomb explode={explode} />
          </ErrorBoundary>
        </>
      );
    }
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole('button', { name: 'fix' }));
    await user.click(screen.getByRole('button', { name: 'Try again' }));
    expect(screen.getByText('child is fine')).toBeInTheDocument();
  });
});
