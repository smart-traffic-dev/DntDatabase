import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import type { ConnectionConfig, EditPreview, KeyName, KeyPage, KeySummary, KeyType, KeyValueView } from '../../bindings';
import { useConnections } from '../../store/connections';
import { KEY_GONE, useKeysTabs } from '../../store/keysTabs';
import { openKeysTab, useTabs, type KeysTabRef } from '../../store/tabs';
import { useTree } from '../../store/tree';
import { connNodeId } from '../../store/treeModel';
import { callsOf, config, fail, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { bytesToBase64, textBytes } from './keyEdits';
import { KeysTab } from './KeysTab';

const b64 = (s: string) => textBytes(s);
const name = (s: string): KeyName => ({ display: s, raw: b64(s) });
const summary = (key: KeyName | string, key_type: KeyType = 'String', ttl_ms: number | null = null): KeySummary => ({
  name: typeof key === 'string' ? name(key) : key,
  key_type,
  ttl_ms,
});
const page = (keys: KeySummary[], next_cursor: string | null = null): KeyPage => ({ keys, next_cursor });

const preview = (over: Partial<EditPreview> = {}): EditPreview => ({
  plan_id: 'plan-1',
  statements: ['HSET user:1 name Eve'],
  verdict: 'allow',
  atomic: 'yes',
  protections: [{ level: 'key_only' }],
  reason: null,
  ...over,
});

const committed = () => ({ outcome: { outcome: 'committed' }, statements: [{ index: 0, affected: 1, error: null, conflict: false }], inserted: [], error: null });

const hashOf = (fields: [string, string][], ttl_ms: number | null = null): KeyValueView => ({
  value: { Hash: { fields: fields.map(([f, v]) => [{ t: 'Text', v: f }, { t: 'Text', v }]), total: fields.length, next: null } },
  ttl_ms,
  memory_bytes: 1229,
});

function setup(handlers: Record<string, (args: Record<string, unknown>) => unknown>, over: Partial<ConnectionConfig> = {}) {
  mockBackend({ tabs_save: () => null, ...handlers });
  useConnections.getState().setStored([], [config(ids.prod, 'cache', { kind: 'redis', port: 6379, ...over })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('Redis'));
  openKeysTab(ids.prod, '0');
  const tab = useTabs.getState().tabs[0] as KeysTabRef;
  render(<KeysTab tab={tab} />);
  return tab;
}

const keyRows = () => within(screen.getByRole('listbox', { name: 'Keys' })).queryAllByRole('option');
const valuePane = () => screen.getByRole('region', { name: 'Value' });

beforeEach(() => {
  resetStores();
  useKeysTabs.getState().reset();
});

describe('KeysTab', () => {
  it('scan_pages_with_load_more', async () => {
    let n = 0;
    setup({ keys_scan: () => (++n === 1 ? page([summary('a'), summary('b', 'Hash', 725_000)], '2') : page([summary('c', 'ZSet')])) });
    await waitFor(() => expect(keyRows()).toHaveLength(2));
    expect(keyRows()[1]).toHaveTextContent('b');
    expect(keyRows()[1]).toHaveTextContent('Hash');
    expect(keyRows()[1]).toHaveTextContent('12m 5s');
    expect(keyRows()[0]).toHaveTextContent('no TTL');
    expect(callsOf('keys_scan')[0]).toEqual({ id: ids.prod, db: '0', req: { cursor: null, pattern: '*', key_type: null, count: 200 } });
    expect(screen.getByTestId('keys-count')).toHaveTextContent('2 keys loaded');

    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    await waitFor(() => expect(keyRows()).toHaveLength(3));
    expect(callsOf('keys_scan')).toHaveLength(2);
    expect(callsOf('keys_scan')[1]!.req).toMatchObject({ cursor: '2' });
    expect(keyRows()[2]).toHaveTextContent('Sorted set');
    expect(screen.queryByRole('button', { name: 'Load more' })).toBeNull();
    expect(screen.getByTestId('keys-count')).toHaveTextContent('3 keys loaded');

    // The tree knows the database's total: it is shown too.
    const node = `${connNodeId(ids.prod)}/db:0`;
    act(() => useTree.setState({ nodes: new Map([[node, { id: node, kind: 'keyspace-db', label: 'db0 · 1,234 keys', keyCount: 1234 }]]) }));
    expect(screen.getByTestId('keys-count')).toHaveTextContent('3 of 1,234 keys loaded');
  });

  it('pattern_rescans_from_start', async () => {
    setup({
      keys_scan: (args) => {
        const req = args.req as { cursor: string | null; pattern: string | null };
        if (req.pattern === 'user:*') return page([summary('user:1', 'Hash')]);
        return req.cursor === null ? page([summary('a')], '1') : page([summary('b')], '2');
      },
    });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    await waitFor(() => expect(keyRows()).toHaveLength(2));

    const pattern = screen.getByRole('searchbox', { name: 'Key pattern' });
    fireEvent.change(pattern, { target: { value: 'user:*' } });
    expect(callsOf('keys_scan')).toHaveLength(2);
    fireEvent.keyDown(pattern, { key: 'Enter' });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    expect(keyRows()[0]).toHaveTextContent('user:1');
    expect(callsOf('keys_scan').at(-1)!.req).toEqual({ cursor: null, pattern: 'user:*', key_type: null, count: 200 });
    expect(screen.queryByRole('button', { name: 'Load more' })).toBeNull();

    // The type filter rescans from the start as well.
    fireEvent.change(screen.getByRole('combobox', { name: 'Key type' }), { target: { value: 'ZSet' } });
    await waitFor(() => expect(callsOf('keys_scan').at(-1)!.req).toEqual({ cursor: null, pattern: 'user:*', key_type: 'ZSet', count: 200 }));
  });

  it('hash_value_renders_and_edits', async () => {
    let fields: [string, string][] = [
      ['email', 'ada@example.com'],
      ['name', 'Ada'],
    ];
    setup({
      keys_scan: () => page([summary('user:1', 'Hash')]),
      key_value: () => hashOf(fields, 90_000),
      key_edit_preview: () => preview(),
      data_apply: () => {
        fields = [
          ['email', 'ada@example.com'],
          ['name', 'Eve'],
        ];
        return committed();
      },
    });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    await waitFor(() => expect(within(pane).getByText('Ada')).toBeInTheDocument());
    expect(callsOf('key_value')[0]).toEqual({ id: ids.prod, db: '0', key: name('user:1'), range: { offset: 0, count: 200 } });
    expect(within(pane).getByTestId('key-ttl')).toHaveTextContent('TTL: 1m 30s');
    expect(within(pane).getByText('1.2 KB')).toBeInTheDocument();

    fireEvent.click(within(pane).getByRole('button', { name: 'Edit name' }));
    const input = within(pane).getByRole('textbox', { name: 'Value of name' });
    fireEvent.change(input, { target: { value: 'Eve' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    await waitFor(() => expect(callsOf('key_edit_preview')).toHaveLength(1));
    expect(callsOf('key_edit_preview')[0]).toEqual({
      id: ids.prod,
      db: '0',
      edit: { op: 'hash_set', key: { display: 'user:1', raw: b64('user:1') }, field: b64('name'), value: b64('Eve') },
    });

    const dialog = await screen.findByTestId('edit-preview');
    expect(within(dialog).getByLabelText('Summary')).toHaveTextContent('1 command');
    expect(dialog).toHaveTextContent('HSET user:1 name Eve');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(screen.queryByTestId('edit-preview')).toBeNull());
    expect(callsOf('data_apply')[0]).toEqual({ planId: 'plan-1', confirmed: true });
    // The value reloads and shows the new field value.
    await waitFor(() => expect(within(valuePane()).getByText('Eve')).toBeInTheDocument());
    expect(callsOf('key_value')).toHaveLength(2);
  });

  it('keys_with_same_display_stay_distinct', async () => {
    // Two keys the server stores differently but that display alike: `a\xffb`.
    const binary: KeyName = { display: 'a\\xffb', raw: bytesToBase64(Uint8Array.from([0x61, 0xff, 0x62])) };
    const literal: KeyName = { display: 'a\\xffb', raw: b64('a\\xffb') };
    setup({
      keys_scan: () => page([summary(binary), summary(literal)]),
      key_value: () => ({ value: { String: { value: { t: 'Text', v: 'x' } } }, ttl_ms: null, memory_bytes: null }),
      key_edit_preview: () => preview({ statements: ['DEL "a\\\\xffb"'] }),
      data_apply: () => committed(),
    });
    await waitFor(() => expect(keyRows()).toHaveLength(2));
    fireEvent.click(keyRows()[1]!);
    await waitFor(() => expect(callsOf('key_value')).toHaveLength(1));
    expect(callsOf('key_value')[0]!.key).toEqual(literal);
    expect(keyRows()[1]).toHaveAttribute('aria-selected', 'true');
    expect(keyRows()[0]).toHaveAttribute('aria-selected', 'false');

    fireEvent.click(within(valuePane()).getByRole('button', { name: 'Delete key' }));
    await waitFor(() => expect(callsOf('key_edit_preview')).toHaveLength(1));
    expect(callsOf('key_edit_preview')[0]!.edit).toEqual({ op: 'delete', key: literal });
    const dialog = await screen.findByTestId('edit-preview');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Confirm' }));
    // The list updates in place: the other key with the same display stays.
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    const state = Object.values(useKeysTabs.getState().byTab)[0]!;
    expect(state.keys.map((k) => k.name)).toEqual([binary]);
    expect(state.selected).toBeNull();
    expect(callsOf('keys_scan')).toHaveLength(1);
  });

  it('vanished_key_shows_notice', async () => {
    setup({
      keys_scan: () => page([summary('gone'), summary('kept')]),
      key_value: () => {
        throw fail('key_not_found');
      },
    });
    await waitFor(() => expect(keyRows()).toHaveLength(2));
    fireEvent.click(keyRows()[0]!);
    await waitFor(() => expect(within(valuePane()).getByRole('status')).toHaveTextContent(KEY_GONE));
    expect(KEY_GONE).toBe('Key no longer exists');
    expect(keyRows()).toHaveLength(1);
    expect(keyRows()[0]).toHaveTextContent('kept');
    expect(Object.values(useKeysTabs.getState().byTab)[0]!.selected).toBeNull();
  });

  it('read_only_hides_edit_controls', async () => {
    setup(
      {
        keys_scan: () => page([summary('user:1', 'Hash', 5000)]),
        key_value: () => hashOf([['name', 'Ada']], 5000),
      },
      { read_only: true },
    );
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    await waitFor(() => expect(within(pane).getByText('Ada')).toBeInTheDocument());
    for (const label of ['New key…', 'Set TTL…', 'Persist', 'Rename…', 'Delete key', 'Edit name', 'Delete name', 'Add field']) {
      expect(screen.queryByRole('button', { name: label })).toBeNull();
    }
    expect(within(pane).getByRole('button', { name: 'Refresh' })).toBeInTheDocument();
  });

  it('hex_mode_rejects_bad_hex', async () => {
    setup({
      keys_scan: () => page([summary('greeting')]),
      key_value: () => ({ value: { String: { value: { t: 'Text', v: 'hi' } } }, ttl_ms: null, memory_bytes: null }),
      key_edit_preview: () => preview({ statements: ['SET greeting ho'] }),
    });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    const text = await within(pane).findByRole('textbox', { name: 'String value' });
    expect(text).toHaveValue('hi');
    fireEvent.click(within(pane).getByRole('button', { name: 'Hex' }));
    expect(within(pane).getByRole('button', { name: 'Hex' })).toHaveAttribute('aria-pressed', 'true');
    expect(text).toHaveValue('68 69');
    const save = within(pane).getByRole('button', { name: 'Save' });
    expect(save).toBeEnabled();

    fireEvent.change(text, { target: { value: '68 6' } });
    expect(within(pane).getByRole('alert')).toHaveTextContent('Invalid hex');
    expect(save).toBeDisabled();
    fireEvent.change(text, { target: { value: 'zz' } });
    expect(save).toBeDisabled();

    fireEvent.change(text, { target: { value: '68 6f' } });
    expect(within(pane).queryByText('Invalid hex')).toBeNull();
    fireEvent.click(save);
    await waitFor(() => expect(callsOf('key_edit_preview')).toHaveLength(1));
    expect(callsOf('key_edit_preview')[0]!.edit).toEqual({ op: 'set_string', key: name('greeting'), value: b64('ho') });
  });

  it('a non-UTF-8 string opens in Hex with its real bytes and saves them unchanged', async () => {
    const raw = [0xff, 0x00, 0xfe];
    setup({
      keys_scan: () => page([summary('bin')]),
      key_value: () => ({ value: { String: { value: { t: 'Bytes', v: { len: 3, preview: raw } } } }, ttl_ms: null, memory_bytes: null }),
      key_edit_preview: () => preview({ statements: ['SET bin …'] }),
    });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    const text = await within(pane).findByRole('textbox', { name: 'String value' });
    expect(within(pane).getByRole('button', { name: 'Hex' })).toHaveAttribute('aria-pressed', 'true');
    expect(text).toHaveValue('ff 00 fe');
    // Through Text and back, unedited: still the real bytes, never the replacement characters.
    fireEvent.click(within(pane).getByRole('button', { name: 'Text' }));
    fireEvent.click(within(pane).getByRole('button', { name: 'Hex' }));
    expect(text).toHaveValue('ff 00 fe');
    fireEvent.click(within(pane).getByRole('button', { name: 'Text' }));
    fireEvent.click(within(pane).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(callsOf('key_edit_preview')).toHaveLength(1));
    expect(callsOf('key_edit_preview')[0]!.edit).toEqual({ op: 'set_string', key: name('bin'), value: bytesToBase64(Uint8Array.from(raw)) });
  });

  it('a string known only by its preview is read-only, without Save', async () => {
    const preview64 = Array.from({ length: 64 }, () => 0x61);
    setup({
      keys_scan: () => page([summary('big')]),
      key_value: () => ({ value: { String: { value: { t: 'Bytes', v: { len: 5000, preview: preview64 } } } }, ttl_ms: null, memory_bytes: null }),
    });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    const text = await within(pane).findByRole('textbox', { name: 'String value' });
    expect(text).toHaveAttribute('readonly');
    expect(within(pane).queryByRole('button', { name: 'Save' })).toBeNull();
    // The key itself can still be deleted or renamed.
    expect(within(pane).getByRole('button', { name: 'Delete key' })).toBeInTheDocument();
  });

  it('hash and list values known only by their preview offer no edit', async () => {
    const cut = { t: 'Bytes', v: { len: 500, preview: Array.from({ length: 64 }, () => 0x62) } } as const;
    let which: 'hash' | 'list' = 'hash';
    setup({
      keys_scan: () => page([summary('h', 'Hash'), summary('l', 'List')]),
      key_value: () =>
        which === 'hash'
          ? { value: { Hash: { fields: [[{ t: 'Text', v: 'big' }, cut], [{ t: 'Text', v: 'small' }, { t: 'Text', v: 'x' }]], total: 2, next: null } }, ttl_ms: null, memory_bytes: null }
          : { value: { List: { items: [cut, { t: 'Text', v: 'y' }], total: 2 } }, ttl_ms: null, memory_bytes: null },
    });
    await waitFor(() => expect(keyRows()).toHaveLength(2));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    await within(pane).findByRole('button', { name: 'Edit small' });
    expect(within(pane).queryByRole('button', { name: 'Edit big' })).toBeNull();
    // The field's own bytes are complete: it can still be deleted.
    expect(within(pane).getByRole('button', { name: 'Delete big' })).toBeInTheDocument();

    which = 'list';
    fireEvent.click(keyRows()[1]!);
    await within(valuePane()).findByRole('button', { name: 'Edit item 1' });
    expect(within(valuePane()).queryByRole('button', { name: 'Edit item 0' })).toBeNull();
    expect(within(valuePane()).queryByRole('button', { name: 'Remove item 0' })).toBeNull();
  });

  it('hash and list values whose bytes are not UTF-8 offer no Edit but keep Delete / Remove', async () => {
    const binary = { t: 'Bytes', v: { len: 3, preview: [0xff, 0x00, 0xfe] } } as const;
    let which: 'hash' | 'list' = 'hash';
    setup({
      keys_scan: () => page([summary('h', 'Hash'), summary('l', 'List')]),
      key_value: () =>
        which === 'hash'
          ? { value: { Hash: { fields: [[{ t: 'Text', v: 'bin' }, binary], [{ t: 'Text', v: 'small' }, { t: 'Text', v: 'x' }]], total: 2, next: null } }, ttl_ms: null, memory_bytes: null }
          : { value: { List: { items: [binary, { t: 'Text', v: 'y' }], total: 2 } }, ttl_ms: null, memory_bytes: null },
    });
    await waitFor(() => expect(keyRows()).toHaveLength(2));
    fireEvent.click(keyRows()[0]!);
    const pane = valuePane();
    await within(pane).findByRole('button', { name: 'Edit small' });
    expect(within(pane).queryByRole('button', { name: 'Edit bin' })).toBeNull();
    expect(within(pane).getByRole('button', { name: 'Delete bin' })).toBeInTheDocument();

    which = 'list';
    fireEvent.click(keyRows()[1]!);
    await within(valuePane()).findByRole('button', { name: 'Edit item 1' });
    expect(within(valuePane()).queryByRole('button', { name: 'Edit item 0' })).toBeNull();
    expect(within(valuePane()).getByRole('button', { name: 'Remove item 0' })).toBeInTheDocument();
  });

  it('Refresh of an open key that vanished shows the notice and leaves the list usable', async () => {
    let gone = false;
    setup({
      keys_scan: () => page([summary('a'), summary('b')]),
      key_value: (args) => {
        if (gone && (args.key as KeyName).display === 'a') throw fail('key_not_found');
        return { value: { String: { value: { t: 'Text', v: 'val' } } }, ttl_ms: null, memory_bytes: null };
      },
    });
    await waitFor(() => expect(keyRows()).toHaveLength(2));
    expect(keyRows()[0]).toHaveAttribute('tabindex', '0');
    fireEvent.click(keyRows()[0]!);
    await within(valuePane()).findByRole('textbox', { name: 'String value' });
    gone = true;
    fireEvent.click(within(valuePane()).getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(within(valuePane()).getByRole('status')).toHaveTextContent('Key no longer exists'));
    expect(keyRows()).toHaveLength(1);
    expect(keyRows()[0]).toHaveAttribute('tabindex', '0');
    fireEvent.click(keyRows()[0]!);
    await within(valuePane()).findByRole('textbox', { name: 'String value' });
    expect(within(valuePane()).queryByRole('status')).toBeNull();
  });

  it('a Block verdict shows its reason and offers no Confirm', async () => {
    setup({
      keys_scan: () => page([summary('greeting')]),
      key_value: () => ({ value: { String: { value: { t: 'Text', v: 'hi' } } }, ttl_ms: null, memory_bytes: null }),
      key_edit_preview: () => preview({ verdict: 'block', reason: 'the connection is read-only' }),
    });
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    fireEvent.click(keyRows()[0]!);
    fireEvent.click(await within(valuePane()).findByRole('button', { name: 'Delete key' }));
    const dialog = await screen.findByTestId('edit-preview');
    expect(within(dialog).getByRole('alert')).toHaveTextContent('the connection is read-only');
    expect(within(dialog).queryByRole('button', { name: 'Confirm' })).toBeNull();
  });

  it('New key… creates the key with its first value and selects it', async () => {
    setup({
      keys_scan: () => page([]),
      key_edit_preview: () => preview({ statements: ['SADD colors red'] }),
      data_apply: () => committed(),
      key_value: () => ({ value: { Set: { members: [{ t: 'Text', v: 'red' }], total: 1, next: null } }, ttl_ms: null, memory_bytes: null }),
    });
    await waitFor(() => expect(callsOf('keys_scan')).toHaveLength(1));
    fireEvent.click(screen.getByRole('button', { name: 'New key…' }));
    const dialog = screen.getByTestId('new-key');
    fireEvent.change(within(dialog).getByLabelText('Name'), { target: { value: 'colors' } });
    fireEvent.change(within(dialog).getByLabelText('Type'), { target: { value: 'Set' } });
    fireEvent.change(within(dialog).getByLabelText('Member'), { target: { value: 'red' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    await waitFor(() => expect(callsOf('key_edit_preview')).toHaveLength(1));
    expect(callsOf('key_edit_preview')[0]!.edit).toEqual({ op: 'set_add', key: name('colors'), member: b64('red') });
    fireEvent.click(within(await screen.findByTestId('edit-preview')).getByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(keyRows()).toHaveLength(1));
    expect(keyRows()[0]).toHaveTextContent('colors');
    expect(keyRows()[0]).toHaveAttribute('aria-selected', 'true');
    await waitFor(() => expect(within(valuePane()).getByText('red')).toBeInTheDocument());
  });
});
