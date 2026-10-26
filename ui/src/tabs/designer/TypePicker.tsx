import { useMemo } from 'react';
import type { TypeInfo } from '../../bindings';
import { ComboInput, filterOptions } from './ComboInput';

/** The group a catalog type is listed under (by name: the catalog itself has no categories). */
const GROUPS: [string, RegExp][] = [
  ['Numeric', /^(tinyint|smallint|mediumint|int|integer|bigint|decimal|numeric|float|double|double precision|real|bit|bool|boolean|serial|bigserial|smallserial|money)$/],
  ['String', /^(char|varchar|character|character varying|tinytext|text|mediumtext|longtext|enum|set|citext|name)$/],
  ['Date & time', /^(date|time|timetz|datetime|timestamp|timestamptz|year|interval)/],
  ['Binary', /^(binary|varbinary|tinyblob|blob|mediumblob|longblob|bytea|bit varying|varbit)$/],
  ['JSON & documents', /^(json|jsonb|xml|uuid|hstore)$/],
  ['Spatial', /^(geometry|point|linestring|polygon|multipoint|multilinestring|multipolygon|geometrycollection|geography|box|circle|line|lseg|path)$/],
  ['Network', /^(inet|cidr|macaddr|macaddr8)$/],
];
export const groupOf = (base: string) => GROUPS.find(([, re]) => re.test(base))?.[0] ?? 'Other';

/** How a type takes parameters, as a hint after its name: `(n)`, `(p,s)`, `[(p)]` (optional), `(values)`. */
export function signatureOf(t: TypeInfo): string {
  if (t.values) return '(values)';
  if (t.precision !== 'None' && t.scale !== 'None') return t.precision === 'Required' ? '(p,s)' : '[(p,s)]';
  if (t.precision !== 'None') return t.precision === 'Required' ? '(p)' : '[(p)]';
  if (t.length !== 'None') return t.length === 'Required' ? '(n)' : '[(n)]';
  return '';
}

/** Catalog types matching `query`: prefix matches first, then the rest containing it. */
export const filterTypes = (catalog: TypeInfo[], query: string) =>
  filterOptions(catalog.map((t) => ({ ...t, value: t.base })), query) as (TypeInfo & { value: string })[];

interface Props {
  value: string;
  catalog: TypeInfo[];
  disabled?: boolean;
  error?: string;
  onCommit(text: string, coalesce: string): void;
}

/** The Type field: free text (custom types stay possible) with the server's types, grouped. */
export function TypePicker({ value, catalog, disabled, error, onCommit }: Props) {
  const options = useMemo(() => catalog.map((t) => ({ value: t.base, hint: signatureOf(t), group: groupOf(t.base) })), [catalog]);
  return (
    <ComboInput
      label="Type"
      value={value}
      options={options}
      disabled={disabled}
      error={error}
      emptyText="No matching type. The text is used as a custom type."
      onCommit={onCommit}
    />
  );
}
