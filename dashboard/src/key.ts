// The view key: kept in this browser only. A link like `https://…/#k=<key>` signs a browser in;
// the fragment never reaches a server, and it is removed from the address bar once read.

const STORAGE = "hr-fleet-key";

export const parseKeyFromHash = (hash: string): string | null =>
  new URLSearchParams(hash.replace(/^#/, "")).get("k")?.trim() || null;

export function loadKey(): string | null {
  const fromLink = parseKeyFromHash(location.hash);
  if (fromLink) {
    localStorage.setItem(STORAGE, fromLink);
    history.replaceState(null, "", location.pathname + location.search);
    return fromLink;
  }
  return localStorage.getItem(STORAGE);
}

export const saveKey = (key: string) => localStorage.setItem(STORAGE, key.trim());
export const forgetKey = () => localStorage.removeItem(STORAGE);
