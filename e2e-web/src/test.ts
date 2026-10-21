import { expect, test as base } from '@playwright/test';
import { backendProblems } from './shim';

/**
 * Playwright's `test`, plus a check after every test: the UI must not have called a command
 * the fake backend does not know (nor made a handler throw). Such a call would otherwise only
 * show up as an "internal error" toast and could hide a broken flow.
 */
export const test = base.extend<{ backendCheck: void }>({
  backendCheck: [
    async ({ page }, use) => {
      await use();
      expect(backendProblems(page), 'fake backend problems').toEqual([]);
    },
    { auto: true },
  ],
});

export { expect };
