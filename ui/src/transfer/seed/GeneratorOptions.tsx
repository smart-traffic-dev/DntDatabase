import type { FakeKind, Generator } from '../../bindings';
import { FAKE_KINDS, FAKE_LABEL } from './seedFormat';

interface Props {
  column: string;
  generator: Generator;
  /** Keys read for this column's foreign key (from the preview), when known. */
  keyCount?: number;
  /** `ref_table.ref_column` of the column's foreign key. */
  reference?: string;
  disabled?: boolean;
  onChange(g: Generator): void;
}

/** A number input whose empty text keeps the old value (the backend checks the rest). */
function NumberInput({ label, value, onChange, disabled, step }: { label: string; value: number; onChange(n: number): void; disabled?: boolean; step?: string }) {
  return (
    <input
      type="number"
      aria-label={label}
      value={Number.isFinite(value) ? value : ''}
      step={step}
      disabled={disabled}
      onChange={(e) => {
        const n = e.target.valueAsNumber;
        if (Number.isFinite(n)) onChange(n);
      }}
    />
  );
}

function TextInput({ label, value, onChange, disabled, placeholder }: { label: string; value: string; onChange(s: string): void; disabled?: boolean; placeholder?: string }) {
  return <input type="text" aria-label={label} value={value} placeholder={placeholder} disabled={disabled} spellCheck={false} onChange={(e) => onChange(e.target.value)} />;
}

/** The options of one column's generator, inline in its row (spec §6.2). */
export function GeneratorOptions({ column, generator: g, keyCount, reference, disabled, onChange }: Props) {
  const of = (what: string) => `${what} for ${column}`;
  switch (g.kind) {
    case 'sequence':
      return (
        <span className="seed-opts">
          <NumberInput label={of('Start')} value={g.start} disabled={disabled} onChange={(start) => onChange({ ...g, start: Math.trunc(start) })} />
          <NumberInput label={of('Step')} value={g.step} disabled={disabled} onChange={(step) => onChange({ ...g, step: Math.trunc(step) })} />
        </span>
      );
    case 'int_range':
      return (
        <span className="seed-opts">
          <NumberInput label={of('Minimum')} value={g.min} disabled={disabled} onChange={(min) => onChange({ ...g, min: Math.trunc(min) })} />
          <span aria-hidden="true">–</span>
          <NumberInput label={of('Maximum')} value={g.max} disabled={disabled} onChange={(max) => onChange({ ...g, max: Math.trunc(max) })} />
        </span>
      );
    case 'decimal_range':
      return (
        <span className="seed-opts">
          <NumberInput label={of('Minimum')} value={g.min} step="any" disabled={disabled} onChange={(min) => onChange({ ...g, min })} />
          <span aria-hidden="true">–</span>
          <NumberInput label={of('Maximum')} value={g.max} step="any" disabled={disabled} onChange={(max) => onChange({ ...g, max })} />
          <NumberInput label={of('Scale')} value={g.scale} disabled={disabled} onChange={(scale) => onChange({ ...g, scale: Math.max(0, Math.trunc(scale)) })} />
        </span>
      );
    case 'bool':
      return (
        <span className="seed-opts">
          <NumberInput label={of('True %')} value={g.true_percent} disabled={disabled} onChange={(p) => onChange({ ...g, true_percent: Math.trunc(p) })} />
        </span>
      );
    case 'date_range':
    case 'date_time_range':
    case 'time_range': {
      const hint = g.kind === 'date_range' ? 'YYYY-MM-DD' : g.kind === 'time_range' ? 'HH:MM:SS' : 'YYYY-MM-DD HH:MM:SS';
      return (
        <span className="seed-opts">
          <TextInput label={of('From')} value={g.from} placeholder={hint} disabled={disabled} onChange={(from) => onChange({ ...g, from })} />
          <span aria-hidden="true">–</span>
          <TextInput label={of('To')} value={g.to} placeholder={hint} disabled={disabled} onChange={(to) => onChange({ ...g, to })} />
        </span>
      );
    }
    case 'pick':
      return (
        <span className="seed-opts wide">
          <TextInput
            label={of('Values')}
            value={g.values.join(', ')}
            placeholder="a, b, c"
            disabled={disabled}
            onChange={(text) => onChange({ ...g, values: text.split(',').map((v) => v.trim()).filter((v) => v !== '') })}
          />
        </span>
      );
    case 'fake':
      return (
        <span className="seed-opts">
          <select aria-label={of('Fake data')} value={g.fake} disabled={disabled} onChange={(e) => onChange({ ...g, fake: e.target.value as FakeKind })}>
            {FAKE_KINDS.map((k) => (
              <option key={k} value={k}>
                {FAKE_LABEL[k]}
              </option>
            ))}
          </select>
        </span>
      );
    case 'constant':
      return (
        <span className="seed-opts wide">
          <TextInput label={of('Value')} value={g.value} disabled={disabled} onChange={(value) => onChange({ ...g, value })} />
        </span>
      );
    case 'pattern':
      return (
        <span className="seed-opts wide" title="# digit, ? letter, * letter or digit, \ escapes the next character">
          <TextInput label={of('Pattern')} value={g.pattern} disabled={disabled} onChange={(pattern) => onChange({ ...g, pattern })} />
        </span>
      );
    case 'random_bytes':
      return (
        <span className="seed-opts">
          <NumberInput label={of('Length')} value={g.len} disabled={disabled} onChange={(len) => onChange({ ...g, len: Math.trunc(len) })} />
        </span>
      );
    case 'foreign_key':
      return (
        <span className="seed-opts muted">
          {reference ?? g.constraint}
          {keyCount !== undefined && ` · ${keyCount.toLocaleString('en-US')} ${keyCount === 1 ? 'key' : 'keys'}`}
        </span>
      );
    case 'skip':
    case 'uuid':
    case 'json_object':
      return <span className="seed-opts muted">—</span>;
  }
}
