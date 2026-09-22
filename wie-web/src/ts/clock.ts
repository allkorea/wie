export function epochMillis(): number {
  return Date.now();
}

export function monotonicMillis(): number {
  return performance.now();
}
