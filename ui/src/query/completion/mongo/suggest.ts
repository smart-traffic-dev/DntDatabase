// The suggestions of a MongoDB console context (see `context.ts`), from the loaded names.
import { mongoCollectionMember } from '../../../engine';
import type { MongoAnalysis } from './context';
import {
  COLL_METHODS,
  DB_METHODS,
  HELPERS,
  MODIFIERS,
  QUERY_OPERATORS,
  SHOW_TARGETS,
  STAGES,
  STATEMENT_WORDS,
  UPDATE_OPERATORS,
  type Word,
} from './operators';

export interface MongoSuggestion {
  label: string;
  kind: 'keyword' | 'database' | 'collection' | 'view' | 'method' | 'field' | 'operator' | 'stage' | 'helper';
  detail?: string;
  insertText: string;
  /** `insertText` is a Monaco snippet. */
  snippet?: boolean;
}

export interface MongoObject {
  name: string;
  kind: 'Collection' | 'View';
}

export interface MongoField {
  name: string;
  /** The sampled BSON type (`string`, `objectId`, `mixed`, …). */
  type: string;
}

export interface MongoData {
  /** Collections and views of the database the context is about. */
  objects: MongoObject[];
  /** Sampled top-level fields of the collection the context is about. */
  fields: MongoField[];
  /** Database names (`use `). */
  databases: string[];
}

const JS_IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

function words(list: readonly Word[], kind: MongoSuggestion['kind']): MongoSuggestion[] {
  return list.map((w) =>
    w.snippet ? { label: w.label, kind, detail: w.detail, insertText: w.snippet, snippet: true } : { label: w.label, kind, detail: w.detail, insertText: w.label },
  );
}

function objectItem(o: MongoObject, insertText: string): MongoSuggestion {
  const view = o.kind === 'View';
  return { label: o.name, kind: view ? 'view' : 'collection', detail: view ? 'View' : 'Collection', insertText };
}

/** A key: as is when it is an identifier, else quoted. */
const keyText = (name: string) => (JS_IDENT.test(name) && !name.startsWith('$') ? name : JSON.stringify(name));

function fieldItems(fields: MongoField[]): MongoSuggestion[] {
  return fields.map((f) => ({ label: f.name, kind: 'field', detail: f.type, insertText: keyText(f.name) }));
}

/** Whether the context needs the fields of its collection (the provider loads them only then). */
export function wantsFields(a: MongoAnalysis): boolean {
  const c = a.context;
  return c.kind === 'key' && (c.role === 'query' || c.role === 'fields') && !a.prefix.startsWith('$');
}

/** What to offer for `a`; empty when nothing fits (inside a string, after a complete value, …). */
export function suggestMongo(a: MongoAnalysis, data: MongoData): MongoSuggestion[] {
  const c = a.context;
  const dollar = a.prefix.startsWith('$');
  switch (c.kind) {
    case 'none':
      return [];
    case 'statement':
      return words(STATEMENT_WORDS, 'keyword');
    case 'use':
      return data.databases.map((d) => ({ label: d, kind: 'database', insertText: d }));
    case 'show':
      return words(SHOW_TARGETS, 'keyword');
    case 'db': {
      // The console takes one `getSiblingDB(…)` per statement.
      const methods = c.db === null ? DB_METHODS : DB_METHODS.filter((m) => m.label !== 'getSiblingDB');
      return [...data.objects.map((o) => objectItem(o, mongoCollectionMember(o.name))), ...words(methods, 'method')];
    }
    case 'collection': {
      if (c.explained) return words(COLL_METHODS, 'method');
      // `db.system.` also continues a dotted name (`system.users`).
      const dotted = data.objects.flatMap((o) => {
        const rest = o.name.startsWith(`${c.coll}.`) ? o.name.slice(c.coll.length + 1) : null;
        return rest && JS_IDENT.test(rest) ? [{ ...objectItem(o, rest), label: rest }] : [];
      });
      return [...words(COLL_METHODS, 'method'), ...dotted];
    }
    case 'modifier':
      // `.count()` follows a `find()` only (".count() is not valid after aggregate()").
      return words(c.countable ? MODIFIERS : MODIFIERS.filter((m) => m.label !== 'count'), 'method');
    case 'value':
      return words(HELPERS, 'helper');
    case 'key':
      switch (c.role) {
        case 'query':
          return dollar ? words(QUERY_OPERATORS, 'operator') : fieldItems(data.fields);
        case 'operators':
          return a.prefix === '' || dollar ? words(QUERY_OPERATORS, 'operator') : [];
        case 'update':
          return a.prefix === '' || dollar ? words(UPDATE_OPERATORS, 'operator') : [];
        case 'fields':
          return dollar ? [] : fieldItems(data.fields);
        case 'stage':
          return a.prefix === '' || dollar ? words(STAGES, 'stage') : [];
        default:
          return [];
      }
    default:
      return [];
  }
}
