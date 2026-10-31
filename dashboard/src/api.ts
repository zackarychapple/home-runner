import type { FleetView } from "./types.ts";

export class Unauthorized extends Error {}

/** The fleet Worker, set at build time with VITE_FLEET_URL. */
export const FLEET_URL: string = (import.meta.env.VITE_FLEET_URL ?? "").replace(/\/+$/, "");

export async function fetchFleet(key: string, signal?: AbortSignal): Promise<FleetView> {
  if (!FLEET_URL) throw new Error("This dashboard was built without VITE_FLEET_URL.");
  const resp = await fetch(`${FLEET_URL}/v1/fleet`, { headers: { authorization: `Bearer ${key}` }, signal });
  if (resp.status === 401) throw new Unauthorized();
  if (!resp.ok) throw new Error(`The fleet returned ${resp.status}.`);
  return (await resp.json()) as FleetView;
}
