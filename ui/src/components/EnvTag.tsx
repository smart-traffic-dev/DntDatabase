import type { DbKind, EnvTag as EnvTagValue } from '../bindings';
import { ENV_SHORT } from '../tabs/stripWidth';
import { EngineLogo } from './EngineLogo';

/**
 * The head of a context chip (query and object tabs): the engine's logo, then the environment
 * where it needs care (PROD badge, staging dot); Local adds nothing.
 */
export function ContextMark({ env, kind, testId }: { env: EnvTagValue | null; kind: DbKind | null | undefined; testId?: string }) {
  return (
    <>
      {kind ? <EngineLogo engine={kind} size={16} compact /> : null}
      {env === 'production' ? (
        <span className="qt-env env-production" data-testid={testId}>
          {ENV_SHORT.production}
        </span>
      ) : env === 'staging' ? (
        <EnvTag env="staging" testId={testId} />
      ) : null}
    </>
  );
}

const NAME: Record<EnvTagValue, string> = {
  production: 'Production',
  staging: 'Staging',
  local: 'Local',
};

/**
 * The environment of a connection. With `variant="chip"` the name spelled out (dialog headers).
 * The compact badge (tree, tabs, the strip's context chip) only marks what needs care: Production
 * is a red PROD badge, Staging an amber dot (named in its tooltip), and Local, the safe default,
 * shows nothing ("LCL" read as noise).
 */
export function EnvTag({ env, variant = 'badge', testId }: { env: EnvTagValue; variant?: 'badge' | 'chip'; testId?: string }) {
  const chip = variant === 'chip';
  if (!chip && env === 'local') return null;
  if (!chip && env === 'staging') {
    return <span className="env-dot env-staging" role="img" aria-label="Environment: staging" title="Environment: staging" data-testid={testId} />;
  }
  return (
    <span
      className={`env-tag env-${env}${chip ? ' env-chip' : ''}`}
      title={`Environment: ${env}`}
      aria-label={chip ? undefined : `Environment: ${env}`}
      data-testid={testId}
    >
      {chip ? NAME[env] : ENV_SHORT[env]}
    </span>
  );
}
