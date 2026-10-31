// Keys and tokens. Kept free of Workers bindings so Node can test it.
//
//   ENROLL_KEY   Worker secret: lets a machine join (`home-runner fleet join --key`).
//   VIEW_KEY     Worker secret: lets the dashboard read the fleet.
//   ADMIN_TOKEN  Worker secret: removes machines.
//   hrf_...      per-machine token handed out on join; only its SHA-256 is stored.

export const MACHINE_TOKEN_PREFIX = "hrf_";

export async function sha256(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Compares two secrets by their hashes, so lengths match and timing says nothing about either. */
export async function safeEqual(a: string, b: string): Promise<boolean> {
  const [ha, hb] = await Promise.all([sha256(a), sha256(b)]);
  let diff = ha.length ^ hb.length;
  for (let i = 0; i < ha.length && i < hb.length; i++) diff |= ha.charCodeAt(i) ^ hb.charCodeAt(i);
  return diff === 0;
}

export function bearer(headers: Headers): string | null {
  const h = headers.get("authorization") ?? "";
  return h.startsWith("Bearer ") ? h.slice(7).trim() || null : null;
}

export const isMachineToken = (token: string): boolean => token.startsWith(MACHINE_TOKEN_PREFIX);

export function newMachineToken(bytes: Uint8Array): string {
  const b64 = btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  return `${MACHINE_TOKEN_PREFIX}${b64}`;
}
