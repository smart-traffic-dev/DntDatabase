// Fuzzy matching for the command palette and quick open: a pure scorer, no UI.

export interface Match {
  score: number;
  /** Positions in the text of the matched characters (for highlighting), ascending. */
  indices: number[];
}

const SEPARATORS = new Set([' ', '-', '_', '/', '.', ':', '›', '>', '(', '\\']);

/** Extra points for a match at `j`: the text start, a word start, a camelCase hump. */
function bonus(text: string, j: number): number {
  if (j === 0) return 10;
  const prev = text[j - 1] ?? '';
  if (SEPARATORS.has(prev)) return 8;
  const cur = text[j] ?? '';
  if (prev === prev.toLowerCase() && prev !== prev.toUpperCase() && cur !== cur.toLowerCase()) return 6;
  return 0;
}

const CONSECUTIVE = 6;
const GAP = 3;

/**
 * Matches `query` as a subsequence of `text` (case-insensitive, spaces in the query ignored).
 * The best alignment wins: consecutive characters and word starts score higher, gaps cost a
 * little, shorter texts win ties. An empty query matches everything with score 0. Null: no match.
 */
export function fuzzyMatch(query: string, text: string): Match | null {
  const q = query.replace(/\s+/g, '').toLowerCase();
  if (!q) return { score: 0, indices: [] };
  const t = text.toLowerCase();
  const n = t.length;
  const m = q.length;
  if (m > n) return null;

  // dp[i][j]: best score with q[i] matched at t[j]; from[i][j]: where q[i - 1] was matched.
  const NONE = Number.NEGATIVE_INFINITY;
  const dp: number[][] = Array.from({ length: m }, () => new Array<number>(n).fill(NONE));
  const from: number[][] = Array.from({ length: m }, () => new Array<number>(n).fill(-1));

  for (let i = 0; i < m; i++) {
    // Best dp[i - 1][k] over k <= j - 2 (a non-adjacent predecessor), with its position.
    let bestScore = NONE;
    let bestAt = -1;
    for (let j = 0; j < n; j++) {
      if (i > 0 && j >= 2) {
        const candidate = dp[i - 1]![j - 2]!;
        if (candidate > bestScore) {
          bestScore = candidate;
          bestAt = j - 2;
        }
      }
      if (t[j] !== q[i]) continue;
      const here = 1 + bonus(text, j);
      if (i === 0) {
        dp[0]![j] = here - Math.min(j, 6) * 0.5;
        continue;
      }
      let best = NONE;
      let at = -1;
      const adjacent = j >= 1 ? dp[i - 1]![j - 1]! : NONE;
      if (adjacent > NONE) {
        best = adjacent + CONSECUTIVE;
        at = j - 1;
      }
      if (bestScore > NONE && bestScore - GAP > best) {
        best = bestScore - GAP;
        at = bestAt;
      }
      if (at >= 0) {
        dp[i]![j] = best + here;
        from[i]![j] = at;
      }
    }
  }

  let end = -1;
  let score = NONE;
  for (let j = 0; j < n; j++) {
    const s = dp[m - 1]![j]!;
    if (s > score) {
      score = s;
      end = j;
    }
  }
  if (end < 0) return null;
  const indices: number[] = [];
  for (let i = m - 1, j = end; i >= 0; i--) {
    indices.push(j);
    j = from[i]![j]!;
  }
  indices.reverse();
  return { score: score - n * 0.05, indices };
}

export interface Ranked<T> {
  item: T;
  score: number;
  /** Highlight positions in the first text (empty when the match came from a later one). */
  indices: number[];
}

/**
 * Ranks `items` for `query`: each item offers one or more texts, the first being the one
 * that is shown (a later text, e.g. a path, still matches but scores lower). Best first, ties in
 * the original order. An empty query keeps every item in order.
 */
export function rankItems<T>(items: readonly T[], query: string, texts: (item: T) => readonly string[]): Ranked<T>[] {
  if (!query.trim()) return items.map((item) => ({ item, score: 0, indices: [] }));
  const out: (Ranked<T> & { order: number })[] = [];
  items.forEach((item, order) => {
    const list = texts(item);
    let best: Ranked<T> | null = null;
    list.forEach((text, k) => {
      const match = fuzzyMatch(query, text);
      if (!match) return;
      // A match in a secondary text counts less than one in the label.
      const score = match.score - k * 15;
      if (!best || score > best.score) best = { item, score, indices: k === 0 ? match.indices : [] };
    });
    if (best) out.push({ ...(best as Ranked<T>), order });
  });
  out.sort((a, b) => b.score - a.score || a.order - b.order);
  return out.map(({ item, score, indices }) => ({ item, score, indices }));
}
