import { render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { ResultSet } from '../../store/queryRun';

// The real Glide grid (the global setup replaces it with a placeholder): it must mount under React 19.
vi.unmock('@glideapps/glide-data-grid');

const result: ResultSet = {
  id: 1,
  index: 0,
  columns: [{ name: 'id', db_type: 'int4', logical: 'Integer', nullable: false, origin_table: null, origin_column: null }],
  chunks: [Array.from({ length: 500 }, (_, i) => [{ t: 'Int', v: i }])],
  rowCount: 500,
  truncated: false,
  affected: null,
  elapsedMs: 1,
  done: true,
};

describe('ResultGrid with the real Glide Data Grid', () => {
  it('mounts_under_react_19', async () => {
    const { ResultGrid } = await import('./ResultGrid');
    const { container } = render(<ResultGrid result={result} />);
    // jsdom has no layout or canvas: only mounting without errors can be checked here; that the
    // grid paints and scrolls under React 19 is checked in the real app.
    expect(container.querySelector('[data-testid="glide-mock"]')).toBeNull();
    expect(container.querySelector('.qr-grid')?.childElementCount).toBeGreaterThan(0);
  }, 60_000);
});
