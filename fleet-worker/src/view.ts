// Pure helpers behind the API, kept free of Workers bindings so they can be unit-tested with Node.

/** A machine is offline after missing about three 60s heartbeats, or once it says it stopped. */
export const OFFLINE_AFTER_SECS = 180;
export const MAX_JOBS_PER_REPORT = 500;
export const MAX_RECORD_BYTES = 16 * 1024;
export const SHARE_LEVELS = ["totals", "repos", "full"] as const;
export type Share = (typeof SHARE_LEVELS)[number];

export interface MachineRow {
  id: string;
  host: string;
  house: string;
  share: string;
  version: string;
  os: string;
  arch: string;
  cpus: number;
  online: number;
  last_seen: number;
  pools: string;
}

export interface HouseTotalsRow {
  house: string;
  jobs: number;
  saved: number;
}

export interface DayRow {
  day: string;
  jobs: number;
  saved: number;
}

export interface TotalsRow {
  jobs: number;
  runner_secs: number | null;
  saved_vs_github_usd: number | null;
  saved_vs_depot_usd: number | null;
  own_usd: number | null;
  repos: number;
}

export function monthStart(nowSecs: number): number {
  const d = new Date(nowSecs * 1000);
  return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1) / 1000;
}

export function isOnline(m: Pick<MachineRow, "online" | "last_seen">, nowSecs: number): boolean {
  return m.online === 1 && nowSecs - m.last_seen <= OFFLINE_AFTER_SECS;
}

/** The last `days` UTC days, oldest first, with zeros for days without jobs. */
export function fillDays(rows: DayRow[], nowSecs: number, days = 14) {
  const byDay = new Map(rows.map((r) => [r.day, r]));
  const out = [];
  for (let i = days - 1; i >= 0; i--) {
    const day = new Date((nowSecs - i * 86400) * 1000).toISOString().slice(0, 10);
    const r = byDay.get(day);
    out.push({ day, jobs: r?.jobs ?? 0, saved_vs_github_usd: round(r?.saved ?? 0) });
  }
  return out;
}

export function buildView(
  fleet: string,
  nowSecs: number,
  totals: TotalsRow,
  machines: MachineRow[],
  houseTotals: HouseTotalsRow[],
  days: DayRow[],
) {
  const houses = new Map<string, { house: string; saved_month_usd: number; jobs_month: number; hosts: unknown[] }>();
  const house = (name: string) => {
    let h = houses.get(name);
    if (!h) {
      h = { house: name, saved_month_usd: 0, jobs_month: 0, hosts: [] };
      houses.set(name, h);
    }
    return h;
  };
  for (const t of houseTotals) {
    const h = house(t.house);
    h.saved_month_usd = round(t.saved);
    h.jobs_month = t.jobs;
  }
  for (const m of [...machines].sort((a, b) => a.host.localeCompare(b.host))) {
    const online = isOnline(m, nowSecs);
    let pools: unknown[] = [];
    try {
      pools = JSON.parse(m.pools);
    } catch {
      // A bad row shouldn't take the whole view down.
    }
    house(m.house).hosts.push({
      host: m.host,
      online,
      last_seen: m.last_seen,
      version: m.version,
      os: m.os,
      arch: m.arch,
      cpus: m.cpus,
      share: m.share,
      // Whatever an offline machine last said it was running is no longer true.
      pools: online ? pools : stripLive(pools),
    });
  }
  return {
    fleet,
    now: nowSecs,
    month: {
      jobs: totals.jobs,
      runner_secs: round(totals.runner_secs ?? 0),
      saved_vs_github_usd: round(totals.saved_vs_github_usd ?? 0),
      saved_vs_depot_usd: round(totals.saved_vs_depot_usd ?? 0),
      own_usd: round(totals.own_usd ?? 0),
      repos: totals.repos,
    },
    houses: [...houses.values()].sort((a, b) => b.saved_month_usd - a.saved_month_usd),
    daily: fillDays(days, nowSecs),
  };
}

function stripLive(pools: unknown[]): unknown[] {
  return pools.map((p) => {
    if (!p || typeof p !== "object") return p;
    const pool = p as { slots?: unknown[] };
    return {
      ...pool,
      slots: (pool.slots ?? []).map((s) => ({ ...(s as object), phase: "offline", job: null })),
    };
  });
}

export function round(v: number): number {
  return Math.round(v * 10000) / 10000;
}

export interface JobRecord {
  id: string;
  started_at: number;
  duration_secs?: number;
  repo?: string | null;
  result?: string;
  electricity_usd?: number;
  self_hosted_fee_usd?: number;
  saved_vs_github_usd?: number | null;
  saved_vs_depot_usd?: number | null;
  [key: string]: unknown;
}

/** Checks one job record from a report; returns why it was rejected, or null. */
export function invalidRecord(r: unknown): string | null {
  if (!r || typeof r !== "object") return "not an object";
  const rec = r as JobRecord;
  if (typeof rec.id !== "string" || rec.id.length === 0 || rec.id.length > 200) return "bad id";
  if (typeof rec.started_at !== "number" || !Number.isFinite(rec.started_at)) return "bad started_at";
  if (JSON.stringify(rec).length > MAX_RECORD_BYTES) return "too large";
  return null;
}

const INVITE_ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

export function inviteCode(bytes: Uint8Array): string {
  const chars = [...bytes.slice(0, 8)].map((b) => INVITE_ALPHABET[b % INVITE_ALPHABET.length]);
  return `${chars.slice(0, 4).join("")}-${chars.slice(4).join("")}`;
}
