import type {
  ConvertOptions,
  DataImportPreview,
  ErrorPolicy,
  ImportMode,
  ImportRequest,
  ImportTarget,
  Inspection,
  Mapping,
  MappingReport,
  ObjectRef,
  SourceFormat,
  SourceOptions,
} from '../../bindings';

/**
 * The import wizard's state (spec §6.7): a pure reducer, so the step gating and what each change
 * invalidates are tested without React. Every backend answer enters through an action; the
 * mapping rules themselves stay in the backend (`import_auto_map`, `import_check_mapping`).
 */

export type StepId = 'source' | 'target' | 'mapping' | 'options' | 'check' | 'review';

export const STEPS: { id: StepId; label: string }[] = [
  { id: 'source', label: 'Source' },
  { id: 'target', label: 'Target' },
  { id: 'mapping', label: 'Mapping' },
  { id: 'options', label: 'Options' },
  { id: 'check', label: 'Check' },
  { id: 'review', label: 'Review' },
];

/** Rows and bytes per `INSERT` the wizard starts with (the backend's defaults). */
export const DEFAULT_BATCH_ROWS = 1000;
export const DEFAULT_BATCH_BYTES = 1024 * 1024;
export const DEFAULT_MAX_ERRORS = 1000;

export interface DryRunRef {
  jobId: string;
  /** "Validate on server" (temporary table) rather than the file only. */
  server: boolean;
}

export interface WizardState {
  step: StepId;
  connectionId: string;
  table: ObjectRef | null;
  path: string | null;
  /** Counts the file picks: picking the same file again reads it again. */
  picks: number;
  source: SourceOptions;
  inspection: Inspection | null;
  /** XLSX sheet names of the last inspection (kept while another sheet is inspected). */
  sheets: string[];
  target: ImportTarget | null;
  /** Empty until auto-mapped (or pre-filled from stored parameters). */
  mapping: Mapping[];
  /** The backend's check of `mapping` + `mode` + `overrideIdentity`; null while it is not known. */
  report: MappingReport | null;
  mode: ImportMode;
  errors: ErrorPolicy;
  convert: ConvertOptions;
  batchRows: number;
  batchBytes: number;
  overrideIdentity: boolean;
  dryRun: DryRunRef | null;
  /** The reviewed plan (`import_prepare`); any later change drops it. */
  preview: DataImportPreview | null;
}

export type WizardAction =
  | { type: 'goto'; step: StepId }
  | { type: 'file'; path: string }
  | { type: 'source'; patch: Partial<SourceOptions> }
  | { type: 'inspected'; inspection: Inspection }
  | { type: 'connection'; connectionId: string }
  | { type: 'table'; table: ObjectRef | null }
  | { type: 'targetLoaded'; target: ImportTarget }
  | { type: 'mapping'; mapping: Mapping[] }
  | { type: 'mapOne'; target: string; source: number | null }
  | { type: 'report'; report: MappingReport }
  | { type: 'mode'; mode: ImportMode }
  | { type: 'errors'; errors: ErrorPolicy }
  | { type: 'convert'; patch: Partial<ConvertOptions> }
  | { type: 'batch'; rows: number; bytes: number }
  | { type: 'overrideIdentity'; value: boolean }
  | { type: 'dryRun'; jobId: string; server: boolean }
  | { type: 'prepared'; preview: DataImportPreview }
  | { type: 'unprepare' };

/** The format (and TSV's delimiter) guessed from the file's extension; changeable in the Source step. */
export function formatFromPath(path: string): { format: SourceFormat; delimiter: string | null } {
  const ext = /\.([^.\\/]+)$/.exec(path)?.[1]?.toLowerCase() ?? '';
  if (ext === 'tsv') return { format: 'csv', delimiter: '\t' };
  if (ext === 'json') return { format: 'json', delimiter: null };
  if (ext === 'jsonl' || ext === 'ndjson') return { format: 'jsonl', delimiter: null };
  if (ext === 'xlsx') return { format: 'xlsx', delimiter: null };
  return { format: 'csv', delimiter: null };
}

export function initialState(init: { connectionId: string; table?: ObjectRef | null; params?: ImportRequest | null }): WizardState {
  const p = init.params ?? null;
  return {
    step: 'source',
    connectionId: p?.connection_id ?? init.connectionId,
    table: p?.table ?? init.table ?? null,
    path: p?.path ?? null,
    picks: 0,
    source: p?.source ?? { format: 'csv', delimiter: null, header: true, sheet: null },
    inspection: null,
    sheets: [],
    target: null,
    mapping: p?.mapping ?? [],
    report: null,
    mode: p?.mode ?? { kind: 'append' },
    errors: p?.errors ?? { kind: 'stop' },
    convert: p?.convert ?? { empty: 'null', null_text: null, trim: false, binary: 'hex' },
    batchRows: p?.batch_rows || DEFAULT_BATCH_ROWS,
    batchBytes: p?.batch_bytes || DEFAULT_BATCH_BYTES,
    overrideIdentity: p?.override_identity ?? false,
    dryRun: null,
    preview: null,
  };
}

/** The upsert key is set (it must also pass `import_check_mapping`, R14). */
const keyChosen = (mode: ImportMode) => mode.kind !== 'upsert' || mode.key.length > 0;

/** The current step's requirements are met, so Next is enabled. */
export function canAdvance(s: WizardState, step: StepId = s.step): boolean {
  switch (step) {
    case 'source':
      return s.path !== null && s.inspection !== null;
    case 'target':
      return s.target !== null;
    case 'mapping':
      return s.report !== null && s.report.errors.length === 0;
    case 'options':
      return keyChosen(s.mode) && s.report !== null && s.report.errors.length === 0;
    case 'check':
      return true;
    case 'review':
      return false;
  }
}

/** Every step before `step` can be left: the step may be opened. */
export function canOpen(s: WizardState, step: StepId): boolean {
  for (const { id } of STEPS) {
    if (id === step) return true;
    if (!canAdvance(s, id)) return false;
  }
  return false;
}

/** What a later step computed from earlier choices; dropped when those choices change. */
const dropPlan = { preview: null } as const;
const dropChecks = { dryRun: null, preview: null } as const;

export function reduce(s: WizardState, a: WizardAction): WizardState {
  switch (a.type) {
    case 'goto':
      return canOpen(s, a.step) ? { ...s, step: a.step } : s;
    case 'file': {
      const picks = s.picks + 1;
      // The same file picked again (a "Run again" after a restart: admissions last one app
      // session) keeps its source options and mapping; it is only read again.
      if (a.path === s.path) return { ...s, ...dropChecks, picks, inspection: null };
      const guess = formatFromPath(a.path);
      return {
        ...s,
        ...dropChecks,
        path: a.path,
        picks,
        source: { format: guess.format, delimiter: guess.delimiter, header: true, sheet: null },
        inspection: null,
        sheets: [],
        mapping: [],
        report: null,
      };
    }
    case 'source':
      return { ...s, ...dropChecks, source: { ...s.source, ...a.patch }, inspection: null, mapping: [], report: null };
    case 'inspected':
      return { ...s, inspection: a.inspection, sheets: a.inspection.sheets.length > 0 ? a.inspection.sheets : s.sheets };
    case 'connection':
      if (a.connectionId === s.connectionId) return s;
      return { ...s, ...dropChecks, connectionId: a.connectionId, table: null, target: null, mapping: [], report: null };
    case 'table':
      return { ...s, ...dropChecks, table: a.table, target: null, mapping: [], report: null, mode: { kind: 'append' }, overrideIdentity: false };
    case 'targetLoaded':
      return { ...s, target: a.target, report: null };
    case 'mapping':
      return { ...s, ...dropChecks, mapping: a.mapping, report: null };
    case 'mapOne': {
      const has = s.mapping.some((m) => m.target === a.target);
      const mapping = has
        ? s.mapping.map((m) => (m.target === a.target ? { ...m, source: a.source } : m))
        : [...s.mapping, { target: a.target, source: a.source }];
      return { ...s, ...dropChecks, mapping, report: null };
    }
    case 'report':
      return { ...s, report: a.report };
    case 'mode':
      return { ...s, ...dropChecks, mode: a.mode, report: null };
    case 'errors':
      // The dry run collects errors up to `max_errors`: its result belongs to the old policy.
      return { ...s, ...dropChecks, errors: a.errors };
    case 'convert':
      return { ...s, ...dropChecks, convert: { ...s.convert, ...a.patch } };
    case 'batch':
      return { ...s, ...dropPlan, batchRows: a.rows, batchBytes: a.bytes };
    case 'overrideIdentity':
      return { ...s, ...dropChecks, overrideIdentity: a.value, report: null };
    case 'dryRun':
      return { ...s, dryRun: { jobId: a.jobId, server: a.server } };
    case 'prepared':
      return { ...s, preview: a.preview };
    case 'unprepare':
      return { ...s, preview: null };
  }
}

/** The request of the dry run and the plan; null until a file and a table are chosen. */
export function buildRequest(s: WizardState): ImportRequest | null {
  if (!s.path || !s.table) return null;
  return {
    connection_id: s.connectionId,
    table: s.table,
    path: s.path,
    source: s.source,
    mapping: s.mapping,
    mode: s.mode,
    errors: s.errors,
    convert: s.convert,
    batch_rows: s.batchRows,
    batch_bytes: s.batchBytes,
    override_identity: s.overrideIdentity,
  };
}
