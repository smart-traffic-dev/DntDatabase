import { beforeEach, expect, it } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { resetStores } from '../test/backend';
import type { MenuAction } from './ActionMenu';
import { emptyAreaItems } from './ConnectionTree';

beforeEach(() => resetStores());

const labels = () => emptyAreaItems().map((e) => ('separator' in e ? '-' : e.label));
const run = (label: string) => (emptyAreaItems().find((e) => 'label' in e && e.label === label) as MenuAction).run();

it('the empty tree area offers New connection, a top-level New group, then import / export', () => {
  expect(labels()).toEqual(['New connection…', 'New group', '-', 'Import connections…', 'Export connections…']);
});

it('New group asks for a group at the top level; New connection opens its dialog', () => {
  run('New group');
  expect(useDialogs.getState().request).toEqual({ kind: 'newGroup', parentId: null });
  run('New connection…');
  expect(useDialogs.getState().request).toEqual({ kind: 'newConnection' });
});
