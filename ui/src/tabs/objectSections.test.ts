import { describe, expect, it } from 'vitest';
import { subTabsOf } from './objectSections';

describe('subTabsOf', () => {
  it('a collection opens on its Data, then its DDL', () => {
    expect(subTabsOf('Collection')).toEqual(['data', 'ddl']);
  });

  it('tables and views keep all four sections; other objects DDL only', () => {
    expect(subTabsOf('Table')).toEqual(['data', 'info', 'ddl', 'structure']);
    expect(subTabsOf('View')).toEqual(['data', 'info', 'ddl', 'structure']);
    expect(subTabsOf('Function')).toEqual(['ddl']);
  });
});
