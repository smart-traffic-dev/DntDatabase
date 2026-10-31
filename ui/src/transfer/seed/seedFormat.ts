import type { FakeKind, Generator, SeedColumn } from '../../bindings';

/** Texts of the data seeder: generator and fake-data labels, and which generators fit a column. */

export type GeneratorKind = Generator['kind'];

export const GENERATOR_LABEL: Record<GeneratorKind, string> = {
  skip: 'Skip (default)',
  sequence: 'Sequence',
  int_range: 'Integer range',
  decimal_range: 'Decimal range',
  bool: 'Boolean',
  date_range: 'Date range',
  date_time_range: 'Date-time range',
  time_range: 'Time range',
  pick: 'Pick from list',
  foreign_key: 'Foreign key',
  fake: 'Fake data',
  uuid: 'UUID',
  constant: 'Constant',
  pattern: 'Pattern',
  json_object: 'JSON object',
  random_bytes: 'Random bytes',
};

export const FAKE_LABEL: Record<FakeKind, string> = {
  first_name: 'First name',
  last_name: 'Last name',
  full_name: 'Full name',
  email: 'Email',
  username: 'Username',
  phone: 'Phone',
  street_address: 'Street address',
  city: 'City',
  state: 'State',
  zip_code: 'ZIP code',
  country: 'Country',
  company: 'Company',
  job_title: 'Job title',
  url: 'URL',
  domain: 'Domain',
  ipv4: 'IPv4 address',
  slug: 'Slug',
  word: 'Word',
  sentence: 'Sentence',
  paragraph: 'Paragraph',
  hex_color: 'Hex color',
  password_hash: 'Password hash',
};

export const FAKE_KINDS = Object.keys(FAKE_LABEL) as FakeKind[];

/** The generators a column can use, `Skip` first and `Constant` last. */
export function generatorsFor(col: SeedColumn): GeneratorKind[] {
  if (col.locked) return ['skip'];
  const byType: Record<SeedColumn['column']['logical'], GeneratorKind[]> = {
    Integer: ['sequence', 'int_range', 'pick', 'bool'],
    Decimal: ['decimal_range', 'int_range', 'sequence', 'pick'],
    Float: ['decimal_range', 'int_range', 'sequence', 'pick'],
    Text: ['fake', 'pattern', 'pick', 'uuid', 'int_range', 'sequence'],
    Enum: ['pick'],
    Bool: ['bool'],
    Date: ['date_range'],
    DateTime: ['date_time_range', 'date_range'],
    Time: ['time_range'],
    Json: ['json_object'],
    Uuid: ['uuid'],
    Binary: ['random_bytes'],
    Array: [],
    Other: [],
  };
  return ['skip', ...(col.foreign_key ? (['foreign_key'] as GeneratorKind[]) : []), ...byType[col.column.logical], 'constant'];
}

/** Generators whose values can repeat, so "Unique" means something. */
export const uniqueApplies = (kind: GeneratorKind): boolean =>
  !['skip', 'bool', 'constant', 'sequence', 'uuid', 'json_object', 'random_bytes'].includes(kind);

/** The generator as one line ("Fake data · Email", "Integer range 1 – 1000"). */
export function generatorSummary(g: Generator): string {
  switch (g.kind) {
    case 'fake':
      return `${GENERATOR_LABEL.fake} · ${FAKE_LABEL[g.fake]}`;
    case 'int_range':
    case 'decimal_range':
      return `${GENERATOR_LABEL[g.kind]} ${g.min} – ${g.max}`;
    default:
      return GENERATOR_LABEL[g.kind];
  }
}
