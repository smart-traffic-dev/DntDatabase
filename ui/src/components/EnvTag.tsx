import type { EnvTag as EnvTagValue } from '../bindings';
import { ENV_SHORT } from '../tabs/stripWidth';

const NAME: Record<EnvTagValue, string> = {
  production: 'Production',
  staging: 'Staging',
  local: 'Local',
};

/**
 * The environment of a connection, one look everywhere: a short badge PROD / STG / LCL (tree, tabs,
 * the same text as the strip's context chip), or with `variant="chip"` the name spelled out
 * (dialog headers). The reference palette has
 * no per-environment colours; only Production is coloured.
 */
export function EnvTag({ env, variant = 'badge', testId }: { env: EnvTagValue; variant?: 'badge' | 'chip'; testId?: string }) {
  const chip = variant === 'chip';
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
