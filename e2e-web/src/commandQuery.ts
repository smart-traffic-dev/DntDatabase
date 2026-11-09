import type {
  ColumnMeta,
  ConnectionConfig,
  EnvTag,
  HistoryEntry,
  PlanStmtView,
  PlanView,
  RunEvent,
  StmtKindLabel,
  Target,
  Value,
  Verdict,
} from '../../ui/src/bindings';
import { appError, fail } from '../../ui/src/test/fixtures/data';
import type { CommandFakeState, RedisValue } from './commandFixtures';
import { WithChannel, type Handler } from './shim';

// Query tabs of MongoDB / Redis connections against the fake Command servers: `statement_ranges`
// cuts a script as the backend's command languages do (Redis one command per line, MongoDB by `;`
// or a blank line), `query_plan` judges each command with the R4a policy (`crates/query/src/
// policy.rs`) and `query_run` streams the R4a `RunEvent`s. Only the commands the specs use are
// understood: GET, SET, HGETALL, FLUSHDB, `db.<c>.find({})` (with `.limit(n)`, or `db.getCollection("c")`) and `db.<c>.deleteMany({})`.

type Range = [number, number];

/** The trimmed, non-empty pieces of `text` between the `cuts` (end-exclusive offsets). */
function pieces(text: string, cuts: Range[]): Range[] {
  const out: Range[] = [];
  let from = 0;
  for (const [a, b] of [...cuts, [text.length, text.length] as Range]) {
    const piece = text.slice(from, a);
    const lead = piece.length - piece.trimStart().length;
    const body = piece.trim();
    if (body) out.push([from + lead, from + lead + body.length]);
    from = b;
  }
  return out;
}

/** Redis: one command per line; `#` comment lines are not commands. */
export function redisRanges(text: string): Range[] {
  const cuts = [...text.matchAll(/\r?\n/g)].map((m) => [m.index, m.index + m[0].length] as Range);
  return pieces(text, cuts).filter(([a]) => text[a] !== '#');
}

/** MongoDB: commands end at a `;` or a blank line. */
export function mongoRanges(text: string): Range[] {
  const cuts = [...text.matchAll(/;|\r?\n[ \t]*\r?\n/g)].map((m) => [m.index, m.index + m[0].length] as Range);
  return pieces(text, cuts);
}

/** What the policy judges a command by (`RiskFacts`). */
interface Facts {
  class: 'read' | 'write' | 'admin' | 'unknown';
  unbounded: boolean;
  /** The first target's name (a key, a collection). */
  target: string | null;
  /** A dropped database: its name, '' when unknown at planning. */
  dropsDatabase: string | null;
}

/** `policy()` of `crates/query/src/policy.rs` for the classes the fake knows (no "confirm unbounded" setting). */
function judge(f: Facts, env: EnvTag | null, readOnly: boolean, connectionName: string): { verdict: Verdict; confirm_name: string | null } {
  let verdict: Verdict = 'allow';
  if (f.class !== 'read') verdict = readOnly ? 'block' : env === 'production' ? 'confirm' : 'allow';
  if (verdict === 'allow') {
    if (readOnly && (f.unbounded || f.dropsDatabase !== null)) verdict = 'block';
    else if (f.unbounded && (env === 'production' || env === 'staging')) verdict = 'confirm';
    else if (f.dropsDatabase !== null) verdict = 'confirm';
  }
  if (env !== 'production' || verdict !== 'confirm') return { verdict, confirm_name: null };
  if (f.dropsDatabase !== null) return { verdict, confirm_name: f.dropsDatabase || connectionName };
  if (f.unbounded && f.target) return { verdict, confirm_name: f.target };
  if (f.class === 'unknown') return { verdict, confirm_name: f.target ?? connectionName };
  return { verdict, confirm_name: null };
}

const LABEL: Record<Facts['class'], StmtKindLabel> = { read: 'read', write: 'write', admin: 'admin', unknown: 'other' };

/** A parsed command of the fake: its facts and how it runs. */
interface Command {
  facts: Facts;
  run(): Outcome;
}

type Outcome =
  | { kind: 'rows'; columns: string[]; rows: Value[][] }
  | { kind: 'affected'; affected: number }
  | { kind: 'error'; message: string };

const text = (v: string): Value => ({ t: 'Text', v });
const column = (name: string): ColumnMeta => ({ name, db_type: 'string', logical: 'Text', nullable: true, origin_table: null, origin_column: null });

/** The words of a redis-cli line (double / single quotes group, no escapes beyond `\"`). */
function words(line: string): string[] {
  return [...line.matchAll(/"((?:[^"\\]|\\.)*)"|'([^']*)'|(\S+)/g)].map((m) => m[1] ?? m[2] ?? m[3] ?? '');
}

function redisCommand(state: CommandFakeState, db: number, database: string | null, line: string): Command {
  const [name = '', ...args] = words(line);
  const keys = (): Map<string, RedisValue> => {
    let m = state.redis.get(db);
    if (!m) state.redis.set(db, (m = new Map()));
    return m;
  };
  const facts = (cls: Facts['class'], over: Partial<Facts> = {}): Facts => ({
    class: cls,
    unbounded: false,
    target: args[0] ?? null,
    dropsDatabase: null,
    ...over,
  });
  switch (name.toUpperCase()) {
    case 'GET':
      return {
        facts: facts('read'),
        run: () => {
          const v = keys().get(args[0] ?? '');
          if (v && v.type !== 'string') return { kind: 'error', message: 'WRONGTYPE Operation against a key holding the wrong kind of value' };
          return { kind: 'rows', columns: ['value'], rows: [[v ? text(v.value) : { t: 'Null' }]] };
        },
      };
    case 'HGETALL':
      return {
        facts: facts('read'),
        run: () => {
          const v = keys().get(args[0] ?? '');
          const fields = v?.type === 'hash' ? Object.entries(v.value) : [];
          return { kind: 'rows', columns: ['field', 'value'], rows: fields.map(([f, x]) => [text(f), text(x)]) };
        },
      };
    case 'SET':
      return {
        facts: facts('write'),
        run: () => {
          keys().set(args[0] ?? '', { type: 'string', value: args[1] ?? '' });
          return { kind: 'affected', affected: 1 };
        },
      };
    case 'FLUSHDB':
      return {
        // Admin, dropping the session's database (its name: the tab's database, unknown without one).
        facts: facts('admin', { target: null, dropsDatabase: database ?? '' }),
        run: () => {
          const n = keys().size;
          keys().clear();
          return { kind: 'affected', affected: n };
        },
      };
    default:
      return { facts: facts('unknown'), run: () => ({ kind: 'error', message: `ERR unknown command '${name}'` }) };
  }
}

/** A document field as a result value (nested values as JSON; a missing field is Null). */
function fieldValue(v: unknown): Value {
  if (v === undefined || v === null) return { t: 'Null' };
  if (typeof v === 'string') return text(v);
  if (typeof v === 'boolean') return { t: 'Bool', v };
  if (typeof v === 'number') return Number.isInteger(v) ? { t: 'Int', v } : { t: 'Float', v };
  return { t: 'Json', v: JSON.stringify(v) };
}

/** Documents as rows: one column per top-level field, `_id` first, then in order of first appearance (spec core 7.3). */
function flatten(docs: Record<string, unknown>[]): Outcome {
  const columns = ['_id'];
  for (const d of docs) for (const k of Object.keys(d)) if (!columns.includes(k)) columns.push(k);
  return { kind: 'rows', columns, rows: docs.map((d) => columns.map((c) => fieldValue(d[c]))) };
}

function mongoCommand(state: CommandFakeState, database: string | null, source: string): Command {
  const m = /^db\.(?:([A-Za-z_][\w]*)|getCollection\(\s*("(?:[^"\\]|\\.)*")\s*\))\.(find|deleteMany)\(\s*(\{\s*\})?\s*\)(?:\.limit\(\s*(\d+)\s*\))?$/.exec(source.trim());
  const name = m ? (m[1] ?? (JSON.parse(m[2]!) as string)) : null;
  const docs = () => state.mongo.get(database ?? '')?.get(name ?? '') ?? [];
  if (m?.[3] === 'find') {
    const limit = m[5] === undefined || m[5] === '0' ? Infinity : Number(m[5]);
    return {
      facts: { class: 'read', unbounded: false, target: name, dropsDatabase: null },
      run: () => flatten(docs().slice(0, limit)),
    };
  }
  if (m?.[3] === 'deleteMany' && m[5] === undefined) {
    return {
      // An empty filter: every document of the collection.
      facts: { class: 'write', unbounded: m[4] !== undefined, target: name, dropsDatabase: null },
      run: () => {
        const list = docs();
        const n = list.length;
        list.splice(0, n);
        return { kind: 'affected', affected: n };
      },
    };
  }
  return {
    facts: { class: 'unknown', unbounded: false, target: name, dropsDatabase: null },
    run: () => ({ kind: 'error', message: 'MongoServerError: command not understood by the e2e fake' }),
  };
}

interface PlannedRun {
  connectionId: string;
  database: string | null;
  stmts: (PlanStmtView & { text: string; command: Command })[];
}

/** The statements of `ranges` a run target picks (the statement at the cursor, or before it). */
function pick(ranges: Range[], target: Target): Range[] {
  if (target === 'All') return ranges;
  if ('Selection' in target) {
    const [a, b] = target.Selection;
    return ranges.filter(([s, e]) => e > a && s < b);
  }
  const at = target.Cursor;
  const hit = ranges.find(([s, e]) => at >= s && at <= e) ?? [...ranges].reverse().find(([, e]) => e <= at) ?? ranges[0];
  return hit ? [hit] : [];
}

/**
 * `statement_ranges`, `query_plan` and `query_run` for Command connections. `connectionOf` finds a
 * stored connection, `requireOpen` throws `not_open`, `record` appends a History row.
 */
export function createCommandQueries(
  state: CommandFakeState,
  connectionOf: (id: string) => ConnectionConfig | undefined,
  requireOpen: (id: string) => unknown,
  record: (entry: Omit<HistoryEntry, 'id'>) => void,
): Record<string, Handler> {
  const plans = new Map<string, PlannedRun>();
  let seq = 0;
  const isCommand = (cfg: ConnectionConfig | undefined): cfg is ConnectionConfig => cfg?.kind === 'mongo' || cfg?.kind === 'redis';
  const rangesOf = (cfg: ConnectionConfig, text: string) => (cfg.kind === 'redis' ? redisRanges(text) : mongoRanges(text));
  const redisDb = (cfg: ConnectionConfig, database: string | null) =>
    database !== null && /^\d+$/.test(database) ? Number(database) : cfg.options.driver.kind === 'redis' ? cfg.options.driver.db : 0;

  return {
    statement_ranges: ({ text, connId }) => {
      const cfg = connId ? connectionOf(connId as string) : undefined;
      if (!isCommand(cfg)) throw new Error('e2e: statement_ranges is only faked for Command connections');
      return rangesOf(cfg, text as string);
    },

    query_plan: ({ connId, database, text, target }): PlanView => {
      const cfg = connectionOf(connId as string);
      if (!isCommand(cfg)) throw new Error('e2e: query_plan is only faked for Command connections');
      requireOpen(cfg.id);
      const db = (database as string | null) ?? null;
      const source = text as string;
      const stmts = pick(rangesOf(cfg, source), target as Target).map(([s, e], index) => {
        const piece = source.slice(s, e);
        const command = cfg.kind === 'redis' ? redisCommand(state, redisDb(cfg, db), db, piece) : mongoCommand(state, db ?? cfg.default_db, piece);
        const { verdict, confirm_name } = judge(command.facts, cfg.env_tag, cfg.read_only, cfg.name);
        return {
          index,
          range: [s, e] as Range,
          kind: LABEL[command.facts.class],
          verdict,
          preview: piece.replace(/\s+/g, ' ').slice(0, 120),
          unbounded: [],
          drops: [],
          confirm_name,
          estimate: null,
          implicit_commit: false,
          text: piece,
          command,
        };
      });
      const planId = `cmd-plan-${++seq}`;
      plans.set(planId, { connectionId: cfg.id, database: db, stmts });
      return {
        plan_id: planId,
        stmts: stmts.map(({ text: _text, command: _command, ...view }) => view),
        dialect: null,
        family: 'command',
        connection_name: cfg.name,
        env_tag: cfg.env_tag,
        read_only: cfg.read_only,
        tx_state: 'idle',
      };
    },

    // As `query_run`: Confirm needs `confirmed`, Production the exact `confirm_name`s; a plan runs once.
    query_run: ({ planId, confirmed, confirmNames }) => {
      const plan = plans.get(planId as string);
      if (!plan) throw fail('plan_expired');
      const confirm = plan.stmts.filter((s) => s.verdict === 'confirm');
      const names = [...new Set(confirm.flatMap((s) => (s.confirm_name ? [s.confirm_name] : [])))].sort();
      const given = [...((confirmNames as string[]) ?? [])].sort();
      if (confirm.length > 0 && (!confirmed || names.join('\n') !== given.join('\n'))) throw fail('confirmation_required');
      if (plan.stmts.some((s) => s.verdict === 'block')) throw fail('blocked');
      plans.delete(planId as string);
      const events: RunEvent[] = [];
      let resultId = 0;
      let stop: Extract<RunEvent, { t: 'done' }>['stop'] = null;
      for (const s of plan.stmts) {
        events.push({ t: 'statement_start', index: s.index, range: s.range });
        const outcome = s.command.run();
        const entry = {
          connection_id: plan.connectionId,
          database: plan.database,
          sql_text: s.text,
          redacted: false,
          kind: s.kind,
          started_at: new Date().toISOString(),
          elapsed_ms: 1,
          origin: 'query' as const,
          outcome_unknown: false,
        };
        if (outcome.kind === 'error') {
          events.push({ t: 'statement_error', index: s.index, error: appError('query', outcome.message) });
          record({ ...entry, status: 'error', row_count: null, affected: null, error_kind: 'query', error_code: null });
          stop = { kind: 'error', index: s.index };
          break;
        }
        const id = resultId++;
        if (outcome.kind === 'rows') {
          events.push({ t: 'result_start', result_id: id, index: s.index, columns: outcome.columns.map(column) });
          if (outcome.rows.length) events.push({ t: 'rows', result_id: id, rows: outcome.rows });
          events.push({ t: 'result_end', result_id: id, affected: null, row_count: outcome.rows.length, truncated: false, elapsed_ms: 1 });
          record({ ...entry, status: 'ok', row_count: outcome.rows.length, affected: null, error_kind: null, error_code: null });
        } else {
          events.push({ t: 'result_start', result_id: id, index: s.index, columns: [] });
          events.push({ t: 'result_end', result_id: id, affected: outcome.affected, row_count: 0, truncated: false, elapsed_ms: 1 });
          record({ ...entry, status: 'ok', row_count: null, affected: outcome.affected, error_kind: null, error_code: null });
        }
      }
      events.push({ t: 'done', stop, elapsed_ms: plan.stmts.length });
      return new WithChannel(`cmd-run-${++seq}`, 'channel', events);
    },
  };
}
