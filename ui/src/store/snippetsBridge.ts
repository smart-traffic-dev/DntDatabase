/** Where the user's snippets go once loaded or changed: the editor's completion. */
import type { SqlSnippet } from '../query/completion/snippets';

export type UserSnippet = SqlSnippet;

export { setUserSnippets } from '../query/completion/snippets';
