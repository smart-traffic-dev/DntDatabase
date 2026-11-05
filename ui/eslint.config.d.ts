// Typing for `eslint.config.js`, which `src/engine/engine.test.ts` loads to test the kind-comparison rule.
import type { Linter } from 'eslint';

declare const config: Linter.Config[];
export default config;
