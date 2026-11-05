/** Exhaustiveness guard: a `switch` that misses a case fails to compile here. */
export function assertNever(x: never): never {
  throw new Error(`unexpected value ${String(x)}`);
}
