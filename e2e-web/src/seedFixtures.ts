import type { EnvTag, ObjectRef, SeedPreview, SeedRequest, SeedSample, SeedTarget, ServerInfo } from '../../ui/src/bindings';
import { seedPreview as basePreview, seedSample as baseSample, seedTarget as baseTarget } from '../../ui/src/test/fixtures/seed';

/**
 * The data seeder's canned answers for the browser tests (`specs/seed.spec.ts`): the columns of
 * `orders` with their inferred generators, a preview that follows the request, and a reviewed plan
 * whose confirmation follows the connection's environment and the "Existing rows" choice, as
 * `seed_prepare` in `src-tauri/src/seed_data/plan.rs` decides it.
 */

export const ordersSeedTarget = (env: EnvTag | null, server: ServerInfo, readOnly: boolean): SeedTarget =>
  baseTarget({ env, server, read_only: readOnly });

export const seedSample = (request: SeedRequest): SeedSample => baseSample(request);

const qualifiedRelation = (t: ObjectRef) => [t.database, t.schema, t.name].filter(Boolean).map((p) => `\`${p}\``).join('.');

export function seedPreview(request: SeedRequest, planId: string, env: EnvTag | null): SeedPreview {
  const rel = qualifiedRelation(request.table);
  const pre =
    request.existing === 'keep'
      ? null
      : request.existing === 'truncate'
        ? { sql: `TRUNCATE ${rel}`, irreversible: true, warning: 'Existing rows are deleted even if the job fails later.' }
        : { sql: `DELETE FROM ${rel}`, irreversible: false, warning: null };
  const confirm: SeedPreview['confirm'] =
    request.existing !== 'keep' || env === 'production'
      ? { kind: 'type_name', name: request.table.name }
      : env === 'staging'
        ? { kind: 'click' }
        : { kind: 'none' };
  return basePreview({
    plan_id: planId,
    table: request.table,
    rows: request.rows,
    batches: Math.ceil(request.rows / Math.max(1, request.batch_rows)),
    pre_step: pre,
    insert_head: `INSERT INTO ${rel} (\`customer_id\`, \`email\`, \`status\`, \`total\`, \`note\`) VALUES `,
    confirm,
    irreversible_name: env === 'production' && pre?.irreversible ? request.table.name : null,
  });
}
