import { statsText, type SelectionStats } from './selectionStats';

/** The slim footer under a grid: statistics of the selected cells (nothing selected, nothing shown). */
export function GridStats({ stats }: { stats: SelectionStats | null }) {
  if (!stats) return null;
  return (
    <div className="gt-stats" role="status" aria-label="Selection statistics" data-testid="grid-stats">
      {statsText(stats)}
    </div>
  );
}
