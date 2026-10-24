// Host-side avatar fetch (SPEC 24.4.5). The webview never loads an avatar itself. https only, no
// cookies and no token, public addresses only, every redirect re-checked, at most 3 redirects,
// png/jpeg/webp/gif (never SVG), 256 KB, 5 s, LRU of 200 entries and 2 MiB; the result is a data: URI.
// Known limit: the name is resolved and checked, then fetch resolves it again (a rebinding window);
// the response is still only an image of at most 256 KB, and nothing is sent but a GET without credentials.
import { lookup } from "node:dns/promises";
import { isIP } from "node:net";

export const AVATAR_MAX_BYTES = 256 * 1024;
export const AVATAR_MAX_REDIRECTS = 3;
export const AVATAR_TIMEOUT_MS = 5000;
export const AVATAR_CACHE_ENTRIES = 200;
export const AVATAR_CACHE_BYTES = 2 * 1024 * 1024;
const TYPES = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);

/** True only for globally routable addresses. Anything unparseable is not public. */
export function isPublicIp(ip: string): boolean {
  const kind = isIP(ip);
  if (kind === 4) {
    const p = ip.split(".").map(Number);
    const [a, b, c] = p as [number, number, number, number];
    if (a === 0 || a === 10 || a === 127 || a >= 224) return false;
    if (a === 100 && b >= 64 && b <= 127) return false;
    if (a === 169 && b === 254) return false;
    if (a === 172 && b >= 16 && b <= 31) return false;
    if (a === 192 && b === 168) return false;
    if (a === 192 && b === 0 && (c === 0 || c === 2)) return false;
    if (a === 198 && (b === 18 || b === 19)) return false;
    if (a === 198 && b === 51 && c === 100) return false;
    if (a === 203 && b === 0 && c === 113) return false;
    return true;
  }
  if (kind === 6) {
    const s = (ip.toLowerCase().split("%")[0] ?? "").replace(/^\[|\]$/g, "");
    const mapped = /^(?:0*:)*ffff:(\d+\.\d+\.\d+\.\d+)$/.exec(s);
    if (mapped?.[1]) return isPublicIp(mapped[1]);
    const mappedHex = /^(?:0*:)*ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/.exec(s);
    if (mappedHex) {
      const hi = Number.parseInt(mappedHex[1] ?? "0", 16);
      const lo = Number.parseInt(mappedHex[2] ?? "0", 16);
      return isPublicIp(`${hi >> 8}.${hi & 255}.${lo >> 8}.${lo & 255}`);
    }
    if (s === "::" || s === "::1") return false;
    if (s.startsWith("::")) return false; // IPv4-compatible and other reserved forms
    const first = Number.parseInt(s.split(":")[0] || "0", 16);
    if ((first & 0xfe00) === 0xfc00) return false; // fc00::/7
    if ((first & 0xffc0) === 0xfe80) return false; // fe80::/10
    if ((first & 0xff00) === 0xff00) return false; // multicast
    if (s.startsWith("2001:db8") || s.startsWith("64:ff9b") || first === 0x100) return false;
    return true;
  }
  return false;
}

export interface AvatarDeps {
  /** All addresses a host name resolves to. */
  resolve?: (host: string) => Promise<string[]>;
  fetch?: typeof fetch;
}

export class AvatarFetcher {
  private readonly cache = new Map<string, string>();
  private bytes = 0;
  private readonly resolve: (host: string) => Promise<string[]>;
  private readonly doFetch: typeof fetch;

  constructor(deps: AvatarDeps = {}) {
    this.resolve =
      deps.resolve ??
      (async (h) => (await lookup(h, { all: true, verbatim: true })).map((a) => a.address));
    this.doFetch = deps.fetch ?? fetch;
  }

  get size(): number {
    return this.cache.size;
  }
  get cachedBytes(): number {
    return this.bytes;
  }

  /** Returns a data: URI, or null when the URL is refused or the fetch fails. Never throws. */
  async get(raw: string): Promise<string | null> {
    const hit = this.cache.get(raw);
    if (hit !== undefined) {
      this.cache.delete(raw);
      this.cache.set(raw, hit);
      return hit;
    }
    try {
      const uri = await this.load(raw);
      if (uri) this.put(raw, uri);
      return uri;
    } catch {
      return null;
    }
  }

  private put(key: string, uri: string): void {
    if (uri.length > AVATAR_CACHE_BYTES) return;
    this.cache.set(key, uri);
    this.bytes += uri.length;
    while (this.cache.size > AVATAR_CACHE_ENTRIES || this.bytes > AVATAR_CACHE_BYTES) {
      const oldest = this.cache.keys().next();
      if (oldest.done) break;
      this.bytes -= this.cache.get(oldest.value)?.length ?? 0;
      this.cache.delete(oldest.value);
    }
  }

  private async allowed(u: URL): Promise<boolean> {
    if (u.protocol !== "https:" || u.username !== "" || u.password !== "") return false;
    const host = u.hostname.replace(/^\[|\]$/g, "");
    const addrs = isIP(host) ? [host] : await this.resolve(host);
    return addrs.length > 0 && addrs.every(isPublicIp);
  }

  private async load(raw: string): Promise<string | null> {
    const signal = AbortSignal.timeout(AVATAR_TIMEOUT_MS);
    let url = new URL(raw);
    for (let hop = 0; hop <= AVATAR_MAX_REDIRECTS; hop++) {
      if (!(await this.allowed(url))) return null;
      const res = await this.doFetch(url, {
        redirect: "manual",
        credentials: "omit",
        headers: { accept: "image/png,image/jpeg,image/webp,image/gif" },
        signal,
      });
      if (res.status >= 300 && res.status < 400) {
        const loc = res.headers.get("location");
        if (!loc) return null;
        url = new URL(loc, url);
        continue;
      }
      if (!res.ok) return null;
      const type = (res.headers.get("content-type") ?? "").split(";")[0]?.trim().toLowerCase();
      if (!type || !TYPES.has(type)) return null;
      if (Number(res.headers.get("content-length") ?? "0") > AVATAR_MAX_BYTES) return null;
      const body = await readCapped(res, AVATAR_MAX_BYTES);
      if (!body) return null;
      return `data:${type};base64,${Buffer.from(body).toString("base64")}`;
    }
    return null;
  }
}

async function readCapped(res: Response, cap: number): Promise<Uint8Array | null> {
  if (!res.body) return null;
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > cap) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.byteLength;
  }
  return out;
}
