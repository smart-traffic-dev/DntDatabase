import { renderHook } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../store/connections';
import { config, ids, resetStores, serverInfo } from '../test/backend';
import { defaultCaps } from './index';
import { capsOfConnection, useCaps } from './useCaps';

describe('useCaps', () => {
  beforeEach(() => {
    resetStores();
    useConnections.getState().setStored([], [config(ids.prod, 'docs', { kind: 'mongo', port: 27017 })]);
  });

  it('caps_before_connect_use_defaults', () => {
    const { result } = renderHook(() => useCaps(ids.prod));
    expect(result.current).toEqual(defaultCaps('mongo'));
    expect(capsOfConnection(ids.prod)).toEqual(defaultCaps('mongo'));
  });

  it('the server report wins once the connection is open', () => {
    const reported = { ...defaultCaps('mongo'), explain: true };
    useConnections.getState().setInfo(ids.prod, { ...serverInfo('MySql8'), kind: 'mongo', variant: 'Mongo', capabilities: reported });
    const { result } = renderHook(() => useCaps(ids.prod));
    expect(result.current).toEqual(reported);
  });

  it('is null without a connection or for a deleted one', () => {
    expect(renderHook(() => useCaps(null)).result.current).toBeNull();
    expect(renderHook(() => useCaps('00000000-0000-0000-0000-000000000000')).result.current).toBeNull();
    expect(capsOfConnection(null)).toBeNull();
  });
});
