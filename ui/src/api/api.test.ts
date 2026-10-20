import { beforeEach, describe, expect, it } from 'vitest';
import { AppErrorEx, api, isPasswordRequired } from '.';
import { backendError, fail, mockBackend } from '../test/backend';

describe('api', () => {
  beforeEach(() => {
    mockBackend({});
  });

  it('throws AppErrorEx carrying the AppError when a command returns an error', async () => {
    mockBackend({
      list_tree: () => {
        throw fail('server_db_missing');
      },
    });
    const err = await api.listTree().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AppErrorEx);
    expect((err as AppErrorEx).kind).toBe('server');
    expect((err as AppErrorEx).message).toBe("Unknown database 'shop'");
    expect((err as AppErrorEx).server_code).toBe('1049');
    expect((err as AppErrorEx).stage).toBe('connect_db');
    expect((err as AppErrorEx).timings).toEqual([{ stage: 'resolve_secrets', ms: 1 }]);
    expect((err as AppErrorEx).toAppError()).toEqual(backendError('server_db_missing'));
  });

  it('recognizes the real password_required error', async () => {
    mockBackend({
      list_tree: () => {
        throw fail('password_required');
      },
    });
    const err = (await api.listTree().catch((e: unknown) => e)) as AppErrorEx;
    expect(err.message).toBe('A password is required.');
    expect(isPasswordRequired(err)).toBe(true);
    expect(isPasswordRequired(backendError('internal'))).toBe(false);
  });

  it('resolves to the value of a successful command and sends its arguments', async () => {
    const mock = mockBackend({ list_databases: () => [{ name: 'shop' }] });
    await expect(api.listDatabases('c1', true)).resolves.toEqual([{ name: 'shop' }]);
    expect(mock).toHaveBeenCalledWith('list_databases', { id: 'c1', showSystem: true });
  });

  it('turns a plain string rejection (unknown command, bad arguments) into an internal error', async () => {
    mockBackend({
      list_tree: () => {
        throw 'command list_tree not found';
      },
    });
    const err = await api.listTree().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AppErrorEx);
    expect((err as AppErrorEx).kind).toBe('internal');
    expect((err as AppErrorEx).message).toBe('command list_tree not found');
  });

  it('turns an object that is not an AppError into an internal error with its text', async () => {
    mockBackend({
      list_tree: () => {
        throw { code: 42 };
      },
    });
    const err = await api.listTree().catch((e: unknown) => e);
    expect((err as AppErrorEx).kind).toBe('internal');
    expect((err as AppErrorEx).message).toBe('[object Object]');
  });

  it('wraps a runtime failure (no Tauri backend) as an internal AppErrorEx', async () => {
    mockBackend({
      list_tree: () => {
        throw new TypeError('invoke is not available');
      },
    });
    const err = await api.listTree().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AppErrorEx);
    expect((err as AppErrorEx).kind).toBe('internal');
    expect((err as AppErrorEx).message).toBe('invoke is not available');
  });
});
