import type { IndexDef } from '../../bindings';
import { DraftInput } from './DraftInput';
import { ElementGrid, type ElementGridProps } from './ElementGrid';
import { GridSelect } from './GridSelect';
import { addPart, parseList, removePart, updatePart } from './elementModel';

const PG_METHODS = ['btree', 'hash', 'gist', 'spgist', 'gin', 'brin'];

/** The Indexes grid: name, unique, parts (column, prefix on MySQL, DESC), and the engine's extras. */
export function IndexesGrid(p: ElementGridProps) {
  const pg = p.open.server.kind === 'postgres';
  const mysql8 = p.open.server.variant === 'MySql8';
  const columns = p.draft.columns.map((c) => c.name);
  const cols = ['minmax(0, 1.5fr)', '56px', 'minmax(0, 2.4fr)', ...(pg ? ['110px', 'minmax(0, 1fr)'] : []), ...(mysql8 ? ['64px'] : []), 'minmax(0, 1.2fr)'].join(' ');
  const headers = ['Name', 'Unique', 'Columns', ...(pg ? ['Method', 'Include'] : []), ...(mysql8 ? ['Visible'] : []), 'Comment'];

  return (
    <ElementGrid
      {...p}
      listKey="indexes"
      label="Indexes"
      noun="index"
      cols={cols}
      headers={headers}
      fields={['name', 'unique', 'parts', 'method', 'include', 'visible', 'comment']}
      cells={({ row, disabled, errorOf, set }) => {
        const idx = row.item as IndexDef;
        const at = row.index;
        const partsError = errorOf('parts');
        const editParts = (recipe: (d: typeof p.draft, index: number) => typeof p.draft) => at !== null && p.edit((d) => recipe(d, at));
        return (
          <>
            <div role="cell" className="dz-cell-name">
              <DraftInput aria-label="Name" className="mono" value={idx.name} disabled={disabled} error={errorOf('name')} onCommit={(text, key) => set({ name: text }, key)} />
              {pg && idx.constraint ? (
                <span className="dz-tag" title="Backs a table constraint; it stays a constraint">
                  Constraint
                </span>
              ) : null}
            </div>
            <div role="cell" className="dz-center">
              <input type="checkbox" aria-label="Unique" checked={idx.unique} disabled={disabled} onChange={(e) => set({ unique: e.target.checked })} />
            </div>
            {/* The parts as chips on one line (they wrap): column, prefix (MySQL), DESC, remove; then "+ Column". */}
            <div role="cell" className="dz-parts" title={partsError}>
              {idx.parts.map((part, i) => (
                <div key={i} className={`dz-part${partsError ? ' dz-invalid' : ''}`}>
                  {part.expression !== null ? (
                    <span className="mono dz-part-expr" title="Expression part (read-only)">
                      {part.expression}
                    </span>
                  ) : (
                    <GridSelect
                      aria-label="Column"
                      value={part.column ?? ''}
                      disabled={disabled}
                      onChange={(e) => editParts((d, n) => updatePart(d, n, i, { column: e.target.value }))}
                    >
                      {part.column === null || !columns.includes(part.column) ? <option value={part.column ?? ''}>{part.column ?? ''}</option> : null}
                      {columns.map((c) => (
                        <option key={c} value={c}>
                          {c}
                        </option>
                      ))}
                    </GridSelect>
                  )}
                  {pg ? null : (
                    <input
                      type="number"
                      min={1}
                      aria-label="Prefix length"
                      placeholder="len"
                      title="Prefix length (characters of the column indexed)"
                      className="dz-input mono dz-prefix"
                      value={part.prefix ?? ''}
                      disabled={disabled || part.expression !== null}
                      onChange={(e) => {
                        const n = e.target.value === '' ? null : Number(e.target.value);
                        editParts((d, at) => updatePart(d, at, i, { prefix: n }));
                      }}
                    />
                  )}
                  <label className="dz-check dz-desc">
                    <input
                      type="checkbox"
                      aria-label="DESC"
                      checked={part.descending}
                      disabled={disabled}
                      onChange={(e) => editParts((d, at) => updatePart(d, at, i, { descending: e.target.checked }))}
                    />
                    DESC
                  </label>
                  {idx.parts.length > 1 ? (
                    <button
                      type="button"
                      className="dz-part-remove"
                      aria-label="Remove column"
                      title="Remove this column from the index"
                      disabled={disabled}
                      onClick={() => editParts((d, at) => removePart(d, at, i))}
                    >
                      ×
                    </button>
                  ) : null}
                </div>
              ))}
              <button
                type="button"
                className="dz-part-add"
                aria-label="Add column"
                title="Add a column to the index"
                disabled={disabled}
                onClick={() => editParts((d, at) => addPart(d, at))}
              >
                + Column
              </button>
            </div>
            {pg ? (
              <>
                <div role="cell">
                  <GridSelect
                    aria-label="Method"
                    value={idx.method ?? ''}
                    disabled={disabled}
                    onChange={(e) => set({ method: e.target.value === '' ? null : e.target.value })}
                  >
                    <option value="">default</option>
                    {idx.method !== null && !PG_METHODS.includes(idx.method) ? <option value={idx.method}>{idx.method}</option> : null}
                    {PG_METHODS.map((m) => (
                      <option key={m} value={m}>
                        {m}
                      </option>
                    ))}
                  </GridSelect>
                </div>
                <div role="cell">
                  <DraftInput
                    aria-label="Include"
                    className="mono"
                    placeholder="col, col"
                    value={idx.include.join(', ')}
                    disabled={disabled}
                    error={errorOf('include')}
                    normalize={(t) => parseList(t).join(', ')}
                    onCommit={(text, key) => set({ include: parseList(text) }, key)}
                  />
                </div>
              </>
            ) : null}
            {mysql8 ? (
              <div role="cell" className="dz-center">
                <input type="checkbox" aria-label="Visible" checked={idx.visible} disabled={disabled} onChange={(e) => set({ visible: e.target.checked })} />
              </div>
            ) : null}
            <div role="cell">
              <DraftInput
                aria-label="Comment"
                value={idx.comment ?? ''}
                disabled={disabled}
                error={errorOf('comment')}
                onCommit={(text, key) => set({ comment: text === '' ? null : text }, key)}
              />
            </div>
          </>
        );
      }}
    />
  );
}
