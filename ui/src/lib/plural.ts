// Count texts shared by the whole UI: "1 row", "2 rows", "10,000 rows".

const grouping = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });

/** `n` with thousands separators ("10,000"). */
export function formatCount(n: number): string {
  return grouping.format(n);
}

/** `singular` for exactly 1, else `pluralForm` (default: `singular` + "s"). */
export function pluralWord(n: number, singular: string, pluralForm = `${singular}s`): string {
  return n === 1 ? singular : pluralForm;
}

/** "1 row", "0 rows", "10,000 rows"; pass `pluralForm` for irregular words ("queries"). */
export function plural(n: number, singular: string, pluralForm?: string): string {
  return `${formatCount(n)} ${pluralWord(n, singular, pluralForm)}`;
}
