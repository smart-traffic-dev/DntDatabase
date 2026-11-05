// The Monaco languages of the Command family (ids from `editorLanguage`), registered in code.
import type * as Monaco from 'monaco-editor';
import { editorLanguage } from '..';
import { mongoConfiguration, mongoLanguage } from './mongo';
import { redisConfiguration, redisLanguage } from './redis';

/** Monaco instances the languages are registered on (a WeakSet: the real monaco is a module namespace). */
const registered = new WeakSet<object>();

/** Registers `mongo` and `redis` once per monaco instance; call before an editor creates its model. */
export function registerCommandLanguages(monaco: typeof Monaco): void {
  if (registered.has(monaco)) return;
  registered.add(monaco);
  const languages: [string, Monaco.languages.IMonarchLanguage, Monaco.languages.LanguageConfiguration][] = [
    [editorLanguage('mongo'), mongoLanguage, mongoConfiguration],
    [editorLanguage('redis'), redisLanguage, redisConfiguration],
  ];
  for (const [id, language, configuration] of languages) {
    monaco.languages.register({ id });
    monaco.languages.setMonarchTokensProvider(id, language);
    monaco.languages.setLanguageConfiguration(id, configuration);
  }
}
