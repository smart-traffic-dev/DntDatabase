import type { CheckDef } from '../../bindings';
import { DraftInput } from './DraftInput';
import { ElementGrid, type ElementGridProps } from './ElementGrid';

/** The Checks grid: a name and the expression (monospace). */
export function ChecksGrid(p: ElementGridProps) {
  return (
    <ElementGrid
      {...p}
      listKey="checks"
      label="Checks"
      noun="check"
      cols="minmax(0, 1fr) minmax(0, 3fr)"
      headers={['Name', 'Expression']}
      fields={['name', 'expression']}
      cells={({ row, disabled, errorOf, set }) => {
        const check = row.item as CheckDef;
        return (
          <>
            <div role="cell">
              <DraftInput aria-label="Name" className="mono" value={check.name} disabled={disabled} error={errorOf('name')} onCommit={(text, key) => set({ name: text }, key)} />
            </div>
            <div role="cell">
              <DraftInput
                aria-label="Expression"
                className="mono"
                placeholder="price > 0"
                value={check.expression}
                disabled={disabled}
                error={errorOf('expression')}
                onCommit={(text, key) => set({ expression: text }, key)}
              />
            </div>
          </>
        );
      }}
    />
  );
}
