// The `mongo` Monarch language of a MongoDB query tab (spec §7.3): mongosh-style scripts.
import type * as Monaco from 'monaco-editor';

/** Collection and cursor methods shown as `type.identifier` after a `.`. */
const METHODS = [
  'aggregate',
  'bulkWrite',
  'count',
  'countDocuments',
  'createIndex',
  'deleteMany',
  'deleteOne',
  'distinct',
  'drop',
  'dropIndex',
  'estimatedDocumentCount',
  'explain',
  'find',
  'findOne',
  'findOneAndDelete',
  'findOneAndReplace',
  'findOneAndUpdate',
  'getCollection',
  'getIndexes',
  'insertMany',
  'insertOne',
  'limit',
  'replaceOne',
  'runCommand',
  'skip',
  'sort',
  'updateMany',
  'updateOne',
];

export const mongoLanguage: Monaco.languages.IMonarchLanguage = {
  defaultToken: '',
  keywords: ['db'],
  predefined: ['ObjectId', 'ISODate'],
  methods: METHODS,
  tokenizer: {
    root: [
      { include: '@whitespace' },
      [/(\.)([A-Za-z_$][\w$]*)/, ['delimiter', { cases: { '@methods': 'type.identifier', '@default': 'identifier' } }]],
      [/\$[A-Za-z_][\w$]*/, 'keyword.operator'],
      [/[A-Za-z_][\w$]*/, { cases: { '@keywords': 'keyword', '@predefined': 'predefined', '@default': 'identifier' } }],
      [/\d+(?:\.\d+)?(?:[eE][-+]?\d+)?/, 'number'],
      [/"/, 'string', '@string_double'],
      [/'/, 'string', '@string_single'],
      [/`/, 'string', '@string_back'],
      [/[{}]/, 'delimiter.bracket'],
      [/[()]/, 'delimiter.parenthesis'],
      [/[[\]]/, 'delimiter.square'],
      [/[;,.:]/, 'delimiter'],
    ],
    whitespace: [
      [/\s+/, ''],
      [/\/\*/, 'comment', '@comment'],
      [/\/\/.*$/, 'comment'],
    ],
    comment: [
      [/[^/*]+/, 'comment'],
      [/\*\//, 'comment', '@pop'],
      [/[/*]/, 'comment'],
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
    string_back: [
      [/[^\\`]+/, 'string'],
      [/\\./, 'string.escape'],
      [/`/, 'string', '@pop'],
    ],
  },
};

/** Brackets and comments for the editor (auto-close, Ctrl+/). */
export const mongoConfiguration: Monaco.languages.LanguageConfiguration = {
  comments: { lineComment: '//', blockComment: ['/*', '*/'] },
  brackets: [
    ['{', '}'],
    ['[', ']'],
    ['(', ')'],
  ],
  autoClosingPairs: [
    { open: '{', close: '}' },
    { open: '[', close: ']' },
    { open: '(', close: ')' },
    { open: '"', close: '"', notIn: ['string'] },
    { open: "'", close: "'", notIn: ['string'] },
    { open: '`', close: '`', notIn: ['string'] },
  ],
};
