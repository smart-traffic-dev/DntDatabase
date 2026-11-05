// The `redis` Monarch language of a Redis query tab (spec §7.3): one redis-cli command per line.
import type * as Monaco from 'monaco-editor';

export const redisLanguage: Monaco.languages.IMonarchLanguage = {
  defaultToken: '',
  ignoreCase: true,
  tokenizer: {
    root: [
      // `^`: only at the start of a line. The command is the first word, whatever its case.
      [/^\s*[A-Za-z][^\s"'#]*/, 'keyword'],
      [/\s+/, ''],
      [/#.*$/, 'comment'],
      [/"/, 'string', '@string_double'],
      [/'/, 'string', '@string_single'],
      [/-?\d+(?:\.\d+)?(?=\s|$)/, 'number'],
      [/[^\s"']+/, 'identifier'],
    ],
    string_double: [
      [/[^\\"]+/, 'string'],
      [/\\./, 'string.escape'],
      [/"/, 'string', '@pop'],
    ],
    string_single: [
      [/[^\\']+/, 'string'],
      [/\\./, 'string.escape'],
      [/'/, 'string', '@pop'],
    ],
  },
};

/** Quotes auto-close; `#` is the line comment (Ctrl+/). */
export const redisConfiguration: Monaco.languages.LanguageConfiguration = {
  comments: { lineComment: '#' },
  autoClosingPairs: [
    { open: '"', close: '"', notIn: ['string'] },
    { open: "'", close: "'", notIn: ['string'] },
  ],
};
