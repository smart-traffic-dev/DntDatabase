import js from '@eslint/js';
import globals from 'globals';
import reactHooks from 'eslint-plugin-react-hooks';
import tseslint from 'typescript-eslint';

const KINDS = "/^(mysql|postgres|mongo|redis)$/";
// `x === 'redis'`, `'redis' !== x`, and the loose forms: one side is a kind literal.
const KIND_COMPARISON =
  `BinaryExpression[operator=/^[!=]==?$/]:matches([left.type='Literal'][left.value=${KINDS}], [right.type='Literal'][right.value=${KINDS}])`;
const KIND_MESSAGE =
  'Compare engine kinds only inside ui/src/engine/ (use familyOf/capsOf or a switch there).';

export default tseslint.config(
  { ignores: ['dist', 'src/bindings.ts'] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ['**/*.{ts,tsx}'],
    languageOptions: { globals: globals.browser },
    plugins: { 'react-hooks': reactHooks },
    rules: {
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'warn',
      'no-restricted-syntax': [
        'error',
        {
          selector: KIND_COMPARISON,
          message: KIND_MESSAGE,
        },
      ],
    },
  },
  {
    files: ['src/engine/**', '**/*.test.{ts,tsx}'],
    rules: { 'no-restricted-syntax': 'off' },
  },
);
