import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { CellInput, ColumnMeta, ObjectRef, Value } from '../../bindings';
import { callsOf, ids, mockBackend } from '../../test/backend';
import { ValueEditor } from './ValueEditor';
import { bytesToBase64, detectImage, formatJson, parseHex } from './valueModel';

const object: ObjectRef = { database: 'shop', schema: null, name: 'docs', kind: 'Table' };
const column = (name: string, logical: ColumnMeta['logical'], db_type = 'text'): ColumnMeta => ({
  name,
  db_type,
  logical,
  nullable: true,
  origin_table: null,
  origin_column: null,
});

const PNG = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
const KEY: [string, Value][] = [['id', { t: 'Int', v: 7 }]];

function setup(over: Partial<React.ComponentProps<typeof ValueEditor>> = {}) {
  const onStage = vi.fn<(input: CellInput, token: string | null) => void>();
  const onClose = vi.fn();
  const view = render(
    <ValueEditor
      connectionId={ids.prod}
      object={object}
      column={column('doc', 'Json', 'json')}
      value={{ t: 'Json', v: '{"a":1}' }}
      truncated={false}
      fetchKey={KEY}
      editable
      onStage={onStage}
      onClose={onClose}
      {...over}
    />,
  );
  return { onStage, onClose, user: userEvent.setup(), ...view };
}

describe('value editor', () => {
  beforeEach(() => {
    mockBackend({});
  });

  it('value_editor_json_validation', async () => {
    const { user, onStage } = setup();
    await user.click(screen.getByRole('tab', { name: 'JSON' }));
    const box = screen.getByRole('textbox', { name: 'Value' });
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    await user.clear(box);
    await user.click(box);
    await user.paste('{"a": ');
    expect(await screen.findByRole('alert')).toHaveTextContent(/Invalid JSON/);
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    await user.clear(box);
    await user.click(box);
    await user.paste('{"a":[1,2],"b":{}}');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Format' }));
    expect(box).toHaveValue('{\n  "a": [\n    1,\n    2\n  ],\n  "b": {}\n}');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(onStage).toHaveBeenCalledWith({ kind: 'text', text: '{\n  "a": [\n    1,\n    2\n  ],\n  "b": {}\n}' }, null);
  });

  it('formats without touching numbers or strings', () => {
    expect(formatJson('{"n":12345678901234567890,"s":"a, b: {c}"}')).toEqual({
      ok: true,
      text: '{\n  "n": 12345678901234567890,\n  "s": "a, b: {c}"\n}',
    });
    expect(formatJson('[1,')).toEqual({ ok: false, message: expect.any(String) });
  });

  it('value_editor_loads_full_value_before_edit', async () => {
    let release: (v: unknown) => void = () => undefined;
    mockBackend({
      data_cell_fetch: () => new Promise((r) => (release = r)),
    });
    const cut = 'x'.repeat(100);
    const { user, onStage } = setup({ column: column('body', 'Text'), value: { t: 'Text', v: cut }, truncated: true });
    // Until the complete value is here nothing can be edited or saved.
    expect(screen.getByText('Loading the full value…')).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Value' })).toHaveAttribute('readonly');
    expect(callsOf('data_cell_fetch')).toEqual([{ id: ids.prod, obj: object, key: KEY, column: 'body' }]);
    release({ token: 'tok-1', payload: { kind: 'text', text: `${cut}-full` } });
    await waitFor(() => expect(screen.getByRole('textbox', { name: 'Value' })).toHaveValue(`${cut}-full`));
    const box = screen.getByRole('textbox', { name: 'Value' });
    expect(box).not.toHaveAttribute('readonly');
    expect(screen.getByText('105 characters')).toBeInTheDocument();
    await user.type(box, '!');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    // The token of the complete original travels with the change (the conflict check uses it).
    expect(onStage).toHaveBeenCalledWith({ kind: 'text', text: `${cut}-full!` }, 'tok-1');
  });

  it('a value that cannot be fetched stays read-only and says why', async () => {
    mockBackend({ data_cell_fetch: () => Promise.reject({ kind: 'config', message: 'The row is gone.' }) });
    setup({ column: column('body', 'Text'), value: { t: 'Text', v: 'cut' }, truncated: true });
    expect(await screen.findByRole('alert')).toHaveTextContent('The row is gone.');
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
  });

  it('image_preview_by_magic_bytes', async () => {
    expect(detectImage(PNG)).toBe('png');
    expect(detectImage(Uint8Array.from([0xff, 0xd8, 0xff, 0xe0]))).toBe('jpeg');
    expect(detectImage(new TextEncoder().encode('GIF89a..'))).toBe('gif');
    expect(detectImage(new TextEncoder().encode('RIFF\0\0\0\0WEBPVP8 '))).toBe('webp');
    expect(detectImage(new TextEncoder().encode('hello world'))).toBeNull();
    expect(detectImage(new Uint8Array())).toBeNull();

    mockBackend({
      data_cell_fetch: () => ({ token: 't', payload: { kind: 'base64', data: bytesToBase64(PNG), len: PNG.length } }),
    });
    const { user } = setup({
      column: column('pic', 'Binary', 'blob'),
      value: { t: 'Bytes', v: { len: PNG.length, preview: [...PNG] } },
      truncated: true,
    });
    const img = await screen.findByRole('img', { name: 'Image preview' });
    expect(img).toHaveAttribute('src', `data:image/png;base64,${bytesToBase64(PNG)}`);
    await user.click(screen.getByRole('tab', { name: 'Hex' }));
    expect(screen.getByRole('textbox', { name: 'Hex' })).toHaveValue('89 50 4e 47 0d 0a 1a 0a 00 00 00 00');
  });

  it('hex edits are staged as base64', async () => {
    const bytes = [1, 2, 3];
    const { user, onStage } = setup({
      column: column('blob', 'Binary', 'blob'),
      value: { t: 'Bytes', v: { len: 3, preview: bytes } },
    });
    const hex = screen.getByRole('textbox', { name: 'Hex' });
    await user.clear(hex);
    await user.type(hex, 'zz');
    expect(await screen.findByRole('alert')).toHaveTextContent(/hex/i);
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    await user.clear(hex);
    await user.type(hex, 'de ad be ef');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(onStage).toHaveBeenCalledWith({ kind: 'base64', data: bytesToBase64(Uint8Array.from([0xde, 0xad, 0xbe, 0xef])) }, null);
    expect(parseHex('0xDE AD\nbe')).toEqual(Uint8Array.from([0xde, 0xad, 0xbe]));
    expect(parseHex('abc')).toBeNull();
  });

  it('Set NULL stages NULL', async () => {
    const { user, onStage } = setup({ column: column('body', 'Text'), value: { t: 'Text', v: 'x' } });
    await user.click(screen.getByRole('button', { name: 'Set NULL' }));
    expect(onStage).toHaveBeenCalledWith({ kind: 'null' }, null);
  });

  it('Load from file stages the loaded value by its token', async () => {
    mockBackend({
      file_pick_open: () => 'C:\\in.txt',
      data_cell_load_file: () => ({ token: 'file-tok', payload: { kind: 'text', text: 'from file' } }),
    });
    const { user, onStage } = setup({ column: column('body', 'Text'), value: { t: 'Text', v: 'x' } });
    await user.click(screen.getByRole('button', { name: 'Load from file…' }));
    await waitFor(() => expect(screen.getByRole('textbox', { name: 'Value' })).toHaveValue('from file'));
    expect(callsOf('file_pick_open')).toEqual([{ kind: 'cell_value' }]);
    expect(callsOf('data_cell_load_file')).toEqual([{ id: ids.prod, obj: object, column: 'body', path: 'C:\\in.txt' }]);
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(onStage).toHaveBeenCalledWith({ kind: 'loaded', token: 'file-tok' }, null);
  });

  it('Save to file writes the value the server holds', async () => {
    mockBackend({
      data_cell_fetch: () => ({ token: 'tok-2', payload: { kind: 'text', text: 'abc' } }),
      file_pick_save: () => 'C:\\out.txt',
      data_cell_save_file: () => null,
    });
    const { user } = setup({ column: column('body', 'Text'), value: { t: 'Text', v: 'abc' } });
    await user.click(screen.getByRole('button', { name: 'Save to file…' }));
    await waitFor(() => expect(callsOf('data_cell_save_file')).toEqual([{ token: 'tok-2', path: 'C:\\out.txt' }]));
    expect(callsOf('file_pick_save')[0]).toMatchObject({ kind: 'cell_value' });
  });

  it('a read-only value cannot be changed', () => {
    setup({ editable: false });
    expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Set NULL' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Load from file…' })).not.toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Value' })).toHaveAttribute('readonly');
  });
});
