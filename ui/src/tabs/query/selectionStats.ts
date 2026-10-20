// Statistics of the selected cells for the grid footer. Big integers and decimals arrive as
// strings: integers are summed exactly with BigInt, anything else falls back to Number (shown "≈").
import type { Value } from '../../bindings';

export interface SelectionStats {
  /** Selected cells that are not NULL. */
  count: number;
  /** Present when at least one selected cell is a number. */
  numeric: { sum: string; avg: string; min: string; max: string; approx: boolean } | null;
}

const INTEGER = /^[+-]?\d+$/;

/** The number a value holds, as an exact integer when it is one; null for non-numbers. */
function numberOf(v: Value): { big: bigint } | { num: number } | null {
  switch (v.t) {
    case 'Int':
    case 'UInt': {
      const s = String(v.v);
      return INTEGER.test(s) ? { big: BigInt(s) } : null;
    }
    case 'Decimal': {
      const s = v.v.trim();
      if (INTEGER.test(s)) return { big: BigInt(s) };
      const n = Number(s);
      return Number.isFinite(n) ? { num: n } : null;
    }
    case 'Float':
      return Number.isFinite(v.v) ? (Number.isInteger(v.v) && Math.abs(v.v) < 2 ** 53 ? { big: BigInt(v.v) } : { num: v.v }) : null;
    default:
      return null;
  }
}

const group = (n: bigint) => n.toLocaleString('en-US');
const fmtNum = (n: number) => n.toLocaleString('en-US', { maximumFractionDigits: 4 });

/** `sum / count` rounded to 4 decimals, exact on BigInt. */
function bigAverage(sum: bigint, count: bigint): string {
  const neg = sum < 0n;
  const abs = neg ? -sum : sum;
  const scaled = (abs * 10_000n + count / 2n) / count;
  const whole = scaled / 10_000n;
  const frac = (scaled % 10_000n).toString().padStart(4, '0').replace(/0+$/, '');
  const body = `${group(whole)}${frac ? `.${frac}` : ''}`;
  return neg && scaled !== 0n ? `-${body}` : body;
}

/** Stats over `values`; null when there is nothing worth showing (fewer than two cells). */
export function selectionStats(values: Iterable<Value>, total: number): SelectionStats | null {
  if (total < 2) return null;
  let count = 0;
  const nums: ({ big: bigint } | { num: number })[] = [];
  for (const v of values) {
    if (v.t === 'Null') continue;
    count++;
    const n = numberOf(v);
    if (n) nums.push(n);
  }
  if (nums.length === 0) return { count, numeric: null };
  if (nums.every((n): n is { big: bigint } => 'big' in n)) {
    const bigs = nums.map((n) => n.big);
    const sum = bigs.reduce((a, b) => a + b, 0n);
    const min = bigs.reduce((a, b) => (b < a ? b : a));
    const max = bigs.reduce((a, b) => (b > a ? b : a));
    return {
      count,
      numeric: { sum: group(sum), avg: bigAverage(sum, BigInt(bigs.length)), min: group(min), max: group(max), approx: false },
    };
  }
  const xs = nums.map((n) => ('big' in n ? Number(n.big) : n.num));
  const sum = xs.reduce((a, b) => a + b, 0);
  return {
    count,
    numeric: {
      sum: `≈${fmtNum(sum)}`,
      avg: `≈${fmtNum(sum / xs.length)}`,
      min: `≈${fmtNum(xs.reduce((a, b) => Math.min(a, b)))}`,
      max: `≈${fmtNum(xs.reduce((a, b) => Math.max(a, b)))}`,
      approx: true,
    },
  };
}

/** The footer text: "Count 12 · Sum 3,450 · Avg 287.5 · Min 1 · Max 999". */
export function statsText(s: SelectionStats): string {
  const parts = [`Count ${s.count.toLocaleString('en-US')}`];
  if (s.numeric) parts.push(`Sum ${s.numeric.sum}`, `Avg ${s.numeric.avg}`, `Min ${s.numeric.min}`, `Max ${s.numeric.max}`);
  return parts.join(' · ');
}
