// Id helpers (SPEC 24.2): lowercase hex for object ids, and the reverse-hex k-z form of change ids
// (SPEC 2433: hex digit d becomes the letter 'z' - d, so 0 is 'z' and 15 is 'k').

const CHANGE_LEN = 32;

export function toHex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

export function fromHex(text: string): Uint8Array {
  if (text.length % 2 !== 0 || !/^[0-9a-f]*$/i.test(text)) throw new RangeError("not a hex string");
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(text.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** A change id (16 bytes) as its 32 letters k-z. */
export function changeIdToString(bytes: Uint8Array): string {
  if (bytes.length !== CHANGE_LEN / 2) throw new RangeError("a change id is 16 bytes");
  let out = "";
  for (const b of bytes) {
    out += String.fromCharCode(122 - (b >> 4)) + String.fromCharCode(122 - (b & 15));
  }
  return out;
}

export function isChangeId(text: string): boolean {
  return text.length === CHANGE_LEN && /^[k-z]+$/.test(text);
}

export function changeIdFromString(text: string): Uint8Array {
  if (!isChangeId(text)) throw new RangeError("a change id is 32 letters k-z");
  const out = new Uint8Array(CHANGE_LEN / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = ((122 - text.charCodeAt(i * 2)) << 4) | (122 - text.charCodeAt(i * 2 + 1));
  }
  return out;
}

/** The 12-letter short form. */
export function shortChangeId(text: string): string {
  return text.slice(0, 12);
}
