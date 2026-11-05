import { screen, within } from '@testing-library/react';
import type { UserEvent } from '@testing-library/user-event';

/** The open list of the SelectMenu named `label` (it is rendered in <body>, outside its trigger). */
export const menuList = (label: string) => screen.getByRole('listbox', { name: `${label} options` });

/** Picks the option labelled `option` in a SelectMenu: clicks its trigger, then the option in its list. */
export async function pickMenuOption(user: UserEvent, combobox: HTMLElement, option: string): Promise<void> {
  await user.click(combobox);
  const label = combobox.getAttribute('aria-label') ?? '';
  await user.click(within(menuList(label)).getByRole('option', { name: option }));
}

/** The option labels of a SelectMenu (opens its list, reads it, closes it with Escape). */
export async function menuOptionLabels(user: UserEvent, combobox: HTMLElement): Promise<string[]> {
  await user.click(combobox);
  const label = combobox.getAttribute('aria-label') ?? '';
  const labels = within(menuList(label))
    .getAllByRole('option')
    .map((o) => o.getAttribute('aria-label') ?? '');
  await user.keyboard('{Escape}');
  return labels;
}
