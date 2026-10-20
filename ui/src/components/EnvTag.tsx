import type { EnvTag as EnvTagValue } from '../bindings';

const LETTER: Record<EnvTagValue, string> = {
  production: 'P',
  staging: 'S',
  local: 'L',
};

const NAME: Record<EnvTagValue, string> = {
  production: 'Production',
  staging: 'Staging',
  local: 'Local',
};

/**
 * The environment of a connection, one look everywhere: a small letter badge (tree, tabs),
 * or with `variant="chip"` the name spelled out (dialog headers). The reference palette has
 * no per-environment colours; only Production is coloured.
 */
export function EnvTag({ env, variant = 'letter', testId }: { env: EnvTagValue; variant?: 'letter' | 'chip'; testId?: string }) {
  const chip = variant === 'chip';
  return (
    <span
      className={`env-tag env-${env}${chip ? ' env-chip' : ''}`}
      title={`Environment: ${env}`}
      aria-label={chip ? undefined : `Environment: ${env}`}
      data-testid={testId}
    >
      {chip ? NAME[env] : LETTER[env]}
    </span>
  );
}
