/** "5 min ago", or an ISO date once it is older than 60 days. `seconds` is a Unix time. */
export function ago(seconds: number, now = Date.now()): string {
  const d = Math.max(0, Math.round(now / 1000 - seconds));
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  if (d < 86400 * 60) return `${Math.floor(d / 86400)} d ago`;
  return new Date(seconds * 1000).toISOString().slice(0, 10);
}
