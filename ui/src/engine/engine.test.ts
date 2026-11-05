import { Linter } from 'eslint';
import { describe, expect, it } from 'vitest';
import eslintConfig from '../../eslint.config.js';
import type { DbKind, ServerInfo } from '../bindings';
import { sqlCaps } from '../test/fixtures/caps';
import { capsOf, defaultCaps, editorLanguage, engineChip, familyOf, sqlEngine } from './index';

const KINDS: DbKind[] = ['mysql', 'postgres', 'mongo', 'redis'];

describe('engine', () => {
  it('every_kind_has_a_family', () => {
    expect(KINDS.map(familyOf)).toEqual(['sql', 'sql', 'command', 'command']);
  });

  it('sql_engine_has_no_fallback', () => {
    expect(sqlEngine('redis')).toBeNull();
    expect(sqlEngine('mongo')).toBeNull();
    expect(sqlEngine('postgres')).toBe('postgres');
    expect(sqlEngine('mysql')).toBe('mysql');
  });

  it('default_caps_match_spec', () => {
    expect(defaultCaps('redis').paged_objects).toBe(true);
    expect(defaultCaps('mongo').designer).toBe(false);
    expect(defaultCaps('mongo').transactions).toBe(false);
    expect(defaultCaps('mysql')).toEqual(sqlCaps('db'));
    expect(defaultCaps('postgres')).toEqual(sqlCaps('db_schema'));
  });

  it('caps_prefer_server_info', () => {
    const reported = { ...defaultCaps('redis'), paged_objects: false };
    const info = { kind: 'redis', capabilities: reported } as ServerInfo;
    expect(capsOf(info, 'redis')).toBe(reported);
    expect(capsOf(null, 'redis')).toEqual(defaultCaps('redis'));
    expect(capsOf(undefined, 'mysql')).toEqual(defaultCaps('mysql'));
  });

  it('chips_and_languages', () => {
    expect(engineChip('mongo')).toBe('MG');
    expect(engineChip('postgres')).toBe('PG');
    expect(editorLanguage('redis')).toBe('redis');
    expect(editorLanguage('postgres')).toBe('pgsql');
  });

  it('eslint_rule_flags_kind_literals', () => {
    const linter = new Linter({ configType: 'flat' });
    const run = (code: string, filename: string) =>
      linter.verify(code, eslintConfig, { filename }).filter((m) => m.ruleId === 'no-restricted-syntax');
    const code = "declare const c: { kind: string };\nif (c.kind === 'redis') {}";
    const hits = run(code, 'src/x.ts');
    expect(hits).toHaveLength(1);
    expect(hits[0]?.message).toBe('Compare engine kinds only inside ui/src/engine/ (use familyOf/capsOf or a switch there).');
    expect(run("if ('mysql' != k) {}", 'src/x.ts')).toHaveLength(1);
    expect(run("if (k === 'Postgres') {}", 'src/x.ts')).toHaveLength(0);
    expect(run(code, 'src/engine/x.ts')).toHaveLength(0);
    expect(run(code, 'src/x.test.ts')).toHaveLength(0);
  });
});
