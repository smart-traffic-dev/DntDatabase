import type {
  ConnectionConfig,
  DdlEditOpen,
  DdlEditPreview,
  DdlEditReport,
  ObjectKind,
  ObjectRef,
  ObjectSummary,
  Scope,
  ServerInfo,
  Strategy,
} from '../../ui/src/bindings';
import { Failure, appError, fail } from '../../ui/src/test/fixtures/data';
import type { Handler } from './shim';

type Engine = 'mysql' | 'postgres';

/**
 * What a spec can change in the fake DDL editor: `objects` holds the server's definitions by
 * `engine/kind/name` (changing one after an edit opened makes its preview stale); `partial` makes
 * the next DROP + CREATE fail after the DROP (the object is then missing).
 */
export interface DdlFakeState {
  objects: Map<string, string>;
  partial: boolean;
}

const keyOf = (engine: Engine, kind: ObjectKind, name: string) => `${engine}/${kind}/${name}`;

/** The canned objects: MySQL 8 view `v_orders` and procedure `p_restock`, PostgreSQL 17 function `order_total`. */
const OBJECTS: [Engine, ObjectKind, string, string][] = [
  ['mysql', 'View', 'v_orders', 'CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v_orders` AS select `orders`.`id` AS `id`,`orders`.`total` AS `total` from `orders`'],
  ['mysql', 'Procedure', 'p_restock', 'CREATE PROCEDURE `p_restock`(IN qty int)\nBEGIN\n  UPDATE orders SET total = total + qty;\nEND'],
  ['postgres', 'Function', 'order_total', 'CREATE OR REPLACE FUNCTION public.order_total(o bigint)\n RETURNS numeric\n LANGUAGE sql\nAS $function$ SELECT total FROM orders WHERE id = o $function$'],
];

const TEMPLATES: Partial<Record<ObjectKind, string>> = {
  View: 'CREATE VIEW new_view AS\nSELECT 1 AS one',
  Function: 'CREATE FUNCTION new_function() RETURNS integer\nLANGUAGE sql\nAS $$ SELECT 1 $$',
  Procedure: 'CREATE PROCEDURE new_procedure()\nBEGIN\n  SELECT 1;\nEND',
};

/** A MySQL routine, trigger or event is replaced by DROP + CREATE; anything else in place. */
const strategyOf = (engine: Engine, kind: ObjectKind): Strategy => (engine === 'mysql' && kind !== 'View' ? 'recreate' : 'replace');

/** The name the text creates (enough for the canned texts and templates). */
const nameIn = (text: string): string | null =>
  /\b(?:VIEW|FUNCTION|PROCEDURE|TRIGGER|EVENT)\s+(?:IF NOT EXISTS\s+)?(?:[`"]?\w+[`"]?\.)?[`"]?(\w+)/i.exec(text)?.[1] ?? null;

const lostEdit = () => new Failure(appError('config', 'unknown edit', { field: 'edit_id' }));

interface Edit {
  connectionId: string;
  engine: Engine;
  scope: Scope;
  kind: ObjectKind;
  name: string | null;
  /** The server's text when the edit opened. */
  base: string;
}

interface Plan {
  edit: Edit;
  text: string;
  statements: string[];
  confirmName: string | null;
}

const report = (over: Partial<DdlEditReport>): DdlEditReport => ({
  outcome: 'applied',
  error: null,
  warnings: [],
  statements_sent: 0,
  executed: [],
  failed: null,
  object_state: 'unchanged',
  restore_text: null,
  object: null,
  text: null,
  ...over,
});

/** The DDL editor commands (`ddl_edit_*`) plus the views / routines the tree lists and `object_ddl` shows. */
export function createDdlFake(requireOpen: (id: string) => ServerInfo, configOf: (id: string) => ConnectionConfig | undefined) {
  const state: DdlFakeState = { objects: new Map(OBJECTS.map(([e, k, n, t]) => [keyOf(e, k, n), t])), partial: false };
  const edits = new Map<string, Edit>();
  const plans = new Map<string, Plan>();
  let seq = 0;
  const engineOf = (id: string): Engine => (requireOpen(id).variant === 'Postgres' ? 'postgres' : 'mysql');

  const listObjects = (id: string, kind: ObjectKind): ObjectSummary[] => {
    const prefix = `${engineOf(id)}/${kind}/`;
    return [...state.objects.keys()].filter((k) => k.startsWith(prefix)).map((k) => ({ name: k.slice(prefix.length), kind, comment: null, row_estimate: null }));
  };

  const objectDdl = (id: string, obj: ObjectRef): string => state.objects.get(keyOf(engineOf(id), obj.kind, obj.name)) ?? '';

  const handlers: Record<string, Handler> = {
    ddl_edit_open: ({ connectionId, scope, kind, name, table }): DdlEditOpen => {
      const id = connectionId as string;
      const server = requireOpen(id);
      const engine = engineOf(id);
      const k = kind as ObjectKind;
      const n = (name as string | null) ?? null;
      const text = n === null ? (TEMPLATES[k] ?? `CREATE ${k.toUpperCase()} new_object`) : state.objects.get(keyOf(engine, k, n));
      if (text === undefined) throw new Failure(appError('config', `e2e: no canned ${k} ${n}`, { field: 'name' }));
      const editId = `ddl-edit-${++seq}`;
      const s = scope as Scope;
      edits.set(editId, { connectionId: id, engine, scope: s, kind: k, name: n, base: text });
      const cfg = configOf(id);
      const strategy = strategyOf(engine, k);
      return {
        edit_id: editId,
        text,
        target: { kind: k, database: s.database, schema: s.schema, name: n, table: (table as string | null) ?? null },
        object: n === null ? null : { database: s.database, schema: s.schema, name: n, kind: k },
        server,
        env: cfg?.env_tag ?? null,
        read_only: !!cfg?.read_only,
        strategies: [strategy],
        strategy,
        blocked_reason: cfg?.read_only ? 'The connection is read-only.' : null,
        notes: [],
      };
    },
    ddl_edit_preview: ({ editId, text }): DdlEditPreview => {
      const edit = edits.get(editId as string);
      if (!edit) throw lostEdit();
      if (edit.name !== null && state.objects.get(keyOf(edit.engine, edit.kind, edit.name)) !== edit.base) throw fail('plan_stale');
      const body = text as string;
      const recreate = edit.name !== null && strategyOf(edit.engine, edit.kind) === 'recreate';
      const statements = recreate ? [`DROP ${edit.kind.toUpperCase()} IF EXISTS \`${edit.name}\``, body] : [body];
      const planId = `ddl-plan-${++seq}`;
      const confirmName = configOf(edit.connectionId)?.env_tag === 'production' ? (edit.name ?? nameIn(body)) : null;
      plans.set(planId, { edit, text: body, statements, confirmName });
      return {
        plan_id: planId,
        statements,
        warnings: recreate ? ['Between the DROP and the CREATE the object does not exist.'] : [],
        checks: [],
        transactional: edit.engine === 'postgres',
        gap: recreate,
        confirm_name: confirmName,
        expires_in_seconds: 600,
      };
    },
    ddl_edit_apply: ({ planId, confirmed, name }): DdlEditReport => {
      const plan = plans.get(planId as string);
      if (!plan) throw fail('plan_stale');
      if (!confirmed || (plan.confirmName !== null && name !== plan.confirmName)) return report({ outcome: 'not_applied' });
      plans.delete(planId as string);
      const { edit } = plan;
      if (state.partial && plan.statements.length > 1 && edit.name !== null) {
        state.objects.delete(keyOf(edit.engine, edit.kind, edit.name));
        return report({
          outcome: 'partial',
          error: appError('query', 'You have an error in your SQL syntax'),
          statements_sent: 2,
          executed: plan.statements.slice(0, 1),
          failed: plan.statements[1]!,
          object_state: 'missing',
          restore_text: edit.base,
        });
      }
      const created = edit.name ?? nameIn(plan.text) ?? 'new_object';
      state.objects.set(keyOf(edit.engine, edit.kind, created), plan.text);
      edit.name = created;
      edit.base = plan.text;
      return report({
        statements_sent: plan.statements.length,
        executed: edit.engine === 'mysql' ? plan.statements : [],
        object: { database: edit.scope.database, schema: edit.scope.schema, name: created, kind: edit.kind },
        text: plan.text,
      });
    },
    ddl_edit_cancel: () => null,
    ddl_edit_close: ({ editId }) => {
      edits.delete(editId as string);
      return null;
    },
  };

  return { state, handlers, listObjects, objectDdl };
}
