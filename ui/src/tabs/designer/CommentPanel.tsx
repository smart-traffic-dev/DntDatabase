import type { DraftError, TableDef } from '../../bindings';
import './elementsGrid.css';

export interface CommentPanelProps {
  draft: TableDef;
  errors: DraftError[];
  editable: boolean;
  edit(recipe: (d: TableDef) => TableDef, coalesce?: string): void;
}

/** The Comment section: the table's comment. */
export function CommentPanel({ draft, errors, editable, edit }: CommentPanelProps) {
  const error = errors.find((e) => e.element.kind === 'table' && e.field === 'comment')?.message;
  return (
    <div className="dz-options">
      <label className="dz-field">
        <span>Table comment</span>
        <textarea
          aria-label="Table comment"
          className={`dz-input dz-textarea${error ? ' dz-invalid' : ''}`}
          rows={6}
          value={draft.comment ?? ''}
          disabled={!editable}
          title={error}
          aria-invalid={error ? true : undefined}
          onChange={(e) => edit((d) => ({ ...d, comment: e.target.value === '' ? null : e.target.value }), 'table-comment')}
        />
      </label>
      {error ? <p className="dz-subtle">{error}</p> : null}
    </div>
  );
}
