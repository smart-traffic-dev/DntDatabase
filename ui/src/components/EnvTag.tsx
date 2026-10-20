import type { EnvTag as EnvTagValue } from '../bindings';

const LETTER: Record<EnvTagValue, string> = {
  production: 'P',
  staging: 'S',
  local: 'L',
};

/** A small letter badge: the reference palette has no per-environment colours. */
export function EnvTag({ env }: { env: EnvTagValue }) {
  return (
    <span className={`env-tag env-${env}`} title={`Environment: ${env}`} aria-label={`Environment: ${env}`}>
      {LETTER[env]}
    </span>
  );
}
