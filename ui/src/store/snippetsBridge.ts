/**
 * Where the user's snippets go once loaded or changed: the editor's completion.
 *
 * On this branch it is a no-op. At merge, the controller points it at
 * `query/completion/snippets.ts` (`setUserSnippets`), which another branch adds.
 */
export interface UserSnippet {
  prefix: string;
  label: string;
  body: string;
  description: string;
}

export const setUserSnippets: (list: UserSnippet[]) => void = () => {};
